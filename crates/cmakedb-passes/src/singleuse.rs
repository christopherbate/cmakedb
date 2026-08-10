//! `single-use-variables` (roadmap Track F, variable simplification): a
//! plain `set(NAME value)` in project code whose variable is written
//! exactly once and read exactly once via `${}` expansion — a candidate
//! for inlining the value at the sole use site.
//!
//! Report-only, note severity by design: the recorded configuration
//! proves the counts, but a variable can exist for readability or for
//! configurations this recording didn't take. The key FP guard (B1,
//! other configurations) is a static cross-check: if `ast_var_refs`
//! contains more distinct project-file reference nodes for the name than
//! the executed reads touched, some branch/config this recording didn't
//! execute also references it — skipped.
//!
//! Deliberately out of scope (all idiomatic, not simplification targets):
//! cache/env/PARENT_SCOPE writes, writes inside loops or try_compile
//! scratch scopes, and writes *or reads* whose scope chain passes through
//! a function/macro body (params, locals, and set-before-call dynamic
//! scoping are how CMake functions communicate).

use anyhow::Result;
use cmakedb_db::Db;

use crate::{
    event_in_loop, event_in_source, event_in_try_compile, event_span, Finding, Pass, PassConfig,
    Severity,
};

pub struct SingleUseVariables;

impl Pass for SingleUseVariables {
    fn id(&self) -> &'static str {
        "single-use-variables"
    }
    fn description(&self) -> &'static str {
        "variables set once and read once — candidates for inlining"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        // Candidates: a literal set() in a project file, the name's ONLY
        // write of any kind in the recording (excludes append/build-up
        // patterns and function-parameter synthesis for the same name),
        // with exactly one read of the name anywhere — and that read is a
        // ${} expansion resolving to this write. A bare `if(NAME)`
        // condition read or an unresolved read elsewhere disqualifies.
        let mut stmt = db.conn.prepare(
            "SELECT w.id, w.name, w.event_id,
                    (SELECT r.event_id FROM var_reads r
                     WHERE r.resolved_write_id = w.id)
             FROM var_writes w
             JOIN events e ON e.id = w.event_id
             JOIN files f ON f.id = e.file_id
             WHERE w.write_kind = 'set'
               AND e.cmd_lower = 'set'
               AND f.in_source = 1
               AND (SELECT count(*) FROM var_writes w2 WHERE w2.name = w.name) = 1
               AND (SELECT count(*) FROM var_reads r WHERE r.name = w.name) = 1
               AND (SELECT count(*) FROM var_reads r
                    WHERE r.resolved_write_id = w.id
                      AND r.read_kind IN ('expand', 'listarg')) = 1
             ORDER BY w.event_id",
        )?;
        let rows: Vec<(i64, String, i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut findings = Vec::new();
        for (_write_id, name, write_event, read_event) in rows {
            // Public/API-shaped names are never inlining candidates; the
            // CMAKE_ namespace is skipped even without config (behavior
            // flows through CMake itself, not a visible read).
            if name.starts_with("CMAKE_") || name.starts_with("_CMAKE_") || cfg.ignored(&name) {
                continue;
            }
            // The read must be project code too: a value consumed inside a
            // CMake module (find_package inputs etc.) cannot be inlined.
            if !event_in_source(db, read_event) {
                continue;
            }
            if event_in_loop(db, write_event)
                || event_in_try_compile(db, write_event)
                || event_in_try_compile(db, read_event)
            {
                continue;
            }
            // Function/macro bodies are idiomatic variable territory —
            // params, locals, and set-before-call communication. Checked on
            // both ends of the pair.
            if event_in_function_or_macro(db, write_event)
                || event_in_function_or_macro(db, read_event)
            {
                continue;
            }
            // B1 guard: static references in project files that the
            // executed reads didn't touch (other branches/configurations,
            // `if(DEFINED NAME)` probes, ...) mean the variable has more
            // uses than this recording observed.
            let static_refs: i64 = db.conn.query_row(
                "SELECT count(DISTINCT vr.node_id) FROM ast_var_refs vr
                 JOIN ast_nodes n ON n.id = vr.node_id
                 JOIN files f ON f.id = n.file_id
                 WHERE vr.name = ?1 AND f.in_source = 1",
                [&name],
                |r| r.get(0),
            )?;
            let executed_read_nodes: i64 = db.conn.query_row(
                "SELECT count(DISTINCT e.node_id) FROM var_reads r
                 JOIN events e ON e.id = r.event_id
                 WHERE r.name = ?1 AND e.node_id IS NOT NULL",
                [&name],
                |r| r.get(0),
            )?;
            if static_refs > executed_read_nodes {
                continue;
            }
            // Conditional-default idiom (found by LLVM validation, the
            // dominant raw-FP class there): `set(X_default OFF)` +
            // `set(X_default ON)` in a branch this configuration didn't
            // take. Bare write-target names are not var refs, so the guard
            // above can't see them — check for unexecuted project-file
            // commands that could write the name (set/option by first
            // argument; list/string and friends by whole-token mention,
            // conservatively).
            let unexecuted_writers: i64 = db.conn.query_row(
                "SELECT count(*) FROM commands c
                 JOIN ast_nodes n ON n.id = c.node_id
                 JOIN files f ON f.id = n.file_id
                 WHERE f.in_source = 1
                   AND NOT EXISTS (SELECT 1 FROM events e WHERE e.node_id = c.node_id)
                   AND ((c.name_lower IN ('set', 'option')
                         AND (c.args_text = ?1 OR c.args_text LIKE ?1 || ' %'
                              OR c.args_text LIKE '\"' || ?1 || '\" %'))
                     OR (c.name_lower IN ('list', 'string', 'separate_arguments',
                                          'cmake_parse_arguments', 'unset')
                         AND (c.args_text = ?1 OR c.args_text LIKE ?1 || ' %'
                              OR c.args_text LIKE '% ' || ?1 || ' %'
                              OR c.args_text LIKE '% ' || ?1)))",
                [&name],
                |r| r.get(0),
            )?;
            if unexecuted_writers > 0 {
                continue;
            }

            let read_span = event_span(db, read_event);
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Note,
                message: format!(
                    "{name} is set once and read once (at {read_span}) — \
                     consider inlining the value at the use site"
                ),
                primary: event_span(db, write_event),
                related: vec![(
                    read_span,
                    "sole read — the value would be inlined here".into(),
                )],
                fix: None,
            });
        }
        Ok(findings)
    }
}

/// Whether the event's scope chain passes through a function or macro
/// body. Macro scopes are transparent (no own variable table) but still
/// appear in the chain, which is exactly what we want: text inside a
/// macro body is reusable-definition territory regardless of where its
/// writes land. Shared with `aliasvars` (same guard rationale).
pub(crate) fn event_in_function_or_macro(db: &Db, event_id: i64) -> bool {
    let mut scope: Option<i64> = db
        .conn
        .query_row(
            "SELECT scope_id FROM events WHERE id = ?1",
            [event_id],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    for _ in 0..64 {
        let Some(sid) = scope else { return false };
        let row: Option<(String, Option<i64>)> = db
            .conn
            .query_row(
                "SELECT kind, parent_id FROM scopes WHERE id = ?1",
                [sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((kind, parent)) = row else {
            return false;
        };
        if kind == "function" || kind == "macro" {
            return true;
        }
        scope = parent;
    }
    false
}
