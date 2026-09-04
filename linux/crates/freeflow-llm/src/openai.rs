//! OpenAI-compatible chat-completions backend.
//!
//! Ported from `Sources/PostProcessingService.swift` - the request shape at
//! `process(...)`, the primary/fallback model retry at `processWithFallback`,
//! and the rate-limit handling that feeds `LLMCooldownManager`.
//!
//! Works unchanged against Groq, Gemini's OpenAI compatibility endpoint,
//! Ollama, llama.cpp, and vLLM. Only the base URL and model differ.

use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use freeflow_core::cooldown::{rate_limit_cooldown, LlmCooldownManager};
use freeflow_core::model_config::{config_for, strip_think_tags};
use serde_json::{json, Value};

use crate::{BackendHealth, LlmBackend, LlmError, LlmRequest, LlmResponse};

pub struct OpenAiCompatBackend {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    fallback_model: Option<String>,
    cooldowns: Mutex<LlmCooldownManager>,
}

impl OpenAiCompatBackend {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        fallback_model: Option<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model: model.into(),
            fallback_model,
            cooldowns: Mutex::new(LlmCooldownManager::in_memory()),
        }
    }

    /// Persists daily rate-limit cooldowns so they survive a daemon restart.
    pub fn with_cooldown_file(mut self, path: std::path::PathBuf) -> Self {
        self.cooldowns = Mutex::new(LlmCooldownManager::with_persistence(path));
        self
    }

    fn build_payload(&self, request: &LlmRequest, model: &str) -> Value {
        let mut payload = json!({
            "model": model,
            "temperature": 0.0,
            "messages": [
                { "role": "system", "content": request.prompt.system },
                { "role": "user", "content": request.prompt.user },
            ],
        });

        let config = config_for(model);
        let map = payload.as_object_mut().expect("payload is an object");
        if let Some(max_tokens) = config.max_completion_tokens.or(request.max_output_tokens) {
            map.insert("max_completion_tokens".into(), json!(max_tokens));
        }
        if let Some(effort) = config.reasoning_effort {
            map.insert("reasoning_effort".into(), json!(effort));
        }
        if let Some(include) = config.include_reasoning {
            map.insert("include_reasoning".into(), json!(include));
        }
        payload
    }

    async fn call_model(&self, request: &LlmRequest, model: &str) -> Result<String, LlmError> {
        let id = self.id();
        let url = format!("{}/chat/completions", self.base_url);

        let mut builder = self
            .client
            .post(&url)
            .timeout(request.timeout)
            .json(&self.build_payload(request, model));
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let response = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                LlmError::Timeout {
                    backend: id.clone(),
                    waited: request.timeout,
                }
            } else {
                LlmError::Transport {
                    backend: id.clone(),
                    reason: e.to_string(),
                }
            }
        })?;

        let status = response.status();
        if status.as_u16() == 429 {
            let headers = response.headers().clone();
            let cooldown =
                rate_limit_cooldown(|name| headers.get(name).and_then(|v| v.to_str().ok()));
            return Err(LlmError::RateLimited {
                backend: id,
                retry_after: cooldown.duration,
                is_daily: cooldown.is_daily,
            });
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Transport {
                backend: id,
                reason: format!(
                    "HTTP {status}: {}",
                    body.chars().take(300).collect::<String>()
                ),
            });
        }

        let body: Value = response.json().await.map_err(|e| LlmError::BadResponse {
            backend: id.clone(),
            reason: e.to_string(),
        })?;

        let content = body
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| LlmError::BadResponse {
                backend: id,
                reason: "response has no choices[0].message.content".into(),
            })?;

        // Some reasoning models leak their scratchpad into the content field.
        Ok(if config_for(model).should_strip_think_tags {
            strip_think_tags(content)
        } else {
            content.to_string()
        })
    }
}

#[async_trait]
impl LlmBackend for OpenAiCompatBackend {
    fn id(&self) -> String {
        format!("openai-compat:{}", self.model)
    }

    fn health(&self) -> BackendHealth {
        // Both models cooling down means every request would be rejected, so
        // the router should skip this backend entirely rather than pay a
        // round-trip to learn that.
        let Ok(mut cooldowns) = self.cooldowns.lock() else {
            return BackendHealth::Degraded;
        };
        match cooldowns.effective_primary(&self.model, self.fallback_model.as_deref()) {
            Some(model) if model == self.model => BackendHealth::Ready,
            Some(_) => BackendHealth::Degraded,
            None => BackendHealth::Unavailable,
        }
    }

    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {
        let started = Instant::now();

        // Skip a model already known to be rate limited rather than spending a
        // round-trip discovering it again.
        let primary = {
            let mut cooldowns = self.cooldowns.lock().expect("cooldown lock poisoned");
            cooldowns.effective_primary(&self.model, self.fallback_model.as_deref())
        };
        let Some(primary) = primary else {
            return Err(LlmError::Unavailable {
                backend: self.id(),
                reason: "all configured models are rate limited".into(),
            });
        };

        let retry_model = self
            .fallback_model
            .as_ref()
            .filter(|m| **m != primary)
            .cloned();

        let mut last_error = match self.call_model(&request, &primary).await {
            Ok(text) => {
                return Ok(LlmResponse {
                    text,
                    backend_id: self.id(),
                    model: Some(primary),
                    latency: started.elapsed(),
                })
            }
            Err(e) => e,
        };

        if let LlmError::RateLimited {
            retry_after,
            is_daily,
            ..
        } = &last_error
        {
            let mut cooldowns = self.cooldowns.lock().expect("cooldown lock poisoned");
            cooldowns.set_cooldown(
                &primary,
                freeflow_core::cooldown::RateLimitCooldown {
                    duration: *retry_after,
                    is_daily: *is_daily,
                },
            );
        }

        if let Some(retry_model) = retry_model {
            tracing::warn!(
                primary = %primary,
                fallback = %retry_model,
                error = %last_error,
                "primary model failed, retrying with fallback"
            );
            match self.call_model(&request, &retry_model).await {
                Ok(text) => {
                    return Ok(LlmResponse {
                        text,
                        backend_id: self.id(),
                        model: Some(retry_model),
                        latency: started.elapsed(),
                    })
                }
                Err(e) => last_error = e,
            }
        }

        Err(last_error)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use freeflow_core::prompt::Prompt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn request() -> LlmRequest {
        LlmRequest::new(
            Prompt {
                system: "SYSTEM".into(),
                user: "USER".into(),
            },
            Duration::from_secs(5),
        )
    }

    /// One canned reply: status, extra headers, body.
    type CannedResponse = (u16, Vec<(&'static str, &'static str)>, String);

    /// A short-lived HTTP server that replies with canned responses in order.
    /// Returns the base URL and the raw requests it received.
    async fn serve(
        responses: Vec<CannedResponse>,
    ) -> (String, tokio::sync::oneshot::Receiver<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, headers, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 65536];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                requests.push(String::from_utf8_lossy(&buf[..n]).to_string());

                let mut head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
                    body.len()
                );
                for (name, value) in headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("Connection: close\r\n\r\n");
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.flush().await;
            }
            let _ = tx.send(requests);
        });

        (format!("http://{addr}"), rx)
    }

    /// Splits a raw HTTP request and returns just the body.
    fn json_body(raw: &str) -> &str {
        raw.split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or(raw)
    }

    fn ok_body(content: &str) -> String {
        json!({ "choices": [{ "message": { "content": content } }] }).to_string()
    }

    #[tokio::test]
    async fn sends_both_prompt_halves_and_returns_the_content() {
        let (url, rx) = serve(vec![(200, vec![], ok_body("Cleaned text."))]).await;
        let backend = OpenAiCompatBackend::new(url, "openai/gpt-oss-20b", None, Some("k".into()));

        let response = backend.complete(request()).await.unwrap();
        assert_eq!(response.text, "Cleaned text.");
        assert_eq!(response.model.as_deref(), Some("openai/gpt-oss-20b"));

        let sent = rx.await.unwrap();
        let raw = &sent[0];
        // hyper emits header names lowercased on the wire.
        assert!(raw.to_lowercase().contains("authorization: bearer k"));

        // Assert on the parsed payload rather than the serialized text, since
        // serde_json orders object keys alphabetically.
        let body: Value = serde_json::from_str(json_body(raw)).unwrap();
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "SYSTEM");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "USER");
        assert_eq!(body["temperature"], 0.0);
        // gpt-oss-20b carries model-specific tuning that must reach the wire.
        assert_eq!(body["max_completion_tokens"], 4096);
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["include_reasoning"], false);
    }

    #[tokio::test]
    async fn falls_back_to_the_secondary_model_on_error() {
        let (url, rx) = serve(vec![
            (500, vec![], "upstream exploded".into()),
            (200, vec![], ok_body("Second model output.")),
        ])
        .await;
        let backend = OpenAiCompatBackend::new(
            url,
            "openai/gpt-oss-20b",
            Some("qwen/qwen3.6-27b".into()),
            None,
        );

        let response = backend.complete(request()).await.unwrap();
        assert_eq!(response.text, "Second model output.");
        assert_eq!(response.model.as_deref(), Some("qwen/qwen3.6-27b"));

        let sent = rx.await.unwrap();
        assert_eq!(sent.len(), 2, "the fallback model should have been tried");
        let retried: Value = serde_json::from_str(json_body(&sent[1])).unwrap();
        assert_eq!(retried["model"], "qwen/qwen3.6-27b");
    }

    #[tokio::test]
    async fn rate_limit_headers_set_a_cooldown_and_mark_the_model_unusable() {
        let (url, _rx) = serve(vec![(
            429,
            vec![
                ("x-ratelimit-remaining-requests", "0"),
                ("x-ratelimit-reset-requests", "2m59.56s"),
            ],
            "{}".into(),
        )])
        .await;
        let backend = OpenAiCompatBackend::new(url, "openai/gpt-oss-20b", None, None);

        assert_eq!(backend.health(), BackendHealth::Ready);
        let err = backend.complete(request()).await.unwrap_err();
        assert!(
            matches!(err, LlmError::RateLimited { is_daily: true, .. }),
            "got {err:?}"
        );
        // With no fallback model configured the backend is now unusable, and
        // the router must skip it instead of paying another round-trip.
        assert_eq!(backend.health(), BackendHealth::Unavailable);
    }

    #[tokio::test]
    async fn think_tags_are_stripped_for_models_that_emit_them() {
        let (url, _rx) = serve(vec![(
            200,
            vec![],
            ok_body("<think>deliberating</think>Final answer."),
        )])
        .await;
        let backend = OpenAiCompatBackend::new(url, "qwen/qwen3.6-27b", None, None);
        let response = backend.complete(request()).await.unwrap();
        assert_eq!(response.text, "Final answer.");
    }

    #[tokio::test]
    async fn a_response_without_content_is_an_error_not_empty_text() {
        let (url, _rx) = serve(vec![(200, vec![], json!({ "choices": [] }).to_string())]).await;
        let backend = OpenAiCompatBackend::new(url, "some/model", None, None);
        let err = backend.complete(request()).await.unwrap_err();
        assert!(matches!(err, LlmError::BadResponse { .. }), "got {err:?}");
    }
}
