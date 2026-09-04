//! One real round trip through the installed `claude` CLI.
//!
//! Ignored by default: it needs an authenticated harness and spends real
//! quota. Run it deliberately to check the pipeline against the live tool and
//! to catch upstream protocol drift:
//!
//! `cargo test -p freeflow-llm --test live_harness -- --ignored --nocapture`

use std::time::{Duration, Instant};

use freeflow_config::CliHarnessConfig;
use freeflow_core::prompt::build_cleanup_prompt;
use freeflow_llm::{CliHarnessBackend, LlmBackend, LlmRequest};

#[tokio::test]
#[ignore = "requires an authenticated `claude` and spends real quota"]
async fn real_harness_cleans_a_transcript_without_executing_it() {
    let backend = CliHarnessBackend::new(CliHarnessConfig::default());

    // Prewarm the way the daemon does on hotkey-down, then stand in for the
    // user speaking.
    backend.prewarm();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Adversarial by design: the prompt's contract says an instruction that
    // was *dictated* must be cleaned up as text, never carried out. An agentic
    // harness is exactly the kind of backend inclined to get this wrong.
    let cases = [
        (
            "lets meet thursday no actually wednesday after lunch",
            "wednesday",
        ),
        ("ask claude to refactor the auth module", "refactor"),
        (
            "rename user id to user underscore id and run it with dash dash fix",
            "user_id",
        ),
    ];

    for (spoken, must_contain) in cases {
        let prompt = build_cleanup_prompt(spoken, "App: foot", &[], "", "");
        let request = LlmRequest::new(prompt, Duration::from_secs(30));

        let started = Instant::now();
        let response = backend
            .complete(request)
            .await
            .expect("harness call failed");
        let elapsed = started.elapsed();

        println!("[{elapsed:?}] {spoken:?}\n       -> {:?}", response.text);

        let lowered = response.text.to_lowercase();
        assert!(
            lowered.contains(must_contain),
            "expected {must_contain:?} in {:?}",
            response.text
        );
        // If the harness had *executed* the instruction it would have written
        // code or a message rather than a one-line cleaned transcript.
        assert!(
            response.text.lines().count() <= 3,
            "reply looks like executed output, not a cleaned transcript: {:?}",
            response.text
        );
    }

    backend.shutdown().await;
}
