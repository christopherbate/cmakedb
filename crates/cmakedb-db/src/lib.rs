//! L3 semantic database (design §3.3, §4.1).
//!
//! A single SQLite file holding the join of source ASTs, the execution
//! trace, and the File API build graph. Ingestion lives in [`ingest`];
//! everything else is thin query helpers used by passes and the CLI.

pub mod diff;
pub mod ingest;
pub mod schema;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

pub struct Db {
    pub conn: Connection,
}

/// Normalize a path string to CMake's spelling: forward slashes on
/// Windows (CMake always emits `C:/Users/...` in traces and the File API,
/// while native Rust paths use backslashes — the mismatch broke every
/// `in_source` join on Windows, §6.5). On Unix this is the identity, so a
/// legitimate `\` in a Unix filename is never corrupted.
pub fn cmake_path_spelling(s: &str) -> String {
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.to_string()
    }
}

/// Path equality in CMake spelling; case-insensitive on Windows (MSVC
/// tooling drifts drive-letter case between channels).
pub fn cmake_path_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Path-prefix test in CMake spelling; case-insensitive on Windows
/// (case-insensitive filesystems, 8.3 aliases aside).
pub fn cmake_path_starts_with(path: &str, prefix: &str) -> bool {
    if cfg!(windows) {
        path.to_ascii_lowercase()
            .starts_with(&prefix.to_ascii_lowercase())
    } else {
        path.starts_with(prefix)
    }
}

impl Db {
    /// Create (or overwrite the schema of) a database at `path`.
    pub fn create(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(schema::SCHEMA)?;
        conn.execute_batch(schema::INDEXES)?;
        let db = Db { conn };
        db.set_meta("schema_version", &schema::SCHEMA_VERSION.to_string())?;
        Ok(db)
    }

    /// Open an existing database read-write.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        // Cheap validity check.
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='events'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .with_context(|| format!("{} is not a cmakedb database", path.display()))?;
        Ok(Db { conn })
    }

    pub fn in_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(schema::SCHEMA)?;
        Ok(Db { conn })
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            (key, value),
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    /// `file:line` display string for an event id, path relativized against
    /// the recorded source dir when possible.
    pub fn event_location(&self, event_id: i64) -> Result<String> {
        let (path, line): (String, i64) = self.conn.query_row(
            "SELECT f.path, e.line FROM events e JOIN files f ON f.id = e.file_id
             WHERE e.id = ?1",
            [event_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(format!("{}:{}", self.display_path(&path), line))
    }

    /// Relativize a path against the recorded source directory for display.
    pub fn display_path(&self, path: &str) -> String {
        let path = cmake_path_spelling(path);
        if let Ok(Some(src)) = self.get_meta("source_dir") {
            let prefix = format!("{}/", src.trim_end_matches('/'));
            if cmake_path_starts_with(&path, &prefix) {
                return path[prefix.len()..].to_string();
            }
        }
        path
    }

    pub fn target_id(&self, name: &str) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, alias_of FROM targets WHERE name = ?1")?;
        let row: Option<(i64, Option<String>)> = stmt
            .query_row([name], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        match row {
            Some((_, Some(alias))) => self.target_id(&alias),
            Some((id, None)) => Ok(Some(id)),
            None => Ok(None),
        }
    }
}
