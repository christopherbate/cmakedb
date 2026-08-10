//! Declaration-ordering passes (lint roadmap Tier 1).
//!
//! `option-after-use`: an `option()` / user-facing `set(... CACHE ...)`
//! declaration whose variable was already *referenced* earlier in the same
//! configure — the earlier reference observed an empty value because the
//! declaration (and its default) didn't exist yet. Event ordering in the
//! trace proves the misorder outright; this is the classic
//! include-order/option-file bug.

use anyhow::Result;
use cmakedb_db::Db;
use rusqlite::params;

use crate::{
    event_in_source, event_in_try_compile, event_span, Finding, Pass, PassConfig, Severity,
};

pub struct OptionAfterUse;

impl Pass for OptionAfterUse {
    fn id(&self) -> &'static str {
        "option-after-use"
    }
    fn description(&self) -> &'static str {
        "option()/cache declarations first referenced before they were declared"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();

        // First project-file declaration event per option-like name
        // (same declaration shape as dead-options).
        let mut stmt = db.conn.prepare(
            "SELECT e.id, e.cmd_lower, e.args_json FROM events e
             JOIN files f ON f.id = e.file_id
             WHERE e.cmd_lower IN ('option', 'set') AND f.in_source = 1
             ORDER BY e.id",
        )?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut seen = std::collections::HashSet::new();
        for (decl_event, cmd, args_json) in rows {
            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            let Some(name) = args.first().cloned() else {
                continue;
            };
            let is_declaration = match cmd.as_str() {
                "option" => true,
                _ => match args.iter().position(|a| a == "CACHE") {
                    Some(pos) => matches!(
                        args.get(pos + 1).map(|s| s.as_str()),
                        Some("BOOL" | "STRING" | "PATH" | "FILEPATH")
                    ),
                    None => false,
                },
            };
            if !is_declaration || cfg.ignored(&name) || !seen.insert(name.clone()) {
                continue;
            }

            // References before the declaration that observed *nothing*
            // (validated against LLVM; three idioms are excluded):
            //  - resolved reads (cache pre-seeded with -D / preset): fine;
            //  - `if(DEFINED N)` probes: they correctly observe
            //    *undefinedness*, not an empty value (condition-kind read
            //    rows exist exactly for these);
            //  - the guard idiom `if(NOT N) <declare N> endif()`: the
            //    early reference's if-block lexically contains the
            //    declaration.
            let mut early = db.conn.prepare(
                "SELECT DISTINCT e.id FROM ast_var_refs vr
                 JOIN events e ON e.node_id = vr.node_id
                 WHERE vr.name = ?1 AND e.id < ?2
                   AND NOT EXISTS (
                     SELECT 1 FROM var_reads r
                     WHERE r.event_id = e.id AND r.name = ?1
                       AND (r.resolved_write_id IS NOT NULL
                            OR r.read_kind = 'condition'))
                 ORDER BY e.id LIMIT 12",
            )?;
            let early_events: Vec<i64> = early
                .query_map(params![name, decl_event], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;

            // try_compile scratch configures are an isolated world, and
            // guard-idiom conditions wrapping the declaration are intent.
            let early_events: Vec<i64> = early_events
                .into_iter()
                .filter(|e| {
                    !event_in_try_compile(db, *e) && !guards_declaration(db, *e, decl_event)
                })
                .collect();
            if early_events.is_empty() {
                continue;
            }
            // Severity: an early `${N}` expansion baked an empty string
            // into a value (real corruption); bare `if(N)` references are
            // fragile ordering but benign while the default is falsy.
            let value_corrupting: i64 = db.conn.query_row(
                "SELECT count(*) FROM var_reads r
                 WHERE r.name = ?1 AND r.read_kind = 'expand'
                   AND r.resolved_write_id IS NULL AND r.event_id < ?2",
                params![name, decl_event],
                |r| r.get(0),
            )?;
            let severity = if value_corrupting > 0 {
                Severity::Warning
            } else {
                Severity::Note
            };
            if !event_in_source(db, decl_event) {
                continue;
            }

            let related = early_events
                .iter()
                .take(5)
                .map(|e| {
                    (
                        event_span(db, *e),
                        "referenced here, before the declaration ran (saw an empty value)"
                            .to_string(),
                    )
                })
                .collect();
            findings.push(Finding {
                rule: self.id().into(),
                severity,
                message: format!(
                    "{name} is declared here but was already referenced {} time(s) \
                     earlier in this configure — the earlier code saw an empty value",
                    early_events.len()
                ),
                primary: event_span(db, decl_event),
                related,
                fix: None,
            });
        }
        Ok(findings)
    }
}

/// Guard idiom check: the early reference sits in an `if`/`elseif` whose
/// surrounding `if_condition` block lexically contains the declaration
/// (`if(NOT X) set(X ... CACHE ...) endif()`), i.e. the reference exists
/// to decide whether to declare — intent, not a bug.
fn guards_declaration(db: &Db, early_event: i64, decl_event: i64) -> bool {
    let node_of = |event: i64| -> Option<(i64, i64)> {
        db.conn
            .query_row(
                "SELECT n.id, n.file_id FROM events e JOIN ast_nodes n ON n.id = e.node_id
                 WHERE e.id = ?1",
                [event],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
    };
    let (Some((early_node, early_file)), Some((decl_node, decl_file))) =
        (node_of(early_event), node_of(decl_event))
    else {
        return false;
    };
    if early_file != decl_file {
        return false;
    }
    let decl_span: Option<(i64, i64)> = db
        .conn
        .query_row(
            "SELECT byte_start, byte_end FROM ast_nodes WHERE id = ?1",
            [decl_node],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let Some((decl_start, decl_end)) = decl_span else {
        return false;
    };
    // Walk the early node's ancestors; any if_condition container that
    // spans the declaration means the declaration is inside this if-chain.
    let mut cur = Some(early_node);
    for _ in 0..64 {
        let Some(id) = cur else { return false };
        let row: Option<(String, i64, i64, Option<i64>)> = db
            .conn
            .query_row(
                "SELECT kind, byte_start, byte_end, parent_id FROM ast_nodes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();
        let Some((kind, start, end, parent)) = row else {
            return false;
        };
        if kind == "if_condition" && start <= decl_start && decl_end <= end {
            return true;
        }
        cur = parent;
    }
    false
}
