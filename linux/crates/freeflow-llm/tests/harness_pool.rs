//! Worker-pool behaviour against a fake harness process.
//!
//! These drive real child processes, so they live here rather than as unit
//! tests: `CARGO_BIN_EXE_*` is only available to integration tests. The mock
//! harness lets every failure path - hang, crash, unauthenticated, missing
//! binary - be exercised deterministically and without spending real quota.

use std::time::{Duration, Instant};

use freeflow_config::{CliHarnessConfig, HarnessProtocol};
use freeflow_core::prompt::Prompt;
use freeflow_llm::{BackendHealth, CliHarnessBackend, LlmBackend, LlmError, LlmRequest};

const MOCK: &str = env!("CARGO_BIN_EXE_freeflow-mock-harness");

/// A backend pointed at the mock harness, with the given fault flags.
fn config_with(faults: &[&str]) -> CliHarnessConfig {
    CliHarnessConfig {
        command: MOCK.to_string(),
        args: faults.iter().map(|s| s.to_string()).collect(),
        protocol: HarnessProtocol::ClaudeStreamJson,
        pool_size: 1,
        // Priming *is* the bootstrap for a stream-json harness.
        prime_on_spawn: true,
        recycle_after: 1,
        request_timeout_ms: 5_000,
        pool_wait_timeout_ms: 2_000,
    }
}

fn request(user: &str) -> LlmRequest {
    LlmRequest::new(
        Prompt {
            system: "INSTR".into(),
            user: user.into(),
        },
        Duration::from_secs(10),
    )
}

#[tokio::test]
async fn completes_a_turn_and_sends_both_prompt_halves() {
    let backend = CliHarnessBackend::new(config_with(&[]));
    let response = backend.complete(request("PAYLOAD")).await.unwrap();
    // The mock echoes the turn it received.
    assert!(response.text.starts_with("mock:"));
    assert!(response.text.contains("INSTRUCTIONS:\nINSTR"));
    assert!(response.text.contains("INPUT:\nPAYLOAD"));
    backend.shutdown().await;
}

#[tokio::test]
async fn workers_are_used_once_so_transcripts_cannot_leak() {
    let backend = CliHarnessBackend::new(config_with(&[]));
    // The mock exits after one turn. If the pool reused a worker, the
    // second request would hit a dead process and fail.
    for payload in ["FIRST", "SECOND", "THIRD"] {
        let response = backend.complete(request(payload)).await.unwrap();
        assert!(response.text.contains(payload));
    }
    backend.shutdown().await;
}

#[tokio::test]
async fn prewarm_makes_the_next_request_faster() {
    // Bootstrap slow enough that the difference is unambiguous.
    let mut config = config_with(&["--bootstrap-delay-ms", "600"]);
    config.pool_wait_timeout_ms = 5_000;
    let backend = CliHarnessBackend::new(config);

    backend.prewarm();
    // Stand in for the user speaking while bootstrap happens.
    tokio::time::sleep(Duration::from_millis(900)).await;

    let started = Instant::now();
    backend.complete(request("X")).await.unwrap();
    let warm = started.elapsed();

    assert!(
        warm < Duration::from_millis(400),
        "prewarmed request took {warm:?}; bootstrap was not hidden"
    );
    backend.shutdown().await;
}

#[tokio::test]
async fn a_harness_that_hangs_during_bootstrap_fails_fast() {
    // Priming is the bootstrap, so a harness that never answers is caught
    // there. What matters is that it fails quickly and does not leave the
    // daemon stuck mid-dictation.
    let mut config = config_with(&["--hang"]);
    config.request_timeout_ms = 300;
    config.pool_wait_timeout_ms = 100;
    let backend = CliHarnessBackend::new(config);

    let started = Instant::now();
    let err = backend.complete(request("X")).await.unwrap_err();
    let elapsed = started.elapsed();

    assert!(matches!(err, LlmError::Unavailable { .. }), "got {err:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "took {elapsed:?}; a hanging harness must not wedge dictation"
    );
    backend.shutdown().await;
}

#[tokio::test]
async fn a_harness_that_hangs_on_the_real_turn_times_out() {
    // Primes normally, then stops answering. This is the path where a worker
    // was handed out and then went silent.
    let mut config = config_with(&["--hang-after-turns", "1"]);
    config.request_timeout_ms = 300;
    let backend = CliHarnessBackend::new(config);

    let err = backend.complete(request("X")).await.unwrap_err();
    assert!(matches!(err, LlmError::Timeout { .. }), "got {err:?}");
    backend.shutdown().await;
}

#[tokio::test]
async fn a_harness_that_crashes_during_bootstrap_is_unavailable() {
    // Crashing on the priming turn means the worker never becomes usable, so
    // the pool has nothing to hand out.
    let mut config = config_with(&["--crash-after-turns", "0"]);
    config.pool_wait_timeout_ms = 100;
    let backend = CliHarnessBackend::new(config);
    let err = backend.complete(request("X")).await.unwrap_err();
    assert!(matches!(err, LlmError::Unavailable { .. }), "got {err:?}");
    backend.shutdown().await;
}

#[tokio::test]
async fn a_harness_that_crashes_mid_turn_reports_transport_failure() {
    // Survives priming, then dies on the real turn. The pool handed out a
    // worker that then went away, which is a transport failure, not an
    // unavailable backend.
    let backend = CliHarnessBackend::new(config_with(&["--crash-after-turns", "1"]));
    let err = backend.complete(request("X")).await.unwrap_err();
    assert!(matches!(err, LlmError::Transport { .. }), "got {err:?}");
    backend.shutdown().await;
}

#[tokio::test]
async fn repeated_spawn_failures_trip_the_circuit_breaker() {
    let mut config = config_with(&[]);
    config.command = "/nonexistent/harness/binary".into();
    config.pool_wait_timeout_ms = 50;
    let backend = CliHarnessBackend::new(config);

    assert_eq!(backend.health(), BackendHealth::Ready);
    for _ in 0..3 {
        assert!(backend.complete(request("X")).await.is_err());
    }
    // Once tripped the router must skip this backend rather than spawn on
    // every keypress against a harness that cannot start.
    assert_eq!(backend.health(), BackendHealth::Unavailable);
    backend.shutdown().await;
}

#[tokio::test]
async fn an_unauthenticated_harness_is_reported_not_retried_forever() {
    let mut config = config_with(&["--fail-bootstrap"]);
    config.pool_wait_timeout_ms = 50;
    let backend = CliHarnessBackend::new(config);
    let err = backend.complete(request("X")).await.unwrap_err();
    assert!(
        matches!(
            err,
            LlmError::Unavailable { .. } | LlmError::Transport { .. }
        ),
        "got {err:?}"
    );
    backend.shutdown().await;
}
