//! Backend routing: try the primary, fall back, never lose the user's words.

use async_trait::async_trait;

use crate::{BackendHealth, LlmBackend, LlmError, LlmRequest, LlmResponse};

/// Tries backends in order, skipping any that report themselves unusable.
///
/// The daemon treats an exhausted router as "paste the raw transcript", so a
/// total failure here degrades the result rather than losing it.
pub struct RoutingBackend {
    backends: Vec<Box<dyn LlmBackend>>,
}

impl RoutingBackend {
    pub fn new(backends: Vec<Box<dyn LlmBackend>>) -> Self {
        Self { backends }
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }
}

#[async_trait]
impl LlmBackend for RoutingBackend {
    fn id(&self) -> String {
        format!(
            "router[{}]",
            self.backends
                .iter()
                .map(|b| b.id())
                .collect::<Vec<_>>()
                .join(",")
        )
    }

    fn health(&self) -> BackendHealth {
        self.backends
            .iter()
            .map(|b| b.health())
            .min_by_key(|h| match h {
                BackendHealth::Ready => 0,
                BackendHealth::Warming => 1,
                BackendHealth::Degraded => 2,
                BackendHealth::Unavailable => 3,
            })
            .unwrap_or(BackendHealth::Unavailable)
    }

    fn prewarm(&self) {
        // Warm every backend: the one that ends up serving the request may not
        // be the first, and prewarming is cheap by contract.
        for backend in &self.backends {
            backend.prewarm();
        }
    }

    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {
        let mut last_error: Option<LlmError> = None;

        for backend in &self.backends {
            if !backend.health().is_usable() {
                tracing::debug!(backend = %backend.id(), "skipping unusable backend");
                continue;
            }
            match backend.complete(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    tracing::warn!(backend = %backend.id(), error = %e, "backend failed, trying next");
                    let should_fall_back = e.should_fall_back();
                    last_error = Some(e);
                    if !should_fall_back {
                        break;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| LlmError::Unavailable {
            backend: self.id(),
            reason: "no usable backend is configured".into(),
        }))
    }

    async fn shutdown(&self) {
        for backend in &self.backends {
            backend.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use freeflow_core::prompt::Prompt;

    use super::*;

    struct Stub {
        name: &'static str,
        health: BackendHealth,
        result: Option<&'static str>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LlmBackend for Stub {
        fn id(&self) -> String {
            self.name.to_string()
        }
        fn health(&self) -> BackendHealth {
            self.health
        }
        async fn complete(&self, _request: LlmRequest) -> Result<LlmResponse, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.result {
                Some(text) => Ok(LlmResponse {
                    text: text.into(),
                    backend_id: self.name.into(),
                    model: None,
                    latency: Duration::ZERO,
                }),
                None => Err(LlmError::Transport {
                    backend: self.name.into(),
                    reason: "stub failure".into(),
                }),
            }
        }
    }

    fn stub(
        name: &'static str,
        health: BackendHealth,
        result: Option<&'static str>,
    ) -> (Box<dyn LlmBackend>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Box::new(Stub {
                name,
                health,
                result,
                calls: calls.clone(),
            }),
            calls,
        )
    }

    fn request() -> LlmRequest {
        LlmRequest::new(
            Prompt {
                system: "S".into(),
                user: "U".into(),
            },
            Duration::from_secs(1),
        )
    }

    #[tokio::test]
    async fn uses_the_first_healthy_backend() {
        let (primary, primary_calls) = stub("primary", BackendHealth::Ready, Some("from primary"));
        let (fallback, fallback_calls) =
            stub("fallback", BackendHealth::Ready, Some("from fallback"));
        let router = RoutingBackend::new(vec![primary, fallback]);

        let response = router.complete(request()).await.unwrap();
        assert_eq!(response.text, "from primary");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "the fallback should not have been called"
        );
    }

    #[tokio::test]
    async fn falls_through_when_the_primary_fails() {
        let (primary, _) = stub("primary", BackendHealth::Ready, None);
        let (fallback, fallback_calls) =
            stub("fallback", BackendHealth::Ready, Some("from fallback"));
        let router = RoutingBackend::new(vec![primary, fallback]);

        let response = router.complete(request()).await.unwrap();
        assert_eq!(response.text, "from fallback");
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unavailable_backend_is_skipped_without_being_called() {
        let (primary, primary_calls) =
            stub("primary", BackendHealth::Unavailable, Some("unreachable"));
        let (fallback, _) = stub("fallback", BackendHealth::Ready, Some("from fallback"));
        let router = RoutingBackend::new(vec![primary, fallback]);

        let response = router.complete(request()).await.unwrap();
        assert_eq!(response.text, "from fallback");
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            0,
            "an unavailable backend must not be sent a request"
        );
    }

    #[tokio::test]
    async fn exhausting_every_backend_reports_the_last_error() {
        let (primary, _) = stub("primary", BackendHealth::Ready, None);
        let (fallback, _) = stub("fallback", BackendHealth::Ready, None);
        let router = RoutingBackend::new(vec![primary, fallback]);
        assert!(router.complete(request()).await.is_err());
    }

    #[tokio::test]
    async fn an_empty_router_errors_rather_than_hanging() {
        let router = RoutingBackend::new(vec![]);
        assert!(router.is_empty());
        assert_eq!(router.health(), BackendHealth::Unavailable);
        assert!(router.complete(request()).await.is_err());
    }
}
