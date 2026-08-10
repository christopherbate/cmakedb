//! Scope-leak detection (design §4.2): variable writes executed inside a
//! *macro* body (macros don't create scopes) that land in the caller's
//! scope — the classic macro-vs-function bug.

use anyhow::Result;
use cmakedb_db::Db;

use crate::{event_in_source, event_span, Finding, Pass, PassConfig, Severity, SourceSpan};

pub struct ScopeLeaks;

impl Pass for ScopeLeaks {
    fn id(&self) -> &'static str {
        "scope-leaks"
    }
    fn description(&self) -> &'static str {
        "macro-body writes that escape into the caller's scope"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();
        // Writes whose event executed in a macro scope. Effective scope is
        // always an ancestor (macros are transparent), i.e. the caller's.
        let mut stmt = db.conn.prepare(
            "SELECT w.id, w.event_id, w.name, es.name, es.opened_by_event
             FROM var_writes w
             JOIN events e ON e.id = w.event_id
             JOIN scopes es ON es.id = e.scope_id
             WHERE es.kind = 'macro' AND w.write_kind = 'set'
             ORDER BY w.id",
        )?;
        let rows: Vec<(i64, i64, String, Option<String>, Option<i64>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut seen = std::collections::HashSet::new();
        for (wid, eid, name, macro_name, opened_by) in rows {
            if cfg.ignored(&name) || !event_in_source(db, eid) {
                continue;
            }
            let macro_name = macro_name.unwrap_or_else(|| "<macro>".into());
            // One finding per (macro, variable) pair.
            if !seen.insert((macro_name.clone(), name.clone())) {
                continue;
            }
            // §4.2: severity escalates when the name is also written in the
            // caller's scope by other code — that's a real collision, not
            // just an unhygienic temp.
            let clash: i64 = db.conn.query_row(
                "SELECT count(*) FROM var_writes w2
                 JOIN var_writes w ON w.id = ?1
                 JOIN events e2 ON e2.id = w2.event_id
                 JOIN scopes s2 ON s2.id = e2.scope_id
                 WHERE w2.name = w.name AND w2.scope_id = w.scope_id
                   AND w2.id != w.id AND s2.kind != 'macro'",
                [wid],
                |r| r.get(0),
            )?;
            let (severity, detail) = if clash > 0 {
                (
                    Severity::Warning,
                    "the caller also writes this name — the macro clobbers it",
                )
            } else {
                (
                    Severity::Note,
                    "consider a function() or unset() at the end of the macro",
                )
            };
            let mut related: Vec<(SourceSpan, String)> = Vec::new();
            if let Some(open_eid) = opened_by {
                related.push((event_span(db, open_eid), "macro invoked here".into()));
            }
            findings.push(Finding {
                rule: self.id().into(),
                severity,
                message: format!(
                    "macro '{macro_name}' writes '{name}' into the caller's scope; {detail}"
                ),
                primary: event_span(db, eid),
                related,
                fix: None,
            });
        }
        Ok(findings)
    }
}
