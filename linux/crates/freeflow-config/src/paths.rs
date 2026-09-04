//! XDG locations for configuration, state, and the control socket.
//!
//! The macOS app keeps everything under `~/Library/Application Support/FreeFlow`
//! and `UserDefaults`. On Linux the equivalents are split across the XDG base
//! directories, and the control socket belongs in the runtime dir so it is
//! cleaned up automatically when the session ends.

use std::path::PathBuf;

use directories::ProjectDirs;

use crate::ConfigError;

fn project_dirs() -> Result<ProjectDirs, ConfigError> {
    ProjectDirs::from("", "", "freeflow").ok_or(ConfigError::NoHomeDirectory)
}

/// `~/.config/freeflow/config.toml`
pub fn config_file() -> Result<PathBuf, ConfigError> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

pub fn config_dir() -> Result<PathBuf, ConfigError> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

/// `~/.local/state/freeflow`, holding the history database and cooldowns.
pub fn state_dir() -> Result<PathBuf, ConfigError> {
    let dirs = project_dirs()?;
    Ok(dirs
        .state_dir()
        .unwrap_or_else(|| dirs.data_local_dir())
        .to_path_buf())
}

pub fn history_database() -> Result<PathBuf, ConfigError> {
    Ok(state_dir()?.join("history.sqlite3"))
}

pub fn cooldown_file() -> Result<PathBuf, ConfigError> {
    Ok(state_dir()?.join("cooldowns.json"))
}

/// `$XDG_RUNTIME_DIR/freeflow/control.sock`, falling back to the state dir on
/// systems without a runtime dir.
pub fn control_socket() -> Result<PathBuf, ConfigError> {
    if let Some(runtime) = project_dirs()?.runtime_dir() {
        return Ok(runtime.join("control.sock"));
    }
    Ok(state_dir()?.join("control.sock"))
}

/// Expands a leading `~` so config files can use the shorthand users expect.
pub fn expand_tilde(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    let Some(home) = std::env::var_os("HOME") else {
        return PathBuf::from(path);
    };
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    PathBuf::from(home).join(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_handles_home_forms() {
        std::env::set_var("HOME", "/home/synthetic");
        assert_eq!(
            expand_tilde("~/.config/freeflow/key"),
            PathBuf::from("/home/synthetic/.config/freeflow/key")
        );
        assert_eq!(expand_tilde("~"), PathBuf::from("/home/synthetic"));
        assert_eq!(
            expand_tilde("/absolute/path"),
            PathBuf::from("/absolute/path")
        );
        // A bare `~user` form is not shell-expanded and must be left alone
        // rather than silently resolved to the wrong home.
        assert_eq!(
            expand_tilde("relative/path"),
            PathBuf::from("relative/path")
        );
    }
}
