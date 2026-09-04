//! Speech-to-text over an OpenAI-compatible transcription endpoint.
//!
//! Ported from `Sources/TranscriptionService.swift`. Deliberately has exactly
//! one code path for local and hosted providers: they differ only in base URL.
//!
//! Local transcription runs `whisper-server` from whisper.cpp, which serves
//! `/inference` by default but takes `--request-path` and `--inference-path`,
//! so starting it as
//!
//! ```text
//! whisper-server --request-path /v1/audio --inference-path /transcriptions
//! ```
//!
//! makes it answer on `POST /v1/audio/transcriptions` with OpenAI's own
//! multipart field names. Nothing here needs to know which one it is talking
//! to. Note that `--convert` must stay off: it shells out to ffmpeg, and the
//! recorder already produces exactly the 16 kHz mono PCM16 WAV the server
//! wants.

use std::time::Duration;

use freeflow_core::transcript::{parse_transcription_response, TranscriptionResponseParsingError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TranscriptionError {
    #[error("transcription timed out after {waited:?}")]
    Timeout { waited: Duration },
    #[error("could not reach the transcription service: {0}")]
    Transport(String),
    #[error("transcription service returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("transcription service returned an unusable response")]
    BadResponse(#[from] TranscriptionResponseParsingError),
    #[error("could not read the recording: {0}")]
    Audio(String),
}

/// Models that accept `verbose_json`, which carries the per-segment
/// `no_speech_prob` the hallucination filter needs. Anything else gets plain
/// `json`, and the filter then declines to suppress rather than guess.
const MODELS_SUPPORTING_VERBOSE_JSON: &[&str] =
    &["whisper-large-v3", "whisper-large-v3-turbo", "whisper-1"];

pub struct TranscriptionClient {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    /// ISO-639-1 hint. Empty means let the provider auto-detect.
    language: String,
    timeout: Duration,
}

impl TranscriptionClient {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key: None,
            language: String::new(),
            timeout: Duration::from_secs(60),
        }
    }

    pub fn with_api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key;
        self
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = language.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `GET {base}/health` for the local server, used as a readiness gate so
    /// the daemon can say "warming up" instead of failing while a model loads.
    pub async fn health_url(&self) -> String {
        format!("{}/health", self.base_url)
    }

    fn response_format(&self) -> &'static str {
        if MODELS_SUPPORTING_VERBOSE_JSON.contains(&self.model.as_str()) {
            "verbose_json"
        } else {
            "json"
        }
    }

    /// Transcribes a WAV file already on disk.
    pub async fn transcribe_file(
        &self,
        path: &std::path::Path,
    ) -> Result<String, TranscriptionError> {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| TranscriptionError::Audio(format!("{}: {e}", path.display())))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "audio.wav".to_string());
        self.transcribe_bytes(bytes, name).await
    }

    pub async fn transcribe_bytes(
        &self,
        bytes: Vec<u8>,
        file_name: String,
    ) -> Result<String, TranscriptionError> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str("audio/wav")
            .map_err(|e| TranscriptionError::Transport(e.to_string()))?;

        let mut form = reqwest::multipart::Form::new()
            .text("model", self.model.clone())
            .text("response_format", self.response_format())
            .part("file", part);
        if !self.language.trim().is_empty() {
            form = form.text("language", self.language.trim().to_string());
        }

        let url = format!("{}/transcriptions", self.base_url);
        let mut builder = self.client.post(&url).timeout(self.timeout).multipart(form);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let response = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                TranscriptionError::Timeout {
                    waited: self.timeout,
                }
            } else {
                TranscriptionError::Transport(e.to_string())
            }
        })?;

        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| TranscriptionError::Transport(e.to_string()))?;

        if !status.is_success() {
            return Err(TranscriptionError::Http {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&body).chars().take(300).collect(),
            });
        }

        // Hallucination filtering and the non-JSON fallback both live in
        // freeflow-core so they stay unit tested without a network.
        Ok(parse_transcription_response(&body)?)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    async fn serve(status: u16, body: String) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 65536];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());

            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.flush().await;
        });

        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn posts_openai_shaped_multipart_and_returns_the_text() {
        let (url, rx) = serve(200, r#"{"text":"Synthetic transcript."}"#.into()).await;
        let client = TranscriptionClient::new(url, "whisper-large-v3-turbo")
            .with_api_key(Some("secret".into()));

        let text = client
            .transcribe_bytes(b"RIFFfake".to_vec(), "audio.wav".into())
            .await
            .unwrap();
        assert_eq!(text, "Synthetic transcript.");

        let sent = rx.await.unwrap();
        assert!(sent.starts_with("POST /transcriptions "));
        assert!(sent.to_lowercase().contains("authorization: bearer secret"));
        assert!(sent.contains("name=\"model\""));
        assert!(sent.contains("whisper-large-v3-turbo"));
        assert!(sent.contains("name=\"file\"; filename=\"audio.wav\""));
        // Whisper models must request verbose_json so segment metadata is
        // available to the hallucination filter.
        assert!(sent.contains("verbose_json"));
        // No language field unless one was configured.
        assert!(!sent.contains("name=\"language\""));
    }

    #[tokio::test]
    async fn non_whisper_models_request_plain_json() {
        let (url, rx) = serve(200, r#"{"text":"ok"}"#.into()).await;
        let client = TranscriptionClient::new(url, "some/other-asr").with_language("  en  ");
        client
            .transcribe_bytes(b"RIFFfake".to_vec(), "a.wav".into())
            .await
            .unwrap();

        let sent = rx.await.unwrap();
        assert!(sent.contains("name=\"response_format\""));
        assert!(!sent.contains("verbose_json"));
        assert!(sent.contains("name=\"language\""));
        assert!(sent.contains("en"));
    }

    #[tokio::test]
    async fn hallucinated_silence_is_filtered_out() {
        // The filter lives in freeflow-core; this asserts the client actually
        // routes responses through it rather than returning raw text.
        let (url, _rx) = serve(
            200,
            r#"{"text":"Thank you.","segments":[{"no_speech_prob":0.9}]}"#.into(),
        )
        .await;
        let client = TranscriptionClient::new(url, "whisper-large-v3-turbo");
        let text = client
            .transcribe_bytes(b"RIFFfake".to_vec(), "a.wav".into())
            .await
            .unwrap();
        assert_eq!(text, "");
    }

    #[tokio::test]
    async fn http_errors_carry_the_status_and_body() {
        let (url, _rx) = serve(503, "model still loading".into()).await;
        let client = TranscriptionClient::new(url, "whisper-large-v3-turbo");
        let err = client
            .transcribe_bytes(b"RIFFfake".to_vec(), "a.wav".into())
            .await
            .unwrap_err();
        match err {
            TranscriptionError::Http { status, body } => {
                assert_eq!(status, 503);
                assert!(body.contains("model still loading"));
            }
            other => panic!("expected an HTTP error, got {other:?}"),
        }
    }
}
