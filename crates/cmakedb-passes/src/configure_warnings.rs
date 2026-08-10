//! `configure-warnings` (lint roadmap Tier 2): CMake's own configure-time
//! diagnostics — dev/author warnings, deprecation warnings, policy advice,
//! errors — surfaced as findings so they become lintable, gateable, and
//! SARIF-visible. The recorder captures configure stderr; ingestion parses
//! the warning blocks into `configure_diagnostics` (schema v3); this pass
//! is a plain read of that table.
//!
//! No false-positive analysis applies: the messages are CMake's own words.
//! The value is routing (locations, SARIF) and ratcheting.

use anyhow::Result;
use cmakedb_db::Db;

use crate::{Finding, Pass, PassConfig, Severity, SourceSpan};

pub struct ConfigureWarnings;

impl Pass for ConfigureWarnings {
    fn id(&self) -> &'static str {
        "configure-warnings"
    }
    fn description(&self) -> &'static str {
        "CMake's own configure-time diagnostics (dev/policy/deprecation warnings)"
    }

    fn run(&self, db: &Db, _cfg: &PassConfig) -> Result<Vec<Finding>> {
        // Databases recorded before schema v3 have no diagnostics table;
        // that's "nothing captured", not an error.
        let present: i64 = db.conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'configure_diagnostics'",
            [],
            |r| r.get(0),
        )?;
        if present == 0 {
            return Ok(vec![]);
        }
        let mut stmt = db.conn.prepare(
            "SELECT severity, kind, file, line, message
             FROM configure_diagnostics ORDER BY id",
        )?;
        let rows: Vec<(String, String, String, i64, String)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut findings = Vec::new();
        for (severity, kind, file, line, message) in rows {
            let severity: Severity = severity.parse().unwrap_or(Severity::Warning);
            let message = if kind.is_empty() {
                message
            } else {
                format!("({kind}) {message}")
            };
            // Blocks with no location get the '<'-prefixed sentinel the
            // renderers already treat as "not a real file" (SARIF drops it
            // from relatedLocations; text prints it verbatim).
            let primary = if file.is_empty() {
                SourceSpan::new("<configure>", 0)
            } else {
                SourceSpan::new(db.display_path(&file), line)
            };
            findings.push(Finding {
                rule: self.id().into(),
                severity,
                message,
                primary,
                related: vec![],
                fix: None,
            });
        }
        Ok(findings)
    }
}
