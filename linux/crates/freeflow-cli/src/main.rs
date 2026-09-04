//! `freeflow` - the control CLI for `freeflowd`.
//!
//! Deliberately thin. Hyprland dispatches a bind by exec'ing a process, so
//! this runs on every keypress and must start fast and exit fast: it opens the
//! control socket, writes one line, reads one line, and leaves. All the work
//! happens in the daemon.

use std::io::Write;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use freeflow_config::{paths, Config};
use freeflow_core::proto::{encode, Request, Response};
use freeflow_core::shortcut::ShortcutEvent;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "freeflow", about = "Control the FreeFlow dictation daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Push-to-talk edge. Bind `press` to a Hyprland `bind`; with `--key`
    /// the daemon detects the release itself via /dev/input. (A compositor
    /// release bind would consume the key's press for every modifier state.)
    Hold {
        #[arg(value_enum)]
        edge: Edge,
        /// The bind's main key as Hyprland spells it, e.g. `D`.
        #[arg(long)]
        key: Option<String>,
    },
    /// Tap to start, tap again to stop.
    Toggle {
        #[arg(value_enum, default_value = "press")]
        edge: Edge,
        /// The bind's main key as Hyprland spells it, e.g. `T`.
        #[arg(long)]
        key: Option<String>,
    },
    /// Convert an in-progress hold into a toggle, so it survives key release.
    Latch {
        /// The *toggle* bind's key: releasing it is what arms the toggle to
        /// stop, exactly as if the toggle had been started by hand.
        #[arg(long)]
        key: Option<String>,
    },
    /// Re-inject the last result into the currently focused window.
    CopyAgain,
    /// Abandon the current dictation without injecting.
    Cancel,
    /// Print what the daemon is doing.
    Status,
    /// Check the environment: microphone, wtype, compositor, backends.
    Doctor,
    /// List capture devices, for `audio.device` in the config.
    Devices,
    /// Print a fully-populated config file.
    DefaultConfig,
    /// Print the Hyprland bindings to add to hyprland.conf.
    Binds,
    /// Show recent dictations stored locally.
    History {
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Delete every stored dictation.
        #[arg(long)]
        clear: bool,
    },
    /// Run the pipeline against a WAV file and report per-stage latency.
    Bench {
        wav: String,
        /// Also type the result into the focused window.
        #[arg(long)]
        inject: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Edge {
    Press,
    Release,
}

/// How long to wait for a reply.
///
/// Shortcut commands are dispatched synchronously by the compositor on every
/// keypress, so they must never block it for long - the daemon does the work
/// asynchronously and replies immediately. `bench` is the opposite: it runs
/// the whole pipeline before answering, and that legitimately takes seconds.
fn deadline(request: &Request) -> Duration {
    match request {
        Request::Bench { .. } => Duration::from_secs(180),
        Request::Doctor => Duration::from_secs(30),
        _ => Duration::from_secs(5),
    }
}

async fn send(request: Request) -> anyhow::Result<Response> {
    let deadline = deadline(&request);
    let socket = paths::control_socket()?;
    let stream = UnixStream::connect(&socket).await.with_context(|| {
        format!(
            "could not reach freeflowd at {}; is the service running? \
             (systemctl --user status freeflow)",
            socket.display()
        )
    })?;

    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(encode(&request).as_bytes()).await?;
    write_half.flush().await?;

    let mut lines = BufReader::new(read_half).lines();
    // Always bounded, so a wedged daemon cannot block the compositor's bind
    // dispatch indefinitely.
    let line = tokio::time::timeout(deadline, lines.next_line())
        .await
        .context("timed out waiting for freeflowd")??
        .context("freeflowd closed the connection without replying")?;
    Ok(serde_json::from_str(&line)?)
}

/// The bindings to paste into `hyprland.conf`.
///
/// `binds.conf` is the canonical copy: the Home Manager module renders the
/// same text from its defaults and the README quotes it, with a flake check
/// holding all three together. Press binds only - release detection lives in
/// the daemon, because a compositor release bind consumes the key's press.
const HYPRLAND_BINDS: &str = include_str!("../binds.conf");

fn print_status(response: &Response) {
    let mut line = format!("{:?}", response.state);
    if let Some(message) = &response.message {
        line = message.clone();
    }
    if let Some(level) = response.level {
        let bars = (level * 20.0).round() as usize;
        line.push_str(&format!(
            "  [{}{}] {:.2}",
            "#".repeat(bars.min(20)),
            "-".repeat(20 - bars.min(20)),
            level
        ));
    }
    println!("{line}");
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Local commands that do not need the daemon, so they still work when it
    // is not running - which is exactly when you need them.
    match &cli.command {
        Command::Devices => {
            for device in freeflow_platform::audio::list_input_devices()? {
                println!(
                    "{} {:<28} {}",
                    if device.is_default { "*" } else { " " },
                    device.id,
                    device.description
                );
            }
            println!("\n* is the default. Set audio.device in the config to override.");
            return Ok(());
        }
        Command::DefaultConfig => {
            print!("{}", toml::to_string_pretty(&Config::default())?);
            return Ok(());
        }
        Command::Binds => {
            print!("{HYPRLAND_BINDS}");
            return Ok(());
        }
        _ => {}
    }

    let request = match cli.command {
        Command::Hold { edge, key } => Request::Shortcut {
            event: match edge {
                Edge::Press => ShortcutEvent::HoldActivated,
                Edge::Release => ShortcutEvent::HoldDeactivated,
            },
            key,
        },
        Command::Toggle { edge, key } => Request::Shortcut {
            event: match edge {
                Edge::Press => ShortcutEvent::ToggleActivated,
                Edge::Release => ShortcutEvent::ToggleDeactivated,
            },
            key,
        },
        // Latching reuses the toggle edge: the session machine treats a toggle
        // arriving during a hold as "switch to toggle mode".
        Command::Latch { key } => Request::Shortcut {
            event: ShortcutEvent::ToggleActivated,
            key,
        },
        Command::CopyAgain => Request::CopyAgain,
        Command::Cancel => Request::Cancel,
        Command::Status => Request::Status,
        Command::History { limit, clear } => {
            if clear {
                Request::ClearHistory
            } else {
                Request::History { limit }
            }
        }
        Command::Bench { wav, inject } => Request::Bench {
            wav_path: std::fs::canonicalize(&wav)
                .with_context(|| format!("{wav}: not found"))?
                .to_string_lossy()
                .into_owned(),
            inject,
        },
        Command::Doctor => Request::Doctor,
        Command::Devices | Command::DefaultConfig | Command::Binds => unreachable!(),
    };

    let is_doctor = matches!(request, Request::Doctor);
    let is_status = matches!(request, Request::Status);
    let is_bench = matches!(request, Request::Bench { .. });
    let is_history = matches!(request, Request::History { .. });
    let response = send(request).await?;

    if is_doctor {
        for check in &response.checks {
            println!(
                "{} {:<14} {}",
                if check.ok { "ok  " } else { "FAIL" },
                check.name,
                check.detail
            );
        }
        std::io::stdout().flush()?;
        if !response.ok {
            bail!("some checks failed");
        }
        return Ok(());
    }

    if is_status {
        print_status(&response);
        return Ok(());
    }

    if is_history {
        if response.history.is_empty() {
            println!("no history (is dictation.history_enabled set?)");
        }
        for row in &response.history {
            println!(
                "{}  {:>5}ms+{:>5}ms  {:<12} {}",
                row.created_at,
                row.transcription_ms,
                row.cleanup_ms,
                row.app_class.as_deref().unwrap_or("-"),
                row.final_text
            );
            if row.raw_transcript != row.final_text {
                println!("{:>34}raw: {}", "", row.raw_transcript);
            }
        }
        return Ok(());
    }

    if is_bench {
        if !response.ok {
            bail!(response.message.unwrap_or_else(|| "bench failed".into()));
        }
        if let Some(t) = response.timings {
            let total = t.transcription_ms + t.cleanup_ms + t.injection_ms;
            println!(
                "transcription {:>5}ms   cleanup {:>5}ms   injection {:>5}ms   total {:>5}ms",
                t.transcription_ms, t.cleanup_ms, t.injection_ms, total
            );
        }
        println!("result: {:?}", response.message.unwrap_or_default());
        return Ok(());
    }

    if !response.ok {
        bail!(response.message.unwrap_or_else(|| "request failed".into()));
    }
    Ok(())
}
