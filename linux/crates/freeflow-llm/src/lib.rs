//! Text-transformation backends for the dictation pipeline.
//!
//! The pipeline needs exactly one capability - turn a [`Prompt`] into text -
//! and this crate provides several interchangeable ways to do it: an
//! OpenAI-compatible HTTP endpoint (Groq, Gemini's compatibility layer,
//! Ollama, llama.cpp), or a locally installed agent CLI driven over its
//! streaming protocol.
//!
//! Which one runs is a configuration choice, never a code change. That is the
//! point of the [`LlmBackend`] trait.

pub mod harness;
pub mod openai;
pub mod router;

use std::time::Duration;

use async_trait::async_trait;
use freeflow_core::prompt::Prompt;
use thiserror::Error;

pub use harness::CliHarnessBackend;
pub use openai::OpenAiCompatBackend;
pub use router::RoutingBackend;

/// One text transformation. Self-contained by construction: a backend is
/// forbidden from carrying conversational state between calls, which is what
/// keeps one dictation's transcript out of the next one's context.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub prompt: Prompt,
    pub max_output_tokens: Option<u32>,
    pub timeout: Duration,
}

impl LlmRequest {
    pub fn new(prompt: Prompt, timeout: Duration) -> Self {
        Self {
            prompt,
            max_output_tokens: None,
            timeout,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: String,
    /// Which backend produced this, for the history view and logs.
    pub backend_id: String,
    pub model: Option<String>,
    pub latency: Duration,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("{backend} timed out after {waited:?}")]
    Timeout { backend: String, waited: Duration },
    /// The backend cannot serve requests at all right now - not installed, not
    /// authenticated, circuit breaker open. The router should skip it.
    #[error("{backend} is unavailable: {reason}")]
    Unavailable { backend: String, reason: String },
    /// A transient failure; retrying or falling back may succeed.
    #[error("{backend} transport error: {reason}")]
    Transport { backend: String, reason: String },
    /// The provider answered, but not with usable text.
    #[error("{backend} returned an unusable response: {reason}")]
    BadResponse { backend: String, reason: String },
    #[error("{backend} is rate limited; retry in {retry_after:?}")]
    RateLimited {
        backend: String,
        retry_after: Duration,
        /// A daily quota rather than a per-minute one, so it should persist.
        is_daily: bool,
    },
}

impl LlmError {
    /// Whether the router should try the next backend rather than give up.
    pub fn should_fall_back(&self) -> bool {
        // Every failure mode is worth falling back on. The alternative is
        // losing the user's cleanup for a reason they cannot act on.
        true
    }
}

/// Whether a backend is worth sending a request to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendHealth {
    Ready,
    /// Usable, but the next request will be slower than usual.
    Warming,
    /// Reachable but failing; the router prefers alternatives.
    Degraded,
    Unavailable,
}

impl BackendHealth {
    pub fn is_usable(self) -> bool {
        !matches!(self, BackendHealth::Unavailable)
    }
}

#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// Stable identifier used in logs, history rows, and error messages.
    fn id(&self) -> String;

    fn health(&self) -> BackendHealth;

    /// Called on hotkey *press*, before the user has finished speaking.
    ///
    /// Backends with expensive startup use this to do that work while the
    /// user is still talking, which hides it completely rather than merely
    /// amortizing it. Must be cheap to call and safe to call repeatedly.
    fn prewarm(&self) {}

    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError>;

    /// Release any held resources. Called on daemon shutdown so pooled child
    /// processes do not outlive it.
    async fn shutdown(&self) {}
}
