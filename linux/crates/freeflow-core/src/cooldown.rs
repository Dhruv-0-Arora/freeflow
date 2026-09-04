//! Per-model rate-limit cooldowns.
//!
//! Ported from `Sources/LLMCooldownManager.swift`. Tracks which models are
//! currently rate limited so a doomed request is skipped rather than sent and
//! paid for with an extra round-trip.
//!
//! Two storage tiers, as on macOS:
//!   - Minute-level limits (retry-after under an hour) stay in memory.
//!   - Daily limits are persisted so the cooldown survives a daemon restart.
//!
//! The macOS build persists to `UserDefaults`; here the same key format is
//! written to a JSON file, since there is no equivalent store on Linux.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Cooldowns at or above this threshold are treated as daily limits and persisted.
const DAILY_LIMIT_THRESHOLD: Duration = Duration::from_secs(3600);

/// Fallback cooldown used when a 429 carries no parseable timing header. Kept
/// well below [`DAILY_LIMIT_THRESHOLD`] so the next call can re-probe soon.
const DEFAULT_REPROBE_COOLDOWN: Duration = Duration::from_secs(60);

/// How long a rate-limited model must wait, and whether the limit is a daily
/// quota (which the caller should persist even when the wait is short).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitCooldown {
    pub duration: Duration,
    pub is_daily: bool,
}

/// Shared key format, kept identical to the macOS `UserDefaults` key so a
/// persisted store stays recognisable across the two implementations.
pub fn persistence_key(model: &str) -> String {
    format!("llm_cooldown_expiry_{model}")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedCooldowns {
    /// Keyed by [`persistence_key`], valued by Unix timestamp seconds.
    expiries: HashMap<String, f64>,
}

#[derive(Debug, Default)]
pub struct LlmCooldownManager {
    in_memory: HashMap<String, SystemTime>,
    persisted: PersistedCooldowns,
    /// `None` keeps everything in memory, which is what tests want.
    path: Option<PathBuf>,
}

impl LlmCooldownManager {
    /// An entirely in-memory manager. Daily limits will not survive a restart.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Loads persisted daily cooldowns from `path`, tolerating a missing or
    /// corrupt file by starting empty rather than failing to start dictation.
    pub fn with_persistence(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let persisted = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            in_memory: HashMap::new(),
            persisted,
            path: Some(path),
        }
    }

    pub fn is_in_cooldown(&mut self, model: &str) -> bool {
        self.is_in_cooldown_at(model, SystemTime::now())
    }

    fn is_in_cooldown_at(&mut self, model: &str, now: SystemTime) -> bool {
        if let Some(&until) = self.in_memory.get(model) {
            if now < until {
                return true;
            }
            self.in_memory.remove(model);
        }

        let key = persistence_key(model);
        if let Some(&timestamp) = self.persisted.expiries.get(&key) {
            let until = UNIX_EPOCH + Duration::from_secs_f64(timestamp.max(0.0));
            if now < until {
                return true;
            }
            self.persisted.expiries.remove(&key);
            self.flush();
        }

        false
    }

    /// Registers a cooldown from a 429 response.
    ///
    /// `is_daily` forces persistence so a daily quota that happens to reset in
    /// under an hour is still remembered across restarts.
    pub fn set_cooldown(&mut self, model: &str, cooldown: RateLimitCooldown) {
        self.set_cooldown_at(model, cooldown, SystemTime::now());
    }

    fn set_cooldown_at(&mut self, model: &str, cooldown: RateLimitCooldown, now: SystemTime) {
        let expiry = now + cooldown.duration;
        if cooldown.is_daily || cooldown.duration >= DAILY_LIMIT_THRESHOLD {
            let timestamp = expiry
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            self.persisted
                .expiries
                .insert(persistence_key(model), timestamp);
            self.flush();
        } else {
            self.in_memory.insert(model.to_string(), expiry);
        }
    }

    /// Picks the model to actually send to: the primary when it is not cooling
    /// down, otherwise the fallback, otherwise `None` so the caller can skip a
    /// request that is certain to be rejected.
    pub fn effective_primary(&mut self, primary: &str, fallback: Option<&str>) -> Option<String> {
        if !self.is_in_cooldown(primary) {
            return Some(primary.to_string());
        }
        let fallback = fallback?;
        if self.is_in_cooldown(fallback) {
            return None;
        }
        Some(fallback.to_string())
    }

    /// Persistence is best-effort: a cooldown that fails to save costs one
    /// wasted request after a restart, which is not worth failing dictation.
    fn flush(&self) {
        let Some(path) = &self.path else { return };
        let Ok(bytes) = serde_json::to_vec(&self.persisted) else {
            return;
        };
        if let Some(parent) = Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, bytes);
    }
}

/// Reads from a 429 response how long the model must cool down, and whether
/// the limit is a daily one.
///
/// Priority: an exhausted daily request quota (`x-ratelimit-remaining-requests`
/// at or below zero, using the `x-ratelimit-reset-requests` RPD reset) →
/// `retry-after` → `x-ratelimit-reset-tokens` (the per-minute TPM reset) → a
/// short re-probe fallback. The RPD check runs *first* because Groq usually
/// also sends `retry-after`; honouring that first would hide a short,
/// near-reset daily window and stop it being persisted.
pub fn rate_limit_cooldown<'a>(header: impl Fn(&str) -> Option<&'a str>) -> RateLimitCooldown {
    let remaining_requests =
        header("x-ratelimit-remaining-requests").and_then(|v| v.trim().parse::<f64>().ok());
    if remaining_requests.is_some_and(|remaining| remaining <= 0.0) {
        if let Some(daily_reset) = header("x-ratelimit-reset-requests").and_then(parse_duration) {
            return RateLimitCooldown {
                duration: daily_reset,
                is_daily: true,
            };
        }
    }

    for name in ["retry-after", "x-ratelimit-reset-tokens"] {
        if let Some(duration) = header(name).and_then(parse_duration) {
            return RateLimitCooldown {
                duration,
                is_daily: false,
            };
        }
    }

    RateLimitCooldown {
        duration: DEFAULT_REPROBE_COOLDOWN,
        is_daily: false,
    }
}

/// Parses a Groq duration string: bare seconds (`2`, `7.66`), a single
/// suffixed unit (`7.66s`, `120ms`), or compound forms (`2m59.56s`, `1h2m3.5s`).
///
/// Returns `None` for empty, unrecognised-unit, negative, or non-finite input,
/// so a malformed header can never produce a negative or infinite cooldown.
fn parse_duration(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    // A bare number is plain seconds, which covers `retry-after`'s integer form.
    if let Ok(seconds) = trimmed.parse::<f64>() {
        return if seconds.is_finite() && seconds >= 0.0 {
            Some(Duration::from_secs_f64(seconds))
        } else {
            None
        };
    }

    // Otherwise accumulate <number><unit> segments left to right.
    let mut total = 0.0f64;
    let mut number = String::new();
    let mut matched_any_unit = false;
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            i += 1;
            continue;
        }
        // Hit a unit: the preceding digits must form a valid number.
        let parsed = number.parse::<f64>().ok()?;
        number.clear();
        // "ms" must be checked before the single-letter "m" and "s".
        if trimmed[i..].starts_with("ms") {
            total += parsed / 1000.0;
            i += 2;
        } else if c == 'h' {
            total += parsed * 3600.0;
            i += 1;
        } else if c == 'm' {
            total += parsed * 60.0;
            i += 1;
        } else if c == 's' {
            total += parsed;
            i += 1;
        } else {
            return None; // Unrecognized unit.
        }
        matched_any_unit = true;
    }

    // Reject a trailing number with no unit (e.g. "1h30") and unit-less input.
    if !number.is_empty() || !matched_any_unit {
        return None;
    }
    if !total.is_finite() || total < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cooldown(headers: &[(&str, &str)]) -> RateLimitCooldown {
        rate_limit_cooldown(|name| {
            headers
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| *value)
        })
    }

    fn assert_secs(actual: Duration, expected: f64) {
        assert!(
            (actual.as_secs_f64() - expected).abs() < 1e-6,
            "expected {expected}s, got {}s",
            actual.as_secs_f64()
        );
    }

    // Ported from Tests/LLMCooldownManagerTests.swift:testDailyQuotaTakesPriority
    #[test]
    fn daily_quota_takes_priority() {
        let result = cooldown(&[
            ("x-ratelimit-remaining-requests", "0"),
            ("x-ratelimit-reset-requests", "2m59.56s"),
            ("retry-after", "2"),
        ]);
        assert_secs(result.duration, 179.56);
        assert!(result.is_daily);

        let non_exhausted = cooldown(&[
            ("x-ratelimit-remaining-requests", "1"),
            ("x-ratelimit-reset-requests", "1h"),
            ("retry-after", "7.66"),
        ]);
        assert_secs(non_exhausted.duration, 7.66);
        assert!(!non_exhausted.is_daily);
    }

    // Ported from Tests/LLMCooldownManagerTests.swift:testRetryAndTokenDurations
    #[test]
    fn retry_and_token_durations() {
        assert_secs(cooldown(&[("retry-after", "120ms")]).duration, 0.12);
        assert_secs(cooldown(&[("retry-after", "1h2m3.5s")]).duration, 3723.5);
        assert_secs(
            cooldown(&[("x-ratelimit-reset-tokens", "8.25s")]).duration,
            8.25,
        );
    }

    // Ported from Tests/LLMCooldownManagerTests.swift:testMalformedDurationsUseSafeFallback
    #[test]
    fn malformed_durations_use_safe_fallback() {
        for invalid in ["-3", "nan", "inf", "1d", "1h30", ""] {
            let result = cooldown(&[("retry-after", invalid)]);
            assert_secs(result.duration, 60.0);
            assert!(!result.is_daily, "{invalid} should not be treated as daily");
        }
    }

    // Ported from Tests/LLMCooldownManagerTests.swift:testPersistenceKeyIsStable
    #[test]
    fn persistence_key_is_stable() {
        assert_eq!(
            persistence_key("openai/gpt-oss-20b"),
            "llm_cooldown_expiry_openai/gpt-oss-20b"
        );
    }

    // New: the routing decision the manager exists to make. The Swift suite
    // covers header parsing but never asserts `effectivePrimary`.
    #[test]
    fn effective_primary_falls_back_then_gives_up() {
        let mut m = LlmCooldownManager::in_memory();
        let minute = RateLimitCooldown {
            duration: Duration::from_secs(60),
            is_daily: false,
        };

        assert_eq!(
            m.effective_primary("primary", Some("fallback")).as_deref(),
            Some("primary")
        );

        m.set_cooldown("primary", minute);
        assert_eq!(
            m.effective_primary("primary", Some("fallback")).as_deref(),
            Some("fallback")
        );
        assert_eq!(m.effective_primary("primary", None), None);

        m.set_cooldown("fallback", minute);
        assert_eq!(m.effective_primary("primary", Some("fallback")), None);
    }

    // New: an expired cooldown must stop blocking, otherwise a single 429
    // would take a model out of service permanently.
    #[test]
    fn expired_cooldown_is_cleared() {
        let mut m = LlmCooldownManager::in_memory();
        let now = SystemTime::now();
        m.set_cooldown_at(
            "primary",
            RateLimitCooldown {
                duration: Duration::from_secs(30),
                is_daily: false,
            },
            now,
        );
        assert!(m.is_in_cooldown_at("primary", now));
        assert!(!m.is_in_cooldown_at("primary", now + Duration::from_secs(31)));
    }

    // New: a daily limit shorter than an hour must still persist, which is the
    // whole reason `is_daily` exists separately from the duration threshold.
    #[test]
    fn short_daily_limit_is_persisted() {
        let dir = std::env::temp_dir().join(format!("freeflow-cooldown-{}", std::process::id()));
        let path = dir.join("cooldowns.json");
        let _ = std::fs::remove_file(&path);

        let mut m = LlmCooldownManager::with_persistence(&path);
        m.set_cooldown(
            "primary",
            RateLimitCooldown {
                duration: Duration::from_secs(120),
                is_daily: true,
            },
        );

        let mut reloaded = LlmCooldownManager::with_persistence(&path);
        assert!(
            reloaded.is_in_cooldown("primary"),
            "a short daily limit must survive a restart"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
