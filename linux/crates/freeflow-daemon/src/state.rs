//! Daemon state: what is recording, what backends are configured, and the
//! shortcut session it all hangs off.

use std::sync::Arc;
use std::time::Duration;

use freeflow_config::{paths, Config, LlmBackendKind};
use freeflow_core::pipeline::VoiceMacro;
use freeflow_core::proto::{DaemonState, DoctorCheck};
use freeflow_core::shortcut::{
    DictationShortcutAction, DictationShortcutSessionController, RecordingTriggerMode,
    ShortcutEvent,
};
use freeflow_llm::{CliHarnessBackend, LlmBackend, OpenAiCompatBackend, RoutingBackend};
use freeflow_platform::{audio, inject};
use tokio::sync::Mutex;

use crate::pipeline::{self, SessionContext};

/// Live recording plus the context captured when it started.
struct ActiveRecording {
    recording: audio::Recording,
    session: SessionContext,
}

pub struct Daemon {
    config: Config,
    asr: freeflow_asr::TranscriptionClient,
    llm: Arc<dyn LlmBackend>,
    macros: Vec<VoiceMacro>,
    session: Mutex<DictationShortcutSessionController>,
    active: Mutex<Option<ActiveRecording>>,
    state: Mutex<DaemonState>,
    /// The last injected text, so `copy-again` can re-deliver a result that
    /// landed in the wrong window.
    last_result: Mutex<Option<String>>,
}

/// Builds the backend chain from configuration.
///
/// The order is primary then fallback; the router skips anything reporting
/// itself unusable. An empty chain is legitimate - it means "never call a
/// model", and the pipeline then pastes the raw transcript.
fn build_llm(config: &Config) -> anyhow::Result<Arc<dyn LlmBackend>> {
    fn one(kind: LlmBackendKind, config: &Config) -> anyhow::Result<Option<Box<dyn LlmBackend>>> {
        Ok(match kind {
            LlmBackendKind::None => None,
            LlmBackendKind::CliHarness => Some(Box::new(CliHarnessBackend::new(
                config.llm.cli_harness.clone(),
            ))),
            LlmBackendKind::OpenAiCompat => {
                let settings = &config.llm.openai_compat;
                let api_key = match &settings.api_key_file {
                    Some(path) => Some(freeflow_config::read_secret_file(&paths::expand_tilde(
                        path,
                    ))?),
                    None => None,
                };
                Some(Box::new(
                    OpenAiCompatBackend::new(
                        &settings.base_url,
                        &settings.model,
                        settings.fallback_model.clone(),
                        api_key,
                    )
                    .with_cooldown_file(paths::cooldown_file()?),
                ))
            }
        })
    }

    let mut backends = Vec::new();
    if let Some(primary) = one(config.llm.backend, config)? {
        backends.push(primary);
    }
    if let Some(kind) = config.llm.fallback_backend {
        if kind != config.llm.backend {
            if let Some(fallback) = one(kind, config)? {
                backends.push(fallback);
            }
        }
    }
    Ok(Arc::new(RoutingBackend::new(backends)))
}

impl Daemon {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let transcription = &config.transcription;
        let api_key = match &transcription.api_key_file {
            Some(path) => Some(freeflow_config::read_secret_file(&paths::expand_tilde(
                path,
            ))?),
            None => None,
        };
        let asr =
            freeflow_asr::TranscriptionClient::new(&transcription.base_url, &transcription.model)
                .with_api_key(api_key)
                .with_language(transcription.language.clone())
                .with_timeout(Duration::from_secs(transcription.timeout_seconds.max(5)));

        Ok(Self {
            llm: build_llm(&config)?,
            asr,
            macros: Vec::new(),
            config,
            session: Mutex::new(DictationShortcutSessionController::new()),
            active: Mutex::new(None),
            state: Mutex::new(DaemonState::Idle),
            last_result: Mutex::new(None),
        })
    }

    pub async fn state(&self) -> DaemonState {
        *self.state.lock().await
    }

    async fn set_state(&self, state: DaemonState) {
        *self.state.lock().await = state;
    }

    /// Input level while recording, for the overlay's meter.
    pub async fn level(&self) -> Option<f32> {
        self.active
            .lock()
            .await
            .as_ref()
            .map(|a| a.recording.level())
    }

    fn is_busy(state: DaemonState) -> bool {
        matches!(
            state,
            DaemonState::Transcribing | DaemonState::Cleaning | DaemonState::Injecting
        )
    }

    /// Feeds one compositor-dispatched shortcut edge into the session machine.
    pub async fn handle_shortcut(self: &Arc<Self>, event: ShortcutEvent) -> anyhow::Result<()> {
        let busy = Self::is_busy(self.state().await);
        let action = self.session.lock().await.handle(event, busy);

        match action {
            Some(DictationShortcutAction::Start(mode)) => self.start_recording(mode).await,
            Some(DictationShortcutAction::Stop) => self.stop_and_process().await,
            Some(DictationShortcutAction::SwitchedToToggle) => {
                tracing::info!("hold latched into toggle mode");
                Ok(())
            }
            None => Ok(()),
        }
    }

    async fn start_recording(self: &Arc<Self>, mode: RecordingTriggerMode) -> anyhow::Result<()> {
        // Capture context before anything can steal focus or clear the
        // selection.
        let session = SessionContext::capture(self.config.dictation.edit_mode_enabled);

        // Start the backend bootstrapping now. For the CLI harness this is the
        // difference between ~1.2s and ~5.8s: the work happens while the user
        // is still speaking.
        self.llm.prewarm();

        let recording = audio::start(
            self.config.audio.device.as_deref(),
            self.config.audio.max_recording_seconds,
        )?;

        tracing::info!(?mode, class = ?session.window.class, "recording");
        *self.active.lock().await = Some(ActiveRecording { recording, session });
        self.set_state(DaemonState::Recording).await;

        // A missed hotkey release must not leave the microphone open forever.
        let daemon = Arc::clone(self);
        let limit = self.config.audio.max_recording_seconds;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(limit + 1)).await;
            let still_recording = daemon.active.lock().await.is_some();
            if still_recording && daemon.state().await == DaemonState::Recording {
                tracing::warn!(limit, "recording hit its limit; stopping");
                daemon.session.lock().await.reset();
                let _ = daemon.stop_and_process().await;
            }
        });
        Ok(())
    }

    async fn stop_and_process(self: &Arc<Self>) -> anyhow::Result<()> {
        let Some(active) = self.active.lock().await.take() else {
            return Ok(());
        };
        let ActiveRecording { recording, session } = active;
        let wav = recording.stop()?;

        let daemon = Arc::clone(self);
        // Run off the request path so the CLI's `bindri` dispatch returns
        // immediately; Hyprland execs it synchronously.
        tokio::spawn(async move {
            let result = pipeline::run(
                wav,
                session,
                pipeline::Services {
                    config: &daemon.config,
                    asr: &daemon.asr,
                    llm: &daemon.llm,
                    macros: &daemon.macros,
                },
                true,
                |state| {
                    let daemon = Arc::clone(&daemon);
                    tokio::spawn(async move { daemon.set_state(state).await });
                },
            )
            .await;

            match result {
                Ok(result) => {
                    tracing::info!(
                        outcome = ?result.outcome,
                        chars = result.text.chars().count(),
                        transcription_ms = result.timings.transcription.as_millis(),
                        cleanup_ms = result.timings.cleanup.as_millis(),
                        injection_ms = result.timings.injection.as_millis(),
                        "dictation complete"
                    );
                    if !result.text.is_empty() {
                        *daemon.last_result.lock().await = Some(result.text);
                    }
                    daemon.set_state(DaemonState::Idle).await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "dictation failed");
                    daemon.set_state(DaemonState::Error).await;
                }
            }
        });
        Ok(())
    }

    /// Re-injects the last result.
    pub async fn copy_again(&self) -> anyhow::Result<()> {
        let Some(text) = self.last_result.lock().await.clone() else {
            return Ok(());
        };
        // No stored window context: the point of this command is to deliver
        // into wherever the user is focused *now*.
        pipeline::inject_text(&text, &SessionContext::default(), &self.config)?;
        Ok(())
    }

    /// Runs the pipeline against a WAV file instead of the microphone.
    ///
    /// Everything downstream of capture is exercised for real: the same ASR
    /// endpoint, the same backend chain, the same prompts.
    pub async fn bench(
        &self,
        wav_path: &std::path::Path,
        inject: bool,
    ) -> anyhow::Result<(String, crate::pipeline::Timings)> {
        let wav = std::fs::read(wav_path)?;
        let session = SessionContext::capture(false);
        let result = crate::pipeline::run(
            wav,
            session,
            crate::pipeline::Services {
                config: &self.config,
                asr: &self.asr,
                llm: &self.llm,
                macros: &self.macros,
            },
            inject,
            |_| {},
        )
        .await?;
        Ok((result.text, result.timings))
    }

    /// Abandons the current recording without injecting.
    pub async fn cancel(&self) -> anyhow::Result<()> {
        self.session.lock().await.reset();
        // Dropping the recording stops capture; the audio is discarded.
        self.active.lock().await.take();
        self.set_state(DaemonState::Idle).await;
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.llm.shutdown().await;
    }

    /// Environment checks, so a broken setup reports itself instead of
    /// silently doing nothing when the hotkey is pressed.
    pub async fn doctor(&self) -> Vec<DoctorCheck> {
        let mut checks = Vec::new();

        let devices = audio::list_input_devices();
        checks.push(match &devices {
            Ok(list) if !list.is_empty() => DoctorCheck {
                name: "microphone".into(),
                ok: true,
                detail: format!(
                    "{} input devices; default is {:?}",
                    list.len(),
                    list.iter().find(|d| d.is_default).map(|d| &d.id)
                ),
            },
            Ok(_) => DoctorCheck {
                name: "microphone".into(),
                ok: false,
                detail: "no input devices found".into(),
            },
            Err(e) => DoctorCheck {
                name: "microphone".into(),
                ok: false,
                detail: e.to_string(),
            },
        });

        checks.push(DoctorCheck {
            name: "wtype".into(),
            ok: inject::wtype_available(),
            detail: "types text into the focused window".into(),
        });

        let window = freeflow_platform::context::active_window();
        checks.push(DoctorCheck {
            name: "hyprland".into(),
            ok: window.class.is_some() || window.address.is_some(),
            detail: match &window.class {
                Some(class) => format!("focused window class {class:?}"),
                None => "hyprctl returned no focused window".into(),
            },
        });

        let health = self.llm.health();
        checks.push(DoctorCheck {
            name: "llm".into(),
            ok: health.is_usable(),
            detail: format!("{} is {health:?}", self.llm.id()),
        });

        checks.push(self.transcription_check().await);
        checks
    }

    async fn transcription_check(&self) -> DoctorCheck {
        let url = format!(
            "{}/health",
            self.config.transcription.base_url.trim_end_matches('/')
        );
        let reachable = match reqwest_get(&url).await {
            Ok(status) => status,
            Err(e) => {
                return DoctorCheck {
                    name: "transcription".into(),
                    ok: false,
                    detail: format!("{url} unreachable: {e}"),
                }
            }
        };
        DoctorCheck {
            name: "transcription".into(),
            ok: reachable,
            detail: url,
        }
    }
}

/// Minimal reachability probe. Uses the ASR crate's transport indirectly by
/// spawning a short request rather than pulling reqwest into this crate's
/// public surface.
async fn reqwest_get(url: &str) -> Result<bool, String> {
    let output = tokio::process::Command::new("curl")
        .args([
            "-fsS",
            "-m",
            "3",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            url,
        ])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let code = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(code.starts_with('2'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(backend: LlmBackendKind, fallback: Option<LlmBackendKind>) -> Config {
        let mut config = Config::default();
        config.llm.backend = backend;
        config.llm.fallback_backend = fallback;
        // Avoid touching a real key file during construction.
        config.llm.openai_compat.api_key_file = None;
        config
    }

    #[test]
    fn the_backend_chain_follows_configuration() {
        let router = build_llm(&config_with(
            LlmBackendKind::CliHarness,
            Some(LlmBackendKind::OpenAiCompat),
        ))
        .unwrap();
        let id = router.id();
        assert!(id.contains("cli-harness:claude"), "{id}");
        assert!(id.contains("openai-compat:"), "{id}");
        // Order matters: the primary must be tried first.
        assert!(
            id.find("cli-harness").unwrap() < id.find("openai-compat").unwrap(),
            "{id}"
        );
    }

    #[test]
    fn a_fallback_equal_to_the_primary_is_not_added_twice() {
        let router = build_llm(&config_with(
            LlmBackendKind::OpenAiCompat,
            Some(LlmBackendKind::OpenAiCompat),
        ))
        .unwrap();
        assert_eq!(router.id().matches("openai-compat:").count(), 1);
    }

    // `backend = "none"` is a supported choice: it means never call a model,
    // and the pipeline then pastes the raw transcript. It must not be an error.
    #[test]
    fn a_disabled_backend_yields_an_empty_chain_rather_than_failing() {
        let router = build_llm(&config_with(LlmBackendKind::None, None)).unwrap();
        assert_eq!(router.id(), "router[]");
        assert!(!router.health().is_usable());
    }
}
