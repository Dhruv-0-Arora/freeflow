//! A stand-in for `claude` that speaks the same stream-json protocol.
//!
//! Exists so the worker pool's failure handling - slow bootstrap, mid-turn
//! crash, EOF, auth failure, hangs - can be tested deterministically, offline,
//! and without spending real quota.
//!
//! Faults are configured through argv rather than the environment so that
//! concurrently running tests cannot affect each other:
//!
//! - `--bootstrap-delay-ms N`  delay before announcing readiness
//! - `--turn-delay-ms N`       delay before answering each turn
//! - `--fail-bootstrap`        exit non-zero instead of becoming ready
//! - `--crash-after-turns N`   exit abruptly once this many turns are served
//! - `--hang`                  accept the turn and never answer
//! - `--hang-after-turns N`   answer N turns, then stop answering
//! - `--reply-prefix S`        prefix for echoed replies (default `mock:`)
//!
//! Any other argument is ignored, so the flags the backend always appends
//! (`--system-prompt ...`) pass through harmlessly.

use std::io::{BufRead, Write};
use std::time::Duration;

#[derive(Default)]
struct Faults {
    bootstrap_delay_ms: u64,
    turn_delay_ms: u64,
    fail_bootstrap: bool,
    crash_after_turns: Option<u64>,
    hang: bool,
    hang_after_turns: Option<u64>,
    reply_prefix: String,
}

fn parse_args() -> Faults {
    let mut faults = Faults {
        reply_prefix: "mock:".to_string(),
        ..Default::default()
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let value = |i: usize| args.get(i + 1).and_then(|v| v.parse().ok());
        match args[i].as_str() {
            "--bootstrap-delay-ms" => {
                faults.bootstrap_delay_ms = value(i).unwrap_or(0);
                i += 1;
            }
            "--turn-delay-ms" => {
                faults.turn_delay_ms = value(i).unwrap_or(0);
                i += 1;
            }
            "--crash-after-turns" => {
                faults.crash_after_turns = value(i);
                i += 1;
            }
            "--reply-prefix" => {
                faults.reply_prefix = args.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            "--hang-after-turns" => {
                faults.hang_after_turns = value(i);
                i += 1;
            }
            "--fail-bootstrap" => faults.fail_bootstrap = true,
            "--hang" => faults.hang = true,
            _ => {}
        }
        i += 1;
    }
    faults
}

fn main() {
    let faults = parse_args();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    if faults.fail_bootstrap {
        eprintln!("mock harness: simulated authentication failure");
        std::process::exit(3);
    }
    // Deliberately silent until a turn arrives. Verified against `claude`
    // 2.1.223: even the `system`/`init` event is emitted only after the first
    // user message, so a mock that announced readiness up front would let the
    // pool pass tests against a protocol the real harness does not speak.
    let mut turns = 0u64;
    let mut announced = false;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        turns += 1;

        if !announced {
            // Session setup cost, paid on the first turn like the real thing.
            if faults.bootstrap_delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(faults.bootstrap_delay_ms));
            }
            let _ = writeln!(
                stdout,
                r#"{{"type":"system","subtype":"init","session_id":"mock-session"}}"#
            );
            let _ = stdout.flush();
            announced = true;
        }

        if faults.hang || faults.hang_after_turns.is_some_and(|n| turns > n) {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        if faults.crash_after_turns.is_some_and(|n| turns > n) {
            std::process::exit(9);
        }
        if faults.turn_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(faults.turn_delay_ms));
        }

        // Echo the turn back so tests can assert on what was actually sent.
        let text = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|v| {
                v.pointer("/message/content/0/text")
                    .and_then(|t| t.as_str().map(str::to_string))
            })
            .unwrap_or_else(|| "<unparsed>".to_string());

        let result = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": format!("{}{text}", faults.reply_prefix),
        });
        let _ = writeln!(stdout, "{result}");
        let _ = stdout.flush();
    }
}
