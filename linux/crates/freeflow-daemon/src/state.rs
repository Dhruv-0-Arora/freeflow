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
use freeflow_platform::{audio, inject, keywatch};
use tokio::sync::{watch, Mutex};

use crate::history::{History, NewEntry};
use crate::pipeline::{self, SessionContext};

/// How long a terminal `Error` stays visible before the daemon returns to
/// `Idle` on its own.
const ERROR_LINGER: Duration = Duration::from_secs(4);

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
    /// A watch channel rather than a mutex so writes are synchronous: every
    /// state transition lands in program order, and the terminal Idle/Error
    /// write is guaranteed to be the last one. A spawned write here once left
    /// the daemon reporting "Pasting" forever after a successful dictation.
    state: watch::Sender<DaemonState>,
    /// How long a terminal `Error` lingers before clearing back to `Idle`.
    /// The overlay only hides on `Idle`, so an uncleared error would sit on
    /// screen until the next dictation.
    error_linger: Duration,
    /// Watches `/dev/input` for the session key's release while a session is
    /// active. `None` the rest of the time, so input devices are only read
    /// between the starting press and the ending release.
    watcher: std::sync::Mutex<Option<keywatch::KeyWatcher>>,
    /// The last injected text, so `copy-again` can re-deliver a result that
    /// landed in the wrong window.
    last_result: Mutex<Option<String>>,
    /// `None` when `dictation.history_enabled` is off, or when the store could
    /// not be opened. Either way dictation continues: history is a
    /// convenience, and failing a dictation over it would be the wrong trade.
    history: Mutex<Option<History>>,
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

        // History is a convenience. If the store cannot be opened, dictation
        // must still work, so this degrades to no history rather than failing
        // to start.
        let history = config
            .dictation
            .history_enabled
            .then(|| {
                let path = paths::history_database().ok()?;
                History::open(&path)
                    .inspect_err(|e| {
                        tracing::warn!(error = %e, "history is disabled for this session");
                    })
                    .ok()
            })
            .flatten();

        Ok(Self {
            llm: build_llm(&config)?,
            asr,
            macros: Vec::new(),
            config,
            history: Mutex::new(history),
            session: Mutex::new(DictationShortcutSessionController::new()),
            active: Mutex::new(None),
            state: watch::channel(DaemonState::Idle).0,
            error_linger: ERROR_LINGER,
            watcher: std::sync::Mutex::new(None),
            last_result: Mutex::new(None),
        })
    }

    pub fn state(&self) -> DaemonState {
        *self.state.borrow()
    }

    fn set_state(&self, state: DaemonState) {
        self.state.send_replace(state);
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
    ///
    /// `key` is the bind's main key when the edge came from a Hyprland press
    /// bind. Release edges are not bound in the compositor at all - a release
    /// bind consumes the key's press for every modifier state - so a session
    /// arms an evdev watch on its key and synthesises the release edge itself.
    pub async fn handle_shortcut(
        self: &Arc<Self>,
        event: ShortcutEvent,
        key: Option<String>,
    ) -> anyhow::Result<()> {
        let busy = Self::is_busy(self.state());
        let action = self.session.lock().await.handle(event, busy);

        match action {
            Some(DictationShortcutAction::Start(mode)) => {
                self.start_recording(mode).await?;
                if let Some(key) = key {
                    let release = match mode {
                        RecordingTriggerMode::Hold => ShortcutEvent::HoldDeactivated,
                        RecordingTriggerMode::Toggle => ShortcutEvent::ToggleDeactivated,
                    };
                    self.arm_release_watch(&key, release);
                }
                Ok(())
            }
            Some(DictationShortcutAction::Stop) => self.stop_and_process().await,
            Some(DictationShortcutAction::SwitchedToToggle) => {
                tracing::info!("hold latched into toggle mode");
                // The latch bind passes the *toggle* key: its release is what
                // arms the toggle to stop. The latch chord does not hold that
                // key, so the watch usually fires straight away - which is
                // exactly the arming the old compositor release bind provided.
                if let Some(key) = key {
                    self.arm_release_watch(&key, ShortcutEvent::ToggleDeactivated);
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Watches the session key on `/dev/input` and feeds its release back
    /// into the session machine, exactly as if a compositor bind had
    /// dispatched it.
    fn arm_release_watch(self: &Arc<Self>, key_name: &str, release: ShortcutEvent) {
        let Some(code) = freeflow_core::keymap::keycode(key_name) else {
            tracing::warn!(
                key = key_name,
                "unknown key name; release must come from `freeflow ... release` or the watchdog"
            );
            return;
        };
        let handle = tokio::runtime::Handle::current();
        let daemon = Arc::downgrade(self);
        match keywatch::watch_release(code, move || {
            let Some(daemon) = daemon.upgrade() else {
                return;
            };
            handle.spawn(async move {
                if let Err(e) = daemon.handle_shortcut(release, None).await {
                    tracing::warn!(error = %e, "release edge failed");
                }
            });
        }) {
            Ok(watcher) => *self.watcher.lock().unwrap() = Some(watcher),
            Err(e) => tracing::warn!(
                error = %e,
                "cannot watch for the key release; release must come from `freeflow ... release` or the watchdog"
            ),
        }
    }

    fn clear_release_watch(&self) {
        self.watcher.lock().unwrap().take();
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
        self.set_state(DaemonState::Recording);

        // A missed hotkey release must not leave the microphone open forever.
        let daemon = Arc::clone(self);
        let limit = self.config.audio.max_recording_seconds;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(limit + 1)).await;
            let still_recording = daemon.active.lock().await.is_some();
            if still_recording && daemon.state() == DaemonState::Recording {
                tracing::warn!(limit, "recording hit its limit; stopping");
                daemon.session.lock().await.reset();
                let _ = daemon.stop_and_process().await;
            }
        });
        Ok(())
    }

    async fn stop_and_process(self: &Arc<Self>) -> anyhow::Result<()> {
        // The session is over however we got here - key release, second tap,
        // or the watchdog - so stop reading input devices.
        self.clear_release_watch();
        let Some(active) = self.active.lock().await.take() else {
            return Ok(());
        };
        let ActiveRecording { recording, session } = active;
        let wav = recording.stop()?;

        // Mark the daemon busy before the pipeline task is even scheduled, so
        // a hotkey edge arriving in the gap cannot observe a stale state and
        // start a second recording.
        self.set_state(DaemonState::Transcribing);
        self.spawn_pipeline(wav, session, true);
        Ok(())
    }

    /// Runs the pipeline off the request path so the CLI's hotkey dispatch
    /// returns immediately; Hyprland execs it synchronously.
    ///
    /// Stage reports go through the synchronous `set_state`, which is what
    /// guarantees the terminal `Idle`/`Error` is the last write. Reporting via
    /// a spawned task once let the `Injecting` write land *after* `Idle`,
    /// leaving the daemon claiming "Pasting" forever and dropping hotkeys.
    fn spawn_pipeline(
        self: &Arc<Self>,
        wav: Vec<u8>,
        session: SessionContext,
        should_inject: bool,
    ) -> tokio::task::JoinHandle<()> {
        let daemon = Arc::clone(self);
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
                should_inject,
                |state| daemon.set_state(state),
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
                        daemon.record_history(&result).await;
                        *daemon.last_result.lock().await = Some(result.text);
                    }
                    daemon.set_state(DaemonState::Idle);
                }
                Err(e) => {
                    tracing::error!(error = %e, "dictation failed");
                    daemon.set_state(DaemonState::Error);
                    daemon.schedule_error_clear();
                }
            }
        })
    }

    /// Returns an errored daemon to `Idle` after a linger, unless something
    /// else has already moved the state on.
    fn schedule_error_clear(self: &Arc<Self>) {
        let daemon = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(daemon.error_linger).await;
            if daemon.state() == DaemonState::Error {
                daemon.set_state(DaemonState::Idle);
            }
        });
    }

    /// Writes one completed dictation to the local history store.
    async fn record_history(&self, result: &pipeline::PipelineResult) {
        let guard = self.history.lock().await;
        let Some(store) = guard.as_ref() else { return };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Err(e) = store.record(&NewEntry {
            raw_transcript: &result.raw_transcript,
            final_text: &result.text,
            outcome: &result.outcome,
            app_class: result.app_class.as_deref(),
            transcription_ms: result.timings.transcription.as_millis() as u64,
            cleanup_ms: result.timings.cleanup.as_millis() as u64,
            now,
        }) {
            tracing::warn!(error = %e, "could not write history");
        }
    }

    /// Recent dictations, newest first.
    pub async fn history(&self, limit: usize) -> Vec<freeflow_core::proto::HistoryRow> {
        let guard = self.history.lock().await;
        let Some(store) = guard.as_ref() else {
            return Vec::new();
        };
        store
            .recent(limit)
            .unwrap_or_default()
            .into_iter()
            .map(|e| freeflow_core::proto::HistoryRow {
                created_at: e.created_at,
                raw_transcript: e.raw_transcript,
                final_text: e.final_text,
                outcome: e.outcome,
                app_class: e.app_class,
                transcription_ms: e.transcription_ms,
                cleanup_ms: e.cleanup_ms,
            })
            .collect()
    }

    pub async fn clear_history(&self) -> anyhow::Result<()> {
        let guard = self.history.lock().await;
        if let Some(store) = guard.as_ref() {
            store.clear()?;
        }
        Ok(())
    }

    /// Re-injects the last result.
    pub async fn copy_again(&self) -> anyhow::Result<()> {
        let Some(text) = self.last_result.lock().await.clone() else {
            return Ok(());
        };
        // No stored window context: the point of this command is to deliver
        // into wherever the user is focused *now*. Injection shells out and
        // sleeps, so it runs off the async runtime.
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            pipeline::inject_text(&text, &SessionContext::default(), &config)
        })
        .await??;
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
        // A dry run is a measurement, not a dictation, so it stays out of
        // history. `--inject` types into a real window, which is a dictation
        // by any reasonable definition, so it is recorded like one.
        if inject && !result.text.is_empty() {
            self.record_history(&result).await;
        }
        Ok((result.text, result.timings))
    }

    /// Abandons the current recording without injecting.
    pub async fn cancel(&self) -> anyhow::Result<()> {
        self.session.lock().await.reset();
        self.clear_release_watch();
        // Dropping the recording stops capture; the audio is discarded.
        self.active.lock().await.take();
        self.set_state(DaemonState::Idle);
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

        // KEY_D stands in for "any ordinary key": if no device is readable
        // for it, none will be for the configured binds either.
        let watchable = keywatch::watchable_device_count(32);
        checks.push(DoctorCheck {
            name: "keywatch".into(),
            ok: watchable > 0,
            detail: if watchable > 0 {
                format!("{watchable} input devices readable for release detection")
            } else {
                "no readable /dev/input device; add this user to the `input` group".into()
            },
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

    /// One-request HTTP stub standing in for whisper-server.
    async fn stub_asr(status: u16, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            // Drain the whole request first; replying mid-upload can surface
            // as a transport error in the client instead of the stub status.
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let content_length = loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break end + 4 + length;
                }
            };
            while buf.len() < content_length {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.flush().await;
        });
        format!("http://{addr}")
    }

    fn test_daemon(base_url: &str) -> Daemon {
        let mut config = config_with(LlmBackendKind::None, None);
        config.transcription.base_url = base_url.to_string();
        config.transcription.api_key_file = None;
        // Keep the test off the user's real history store.
        config.dictation.history_enabled = false;
        Daemon::new(config).unwrap()
    }

    // Every stage report must land before the terminal state. When reports
    // went through `tokio::spawn`, a stage write could be scheduled after the
    // terminal `Idle`, leaving the daemon claiming "Cleaning up" or "Pasting"
    // forever and silently dropping hotkeys as busy.
    #[tokio::test]
    async fn a_completed_pipeline_ends_idle_with_no_late_state_writes() {
        let base = stub_asr(200, r#"{"text":"hello world"}"#).await;
        let daemon = Arc::new(test_daemon(&base));

        daemon
            .spawn_pipeline(vec![0u8; 32_044], SessionContext::default(), false)
            .await
            .unwrap();

        // Give any stray spawned write every chance to run before asserting.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(daemon.state(), DaemonState::Idle);
    }

    // A sub-200ms tap produces a header-only WAV. Sending it would come back
    // as HTTP 400 and surface as an error for what is just a twitch of the
    // hotkey; it must take the silent-recording path instead.
    #[tokio::test]
    async fn a_too_short_recording_skips_transcription_and_ends_idle() {
        let base = stub_asr(400, "Invalid request").await;
        let daemon = Arc::new(test_daemon(&base));

        daemon
            .spawn_pipeline(vec![0u8; 44], SessionContext::default(), false)
            .await
            .unwrap();
        assert_eq!(daemon.state(), DaemonState::Idle);
    }

    #[tokio::test]
    async fn a_failed_pipeline_reports_error_then_clears_itself() {
        let base = stub_asr(500, "boom").await;
        let mut daemon = test_daemon(&base);
        daemon.error_linger = Duration::from_millis(25);
        let daemon = Arc::new(daemon);

        daemon
            .spawn_pipeline(vec![0u8; 32_044], SessionContext::default(), false)
            .await
            .unwrap();
        assert_eq!(daemon.state(), DaemonState::Error);

        // The overlay only hides on Idle, so an error must clear on its own.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(daemon.state(), DaemonState::Idle);
    }
}
