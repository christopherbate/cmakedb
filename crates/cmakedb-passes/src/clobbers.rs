//! Clobber detection (design §4.2): two writes in the same scope where the
//! second's resolving reads never observed the first, values differ, and
//! the writes are not loop iterations.

use anyhow::Result;
use cmakedb_db::Db;

use crate::{event_in_loop, event_in_source, event_span, Finding, Pass, PassConfig, Severity};

pub struct Clobbers;

impl Pass for Clobbers {
    fn id(&self) -> &'static str {
        "clobbers"
    }
    fn description(&self) -> &'static str {
        "incompatible multi-writes: a value overwritten before anything read it"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();
        // Consecutive same-scope same-name 'set' writes with differing,
        // known values and no read resolving to the first write.
        let mut stmt = db.conn.prepare(
            "SELECT w1.id, w1.event_id, w1.name, w1.value, w2.event_id, w2.value
             FROM var_writes w1
             JOIN var_writes w2 ON w2.id = (
               SELECT min(wx.id) FROM var_writes wx
               WHERE wx.name = w1.name AND wx.scope_id = w1.scope_id
                 AND wx.id > w1.id AND wx.write_kind IN ('set','cache'))
             WHERE w1.write_kind = 'set' AND w2.write_kind IN ('set','cache')
               AND w1.value IS NOT NULL AND w2.value IS NOT NULL
               AND w1.value != w2.value
               AND NOT EXISTS (SELECT 1 FROM var_reads r WHERE r.resolved_write_id = w1.id)
             ORDER BY w1.id",
        )?;
        let rows: Vec<(i64, i64, String, String, i64, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;

        for (_w1, e1, name, v1, e2, v2) in rows {
            if cfg.ignored(&name) {
                continue;
            }
            // Only report clobbers where the *overwrite* happens in project
            // code; the first write may be anywhere (e.g. a default from a
            // CMake module being overridden is interesting).
            if !event_in_source(db, e2) || !event_in_source(db, e1) {
                continue;
            }
            // A foreach/while rewriting its variable is not a clobber, and
            // the same AST node executing twice is re-execution, not a
            // second write site (§4.2).
            let same_node: i64 = db.conn.query_row(
                "SELECT count(*) FROM events a, events b
                 WHERE a.id = ?1 AND b.id = ?2
                   AND a.node_id IS NOT NULL AND a.node_id = b.node_id",
                [e1, e2],
                |r| r.get(0),
            )?;
            if same_node > 0 || event_in_loop(db, e1) || event_in_loop(db, e2) {
                continue;
            }
            let s1 = event_span(db, e1);
            let s2 = event_span(db, e2);
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Warning,
                message: format!(
                    "'{name}' set to '{}' here but overwritten with '{}' before any read",
                    truncate(&v1),
                    truncate(&v2)
                ),
                primary: s1,
                related: vec![(s2, format!("overwritten here with '{}'", truncate(&v2)))],
                fix: None,
            });
        }
        Ok(findings)
    }
}

fn truncate(s: &str) -> String {
    match s.char_indices().nth(60) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}
