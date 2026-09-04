//! Configuration for the FreeFlow Linux daemon.
//!
//! One TOML file at `~/.config/freeflow/config.toml`, plus secrets kept in
//! separate 0600 files referenced by path. Secrets are never inlined in the
//! config so it stays safe to commit into a Nix/home-manager repository, which
//! is how this machine is configured.
//!
//! Every field has a default, so an absent config file is valid and yields a
//! working local-transcription setup.

pub mod paths;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine the user's home directory")]
    NoHomeDirectory,
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(
        "secret file {path} is readable by other users (mode {mode:o}); run: chmod 600 {path}"
    )]
    InsecureSecret { path: PathBuf, mode: u32 },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub audio: AudioConfig,
    pub transcription: TranscriptionConfig,
    pub llm: LlmConfig,
    pub injection: InjectionConfig,
    pub dictation: DictationConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Capture device name. `None` follows the PipeWire default source, which
    /// is usually right but is a filter node rather than one of the hardware
    /// sources listed by `wpctl status` - see `freeflow devices`.
    pub device: Option<String>,
    /// Hard cap on a single recording. Guards against a missed hotkey release
    /// leaving the microphone open indefinitely.
    pub max_recording_seconds: u64,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: None,
            max_recording_seconds: 120,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TranscriptionConfig {
    /// Any OpenAI-compatible base URL. The default points at a local
    /// `whisper-server` started with `--request-path /v1/audio
    /// --inference-path /transcriptions`, which makes it serve the same route
    /// shape as Groq or OpenAI.
    pub base_url: String,
    pub model: String,
    pub api_key_file: Option<String>,
    /// Optional ISO-639-1 hint. Empty means auto-detect.
    pub language: String,
    pub timeout_seconds: u64,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080/v1/audio".to_string(),
            model: "whisper-large-v3-turbo".to_string(),
            api_key_file: None,
            language: String::new(),
            timeout_seconds: 60,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmBackendKind {
    /// Drive a local agent CLI (`claude`, `agy`, ...) over its streaming protocol.
    CliHarness,
    /// Any OpenAI-compatible chat-completions endpoint.
    #[serde(rename = "openai_compat")]
    OpenAiCompat,
    /// Skip cleanup entirely and paste the raw transcript.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    pub backend: LlmBackendKind,
    /// Used when `backend` is unhealthy or times out. When this also fails the
    /// daemon pastes the raw transcript rather than losing the user's words.
    pub fallback_backend: Option<LlmBackendKind>,
    pub cli_harness: CliHarnessConfig,
    pub openai_compat: OpenAiCompatConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            backend: LlmBackendKind::CliHarness,
            fallback_backend: None,
            cli_harness: CliHarnessConfig::default(),
            openai_compat: OpenAiCompatConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessProtocol {
    /// Claude Code's `--input-format stream-json --output-format stream-json`.
    ClaudeStreamJson,
    /// Antigravity's (`agy`) stream-json dialect: `{"event":"user","message":
    /// {"content":...}}` in, `{"event":"result","result":{...}}` out. Unlike
    /// Claude's it has no system-prompt flag, so the wrapper travels at the
    /// top of each user turn instead.
    AgyStreamJson,
    /// One request on stdin, one response on stdout, no framing.
    PlainStdio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CliHarnessConfig {
    pub command: String,
    pub args: Vec<String>,
    pub protocol: HarnessProtocol,
    /// How many bootstrapped workers to keep ready. Each is a full agent
    /// process, so this trades memory for latency.
    pub pool_size: usize,
    /// Send a throwaway turn on spawn so the first real request pays the
    /// cheaper subsequent-message cost instead of the first-message cost.
    pub prime_on_spawn: bool,
    /// Discard a worker after this many real turns. `1` guarantees no
    /// transcript is ever visible to a later dictation; raising it reuses
    /// bootstrap at the cost of cross-dictation context.
    pub recycle_after: u32,
    pub request_timeout_ms: u64,
    /// How long to wait for a worker that is already bootstrapping before
    /// giving up and starting a cold one inline.
    ///
    /// Should exceed the harness's bootstrap time. Waiting for a warm worker
    /// that then answers in ~1.2s beats a cold spawn that pays bootstrap
    /// (~2-3s) *and* the answer, so this is deliberately generous.
    pub pool_wait_timeout_ms: u64,
}

impl Default for CliHarnessConfig {
    fn default() -> Self {
        Self {
            command: "claude".to_string(),
            args: vec![
                "-p".into(),
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--allowed-tools".into(),
                String::new(),
                // Without these the harness loads the user's MCP servers into
                // every dictation, which is slow and irrelevant to cleanup.
                "--strict-mcp-config".into(),
                "--mcp-config".into(),
                r#"{"mcpServers":{}}"#.into(),
            ],
            protocol: HarnessProtocol::ClaudeStreamJson,
            pool_size: 1,
            prime_on_spawn: true,
            recycle_after: 1,
            request_timeout_ms: 15_000,
            pool_wait_timeout_ms: 5_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiCompatConfig {
    pub base_url: String,
    pub model: String,
    /// Tried when the primary model is rate limited or errors.
    pub fallback_model: Option<String>,
    pub api_key_file: Option<String>,
    pub timeout_seconds: u64,
}

impl Default for OpenAiCompatConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.groq.com/openai/v1".to_string(),
            model: "openai/gpt-oss-20b".to_string(),
            fallback_model: Some("qwen/qwen3.6-27b".to_string()),
            api_key_file: None,
            timeout_seconds: 20,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionMethod {
    /// Type the text directly via the virtual-keyboard protocol. No clipboard
    /// involvement, so nothing to snapshot or restore.
    Wtype,
    /// Copy then synthesize Ctrl-V, restoring the previous clipboard after.
    /// Needed for clients that mishandle synthesized key events.
    Clipboard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InjectionConfig {
    pub method: InjectionMethod,
    /// Per-window-class overrides, keyed by the `class` reported by
    /// `hyprctl activewindow -j`.
    pub method_by_class: std::collections::BTreeMap<String, InjectionMethod>,
    /// Inter-keystroke delay passed to `wtype`. Some clients drop keys at 0.
    pub keystroke_delay_ms: u64,
}

impl Default for InjectionConfig {
    fn default() -> Self {
        Self {
            method: InjectionMethod::Wtype,
            method_by_class: std::collections::BTreeMap::new(),
            keystroke_delay_ms: 0,
        }
    }
}

impl InjectionConfig {
    /// Resolves the method for a window class, falling back to the global one.
    pub fn method_for(&self, window_class: Option<&str>) -> InjectionMethod {
        window_class
            .and_then(|class| self.method_by_class.get(class))
            .copied()
            .unwrap_or(self.method)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DictationConfig {
    /// Names, jargon, and project terms the cleanup pass must preserve.
    pub custom_vocabulary: Vec<String>,
    /// Replaces the built-in cleanup prompt entirely when non-empty.
    pub custom_system_prompt: String,
    /// Non-empty asks the cleanup pass to translate into this language.
    pub output_language: String,
    /// Skip cleanup and paste the raw transcript. Fast and fully offline when
    /// transcription is local.
    pub preserve_exact_wording: bool,
    /// Use the primary selection as edit-mode input when it is non-empty.
    pub edit_mode_enabled: bool,
    pub history_enabled: bool,
}

impl Default for DictationConfig {
    fn default() -> Self {
        Self {
            custom_vocabulary: Vec::new(),
            custom_system_prompt: String::new(),
            output_language: String::new(),
            preserve_exact_wording: false,
            edit_mode_enabled: true,
            history_enabled: true,
        }
    }
}

impl Config {
    /// Loads the config from the default XDG location, returning defaults when
    /// the file does not exist.
    pub fn load() -> Result<Self, ConfigError> {
        let path = paths::config_file()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Reads an API key from a file, refusing to use one that other users can read.
///
/// Failing loudly is deliberate: silently continuing with a world-readable
/// credential is worse than not starting.
pub fn read_secret_file(path: &Path) -> Result<String, ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ConfigError::InsecureSecret {
                path: path.to_path_buf(),
                mode,
            });
        }
    }
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn missing_config_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let original = Config::default();
        let text = toml::to_string(&original).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn partial_config_keeps_defaults_for_absent_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[llm]
backend = "openai_compat"

[dictation]
custom_vocabulary = ["Hyprland", "wtype"]
"#,
        )
        .unwrap();

        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.llm.backend, LlmBackendKind::OpenAiCompat);
        assert_eq!(config.dictation.custom_vocabulary, ["Hyprland", "wtype"]);
        // Untouched sections must still hold their defaults.
        assert_eq!(config.audio, AudioConfig::default());
        assert_eq!(config.llm.cli_harness, CliHarnessConfig::default());
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_ignored() {
        // A typo in a config key would otherwise silently do nothing, which is
        // the single most confusing failure mode for a background daemon.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[audio]\ndevise = \"typo\"\n").unwrap();
        assert!(matches!(
            Config::load_from(&path),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn injection_method_falls_back_to_global() {
        let mut injection = InjectionConfig::default();
        injection
            .method_by_class
            .insert("Chromium".to_string(), InjectionMethod::Clipboard);
        assert_eq!(
            injection.method_for(Some("Chromium")),
            InjectionMethod::Clipboard
        );
        assert_eq!(injection.method_for(Some("foot")), InjectionMethod::Wtype);
        assert_eq!(injection.method_for(None), InjectionMethod::Wtype);
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_permissions_are_enforced() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.key");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "  synthetic-key-value  ").unwrap();
        drop(file);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            read_secret_file(&path),
            Err(ConfigError::InsecureSecret { .. })
        ));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_secret_file(&path).unwrap(), "synthetic-key-value");
    }
}
