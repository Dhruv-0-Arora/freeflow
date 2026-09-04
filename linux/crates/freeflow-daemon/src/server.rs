//! Unix-socket control server.
//!
//! One connection per command, newline-delimited JSON. The socket lives in
//! `$XDG_RUNTIME_DIR`, which is already `0700` and cleaned up when the session
//! ends, so no other user can drive the daemon.

use std::path::Path;
use std::sync::Arc;

use freeflow_core::proto::{encode, DaemonState, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::state::Daemon;

pub async fn serve(daemon: Arc<Daemon>, socket_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // A socket left behind by a crashed daemon would otherwise make bind fail
    // forever. Removing a stale path is safe because only one daemon per
    // session is meaningful.
    let _ = tokio::fs::remove_file(socket_path).await;

    let listener = UnixListener::bind(socket_path)?;
    tracing::info!(socket = %socket_path.display(), "listening");

    let shutdown = tokio_shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let daemon = Arc::clone(&daemon);
                        tokio::spawn(async move {
                            if let Err(e) = handle(daemon, stream).await {
                                tracing::warn!(error = %e, "connection failed");
                            }
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, "accept failed"),
                }
            }
            _ = &mut shutdown => {
                tracing::info!("shutting down");
                break;
            }
        }
    }

    daemon.shutdown().await;
    let _ = tokio::fs::remove_file(socket_path).await;
    Ok(())
}

async fn tokio_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

async fn handle(daemon: Arc<Daemon>, stream: UnixStream) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => dispatch(&daemon, request).await,
            Err(e) => Response::error(daemon.state(), format!("bad request: {e}")),
        };
        write_half.write_all(encode(&response).as_bytes()).await?;
        write_half.flush().await?;
    }
    Ok(())
}

async fn dispatch(daemon: &Arc<Daemon>, request: Request) -> Response {
    match request {
        Request::Shortcut { event, key } => match daemon.handle_shortcut(event, key).await {
            Ok(()) => Response::ok(daemon.state()),
            Err(e) => Response::error(DaemonState::Error, e.to_string()),
        },
        Request::CopyAgain => match daemon.copy_again().await {
            Ok(()) => Response::ok(daemon.state()),
            Err(e) => Response::error(daemon.state(), e.to_string()),
        },
        Request::Cancel => match daemon.cancel().await {
            Ok(()) => Response::ok(daemon.state()).with_message("cancelled"),
            Err(e) => Response::error(daemon.state(), e.to_string()),
        },
        Request::Status => {
            let state = daemon.state();
            let mut response = Response::ok(state).with_message(state.label().to_string());
            response.level = daemon.level().await;
            response
        }
        Request::Doctor => {
            let mut response = Response::ok(daemon.state());
            response.checks = daemon.doctor().await;
            response.ok = response.checks.iter().all(|c| c.ok);
            response
        }
        Request::Bench { wav_path, inject } => {
            match daemon.bench(std::path::Path::new(&wav_path), inject).await {
                Ok((text, timings)) => {
                    let mut response = Response::ok(daemon.state()).with_message(text);
                    response.timings = Some(freeflow_core::proto::StageTimings {
                        transcription_ms: timings.transcription.as_millis() as u64,
                        cleanup_ms: timings.cleanup.as_millis() as u64,
                        injection_ms: timings.injection.as_millis() as u64,
                    });
                    response
                }
                Err(e) => Response::error(daemon.state(), e.to_string()),
            }
        }
        Request::History { limit } => {
            let mut response = Response::ok(daemon.state());
            response.history = daemon.history(limit.clamp(1, 500)).await;
            response
        }
        Request::ClearHistory => match daemon.clear_history().await {
            Ok(()) => Response::ok(daemon.state()).with_message("history cleared"),
            Err(e) => Response::error(daemon.state(), e.to_string()),
        },
        Request::Shutdown => {
            daemon.shutdown().await;
            // The process exits after the reply is flushed, so the caller
            // still gets a confirmation rather than a dropped connection.
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                std::process::exit(0);
            });
            Response::ok(DaemonState::Idle).with_message("shutting down")
        }
    }
}
