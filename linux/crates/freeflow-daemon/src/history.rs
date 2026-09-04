//! Local dictation history.
//!
//! Replaces `Sources/PipelineHistoryStore.swift`, which used CoreData. SQLite
//! is the direct equivalent here and is a smaller dependency than any ORM.
//!
//! History rows contain transcripts, which are among the most sensitive things
//! this program touches. Consequently: the database is created `0600`, it is
//! never transmitted anywhere, it is capped so it does not grow without bound,
//! and it can be switched off entirely with `dictation.history_enabled`.

use std::path::Path;

use freeflow_core::pipeline::Outcome;
use rusqlite::Connection;

/// How many entries to keep. Old rows are pruned on insert rather than by a
/// background job, so the cap holds even if the daemon is never idle.
const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub id: i64,
    /// Unix seconds.
    pub created_at: i64,
    pub raw_transcript: String,
    pub final_text: String,
    pub outcome: String,
    pub app_class: Option<String>,
    pub transcription_ms: i64,
    pub cleanup_ms: i64,
}

/// A dictation about to be written to history.
pub struct NewEntry<'a> {
    pub raw_transcript: &'a str,
    pub final_text: &'a str,
    pub outcome: &'a Outcome,
    pub app_class: Option<&'a str>,
    pub transcription_ms: u64,
    pub cleanup_ms: u64,
    /// Unix seconds. Passed in rather than read here so tests are
    /// deterministic.
    pub now: i64,
}

pub struct History {
    connection: Connection,
}

impl History {
    /// Opens (creating if needed) the history database.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let connection = Connection::open(path)?;
        Self::restrict_permissions(path);
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    /// In-memory store, for tests.
    #[cfg(test)]
    pub fn in_memory() -> rusqlite::Result<Self> {
        let store = Self {
            connection: Connection::open_in_memory()?,
        };
        store.migrate()?;
        Ok(store)
    }

    /// SQLite creates files with the process umask, which is usually 0644.
    /// Transcripts must not be world-readable.
    fn restrict_permissions(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS entries (
                 id                INTEGER PRIMARY KEY AUTOINCREMENT,
                 created_at        INTEGER NOT NULL,
                 raw_transcript    TEXT NOT NULL,
                 final_text        TEXT NOT NULL,
                 outcome           TEXT NOT NULL,
                 app_class         TEXT,
                 transcription_ms  INTEGER NOT NULL DEFAULT 0,
                 cleanup_ms        INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS entries_created_at ON entries (created_at DESC);",
        )
    }

    pub fn record(&self, entry: &NewEntry<'_>) -> rusqlite::Result<i64> {
        self.connection.execute(
            "INSERT INTO entries
                 (created_at, raw_transcript, final_text, outcome, app_class,
                  transcription_ms, cleanup_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                entry.now,
                entry.raw_transcript,
                entry.final_text,
                format!("{:?}", entry.outcome),
                entry.app_class,
                entry.transcription_ms as i64,
                entry.cleanup_ms as i64,
            ],
        )?;
        let id = self.connection.last_insert_rowid();
        self.prune()?;
        Ok(id)
    }

    fn prune(&self) -> rusqlite::Result<()> {
        self.connection.execute(
            "DELETE FROM entries WHERE id NOT IN
                 (SELECT id FROM entries ORDER BY id DESC LIMIT ?1)",
            [MAX_ENTRIES as i64],
        )?;
        Ok(())
    }

    pub fn recent(&self, limit: usize) -> rusqlite::Result<Vec<HistoryEntry>> {
        let mut statement = self.connection.prepare(
            "SELECT id, created_at, raw_transcript, final_text, outcome, app_class,
                    transcription_ms, cleanup_ms
             FROM entries ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(HistoryEntry {
                id: row.get(0)?,
                created_at: row.get(1)?,
                raw_transcript: row.get(2)?,
                final_text: row.get(3)?,
                outcome: row.get(4)?,
                app_class: row.get(5)?,
                transcription_ms: row.get(6)?,
                cleanup_ms: row.get(7)?,
            })
        })?;
        rows.collect()
    }

    #[cfg(test)]
    pub fn count(&self) -> rusqlite::Result<usize> {
        self.connection
            .query_row("SELECT COUNT(*) FROM entries", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|n| n as usize)
    }

    /// Deletes everything. Offered because the store holds transcripts and the
    /// user must be able to get rid of them without hunting for the file.
    pub fn clear(&self) -> rusqlite::Result<()> {
        self.connection.execute("DELETE FROM entries", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(store: &History, text: &str, at: i64) -> i64 {
        store
            .record(&NewEntry {
                raw_transcript: &format!("raw {text}"),
                final_text: text,
                outcome: &Outcome::PostProcessingSucceeded,
                app_class: Some("foot"),
                transcription_ms: 500,
                cleanup_ms: 1400,
                now: at,
            })
            .unwrap()
    }

    #[test]
    fn entries_round_trip() {
        let store = History::in_memory().unwrap();
        record(&store, "synthetic one", 1000);

        let entries = store.recent(10).unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.final_text, "synthetic one");
        assert_eq!(entry.raw_transcript, "raw synthetic one");
        assert_eq!(entry.outcome, "PostProcessingSucceeded");
        assert_eq!(entry.app_class.as_deref(), Some("foot"));
        assert_eq!(entry.transcription_ms, 500);
        assert_eq!(entry.cleanup_ms, 1400);
        assert_eq!(entry.created_at, 1000);
    }

    #[test]
    fn newest_entries_come_first() {
        let store = History::in_memory().unwrap();
        for (i, text) in ["first", "second", "third"].iter().enumerate() {
            record(&store, text, 1000 + i as i64);
        }
        let texts: Vec<_> = store
            .recent(10)
            .unwrap()
            .into_iter()
            .map(|e| e.final_text)
            .collect();
        assert_eq!(texts, ["third", "second", "first"]);
    }

    // The store holds transcripts, so unbounded growth is a privacy problem as
    // well as a disk one.
    #[test]
    fn the_store_is_capped() {
        let store = History::in_memory().unwrap();
        for i in 0..(MAX_ENTRIES + 25) {
            record(&store, &format!("entry {i}"), 1000 + i as i64);
        }
        assert_eq!(store.count().unwrap(), MAX_ENTRIES);
        // Pruning must drop the oldest, not the newest.
        assert_eq!(
            store.recent(1).unwrap()[0].final_text,
            format!("entry {}", MAX_ENTRIES + 24)
        );
    }

    #[test]
    fn clearing_removes_everything() {
        let store = History::in_memory().unwrap();
        record(&store, "synthetic", 1000);
        store.clear().unwrap();
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn opening_twice_reuses_the_existing_database() {
        let dir = std::env::temp_dir().join(format!("freeflow-history-{}", std::process::id()));
        let path = dir.join("history.sqlite3");
        let _ = std::fs::remove_file(&path);

        {
            let store = History::open(&path).unwrap();
            record(&store, "persisted", 1000);
        }
        let reopened = History::open(&path).unwrap();
        assert_eq!(reopened.count().unwrap(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_database_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("freeflow-perm-{}", std::process::id()));
        let path = dir.join("history.sqlite3");
        let _ = std::fs::remove_file(&path);

        let store = History::open(&path).unwrap();
        record(&store, "sensitive synthetic transcript", 1000);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode & 0o077,
            0,
            "history is readable by other users: {mode:o}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
