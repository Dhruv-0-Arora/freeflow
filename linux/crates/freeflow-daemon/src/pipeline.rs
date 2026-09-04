//! The dictation pipeline: hotkey to text at the cursor.
//!
//! Replaces the orchestration in `Sources/AppState.swift` - `startRecording`,
//! `stopAndTranscribe`, `processTranscript`, and `pasteAtCursor`.
//!
//! The branching logic itself lives in `freeflow_core::pipeline` as a pure
//! function so it can be tested without a microphone or a network. What is
//! here is only the I/O ordering around it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use freeflow_config::Config;
use freeflow_core::pipeline::{self, Decision, Outcome, PipelineInput, SessionIntent, VoiceMacro};
use freeflow_core::proto::DaemonState;
use freeflow_core::transcript;
use freeflow_llm::{LlmBackend, LlmRequest};
use freeflow_platform::{clipboard, context, inject};

/// Everything captured at hotkey-down, before recording starts.
///
/// Gathered up front deliberately: by the time the transcript is ready the
/// user may have moved focus or lost their selection, and the context has to
/// describe where the dictation *began*.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub window: context::WindowContext,
    pub selected_text: String,
}

impl SessionContext {
    pub fn capture(edit_mode_enabled: bool) -> Self {
        let window = context::active_window();
        // Reading the selection costs a subprocess, so skip it when edit mode
        // is off rather than paying for it on every dictation.
        let selected_text = if edit_mode_enabled {
            clipboard::read_selected_text()
        } else {
            String::new()
        };
        Self {
            window,
            selected_text,
        }
    }

    /// Edit mode when the user had something highlighted, otherwise ordinary
    /// dictation.
    pub fn intent(&self) -> SessionIntent {
        if self.selected_text.trim().is_empty() {
            SessionIntent::Dictation
        } else {
            SessionIntent::EditMode {
                selected_text: self.selected_text.clone(),
            }
        }
    }
}

/// Per-stage timings, for the overlay and for `freeflow bench`.
#[derive(Debug, Clone, Default)]
pub struct Timings {
    pub transcription: Duration,
    pub cleanup: Duration,
    pub injection: Duration,
}

pub struct PipelineResult {
    pub text: String,
    /// Kept alongside the final text so history shows what cleanup changed,
    /// which is the only way to tell a good prompt from a bad one.
    pub raw_transcript: String,
    pub outcome: Outcome,
    pub app_class: Option<String>,
    pub timings: Timings,
}

/// Runs transcription, cleanup, and injection for one recording.
///
/// `report` is called as each stage begins so the overlay can follow along.
/// When `should_inject` is false the result is computed and returned but not
/// typed anywhere, which is how `freeflow bench` measures latency without
/// scribbling into whatever window the user is looking at.
pub struct Services<'a> {
    pub config: &'a Config,
    pub asr: &'a freeflow_asr::TranscriptionClient,
    pub llm: &'a Arc<dyn LlmBackend>,
    pub macros: &'a [VoiceMacro],
}

pub async fn run(
    wav: Vec<u8>,
    session: SessionContext,
    services: Services<'_>,
    should_inject: bool,
    mut report: impl FnMut(DaemonState),
) -> anyhow::Result<PipelineResult> {
    let Services {
        config,
        asr,
        llm,
        macros,
    } = services;
    let mut timings = Timings::default();

    report(DaemonState::Transcribing);
    let started = Instant::now();
    let raw_transcript = asr.transcribe_bytes(wav, "audio.wav".to_string()).await?;
    timings.transcription = started.elapsed();

    let vocabulary = &config.dictation.custom_vocabulary;
    let context_summary = session.window.summary();
    let input = PipelineInput {
        raw_transcript: &raw_transcript,
        intent: session.intent(),
        context_summary: &context_summary,
        vocabulary,
        custom_system_prompt: &config.dictation.custom_system_prompt,
        output_language: &config.dictation.output_language,
        preserve_exact_wording: config.dictation.preserve_exact_wording,
        macros,
    };

    let decision = pipeline::plan(&input);

    // Only the provider-backed branches make a network or subprocess call.
    let provider_output = match decision.prompt() {
        None => None,
        Some(prompt) => {
            report(DaemonState::Cleaning);
            let started = Instant::now();
            let request = LlmRequest::new(
                prompt.clone(),
                Duration::from_secs(config.llm.openai_compat.timeout_seconds.max(5)),
            );
            let result = llm.complete(request).await;
            timings.cleanup = started.elapsed();

            match result {
                Ok(response) => Some(response.text),
                Err(e) => {
                    // Deliberately not fatal. Every branch of `resolve` has a
                    // fallback that still yields the user's words.
                    tracing::warn!(error = %e, "cleanup failed, falling back");
                    None
                }
            }
        }
    };

    // Apply the sanitizer that matches the branch that ran.
    let sanitized = provider_output.map(|text| match &decision {
        Decision::EditMode(_) => transcript::sanitize_command_mode_transcript(&text),
        Decision::VerbatimTranslation(_) => transcript::sanitize_verbatim_translation(&text),
        _ => transcript::sanitize_post_processed_transcript(&text),
    });

    // Guard against the model performing the dictated instruction instead of
    // cleaning it up. This matters more here than on macOS: the default
    // backend is an agent CLI, which is far more inclined to act.
    let sanitized = match (&decision, sanitized) {
        (Decision::Cleanup(_), Some(text))
            if transcript::appears_to_have_executed_instruction(
                &raw_transcript,
                &text,
                &config.dictation.output_language,
            ) =>
        {
            tracing::warn!("cleanup looks like it executed the transcript; using raw text");
            None
        }
        (_, other) => other,
    };

    let (text, outcome) = pipeline::resolve(&decision, &input, sanitized);
    if text.trim().is_empty() {
        return Ok(PipelineResult {
            text,
            raw_transcript,
            outcome,
            app_class: session.window.class.clone(),
            timings,
        });
    }

    let parsed = pipeline::parse_transcript_commands(&text, true);

    if !should_inject {
        return Ok(PipelineResult {
            text: parsed.transcript,
            raw_transcript,
            outcome,
            app_class: session.window.class.clone(),
            timings,
        });
    }

    report(DaemonState::Injecting);
    let started = Instant::now();
    // Injection shells out to wtype and sleeps between steps; run it on the
    // blocking pool so it cannot stall the async runtime.
    {
        let text = parsed.transcript.clone();
        let session = session.clone();
        let config = config.clone();
        let should_press_enter = parsed.should_press_enter;
        tokio::task::spawn_blocking(move || {
            inject_text(&text, &session, &config)?;
            if should_press_enter {
                // Small settle so the target has processed the typed text
                // before the newline submits it.
                std::thread::sleep(Duration::from_millis(60));
                let _ = inject::press_enter();
            }
            Ok::<(), inject::InjectError>(())
        })
        .await??;
    }
    timings.injection = started.elapsed();

    Ok(PipelineResult {
        text: parsed.transcript,
        raw_transcript,
        outcome,
        app_class: session.window.class.clone(),
        timings,
    })
}

/// Types the result into the window the dictation started in.
pub fn inject_text(
    text: &str,
    session: &SessionContext,
    config: &Config,
) -> Result<(), inject::InjectError> {
    // Focus can move while transcription runs. Put it back, or the text lands
    // in whatever the user clicked on next.
    if let Some(address) = &session.window.address {
        let current = context::active_window();
        if current.address.as_ref() != Some(address) {
            tracing::info!("focus moved during transcription; restoring the original window");
            context::focus_window(address);
            std::thread::sleep(Duration::from_millis(80));
        }
    }

    let method = config.injection.method_for(session.window.class.as_deref());
    inject::inject(text, method, config.injection.keystroke_delay_ms)
}
