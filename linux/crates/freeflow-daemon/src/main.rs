//! `freeflowd` - the FreeFlow dictation daemon.
//!
//! Headless by design. The compositor owns key binding, so hotkeys arrive as
//! IPC messages from `freeflow` (the CLI) driven by Hyprland binds, and the
//! overlay is a separate process talking over the same socket. Keeping the
//! daemon free of GUI and input dependencies is what lets the whole pipeline
//! be exercised from a script.

mod history;
mod pipeline;
mod server;
mod state;

use std::sync::Arc;

use freeflow_config::{paths, Config};

fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("FREEFLOW_LOG")
        .unwrap_or_else(|_| EnvFilter::new("freeflow=info,warn"));
    fmt().with_env_filter(filter).with_target(true).init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();

    let config_path = paths::config_file()?;
    let config = Config::load_from(&config_path)?;
    tracing::info!(config = %config_path.display(), "starting freeflowd");

    let socket_path = paths::control_socket()?;
    let daemon = Arc::new(state::Daemon::new(config)?);

    server::serve(daemon, &socket_path).await
}
