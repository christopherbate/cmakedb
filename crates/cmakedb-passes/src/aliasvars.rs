//! `alias-variables` (roadmap Track F, variable simplification): variables
//! that only pass another variable's value through unchanged.
//!
//! Two shapes, both report-only at note severity:
//!
//! - **Pure alias**: a project-file `set(X ${Y})` whose raw argument text
//!   is exactly the name plus one `${Y}` reference, where X is written
//!   nowhere else, every read of X resolved to that write, and Y was never
//!   rewritten after the alias event — every read of X observed exactly
//!   Y's value, so the reads can use Y directly.
//! - **PARENT_SCOPE round-trip** (narrow, defensible subshape): a
//!   `set(V ${V} PARENT_SCOPE)` self-assignment inside a function body
//!   whose `${V}` read resolved to a write *outside* the function — the
//!   call wrote back the unchanged inherited value, a no-op.
//!
//! Both claims are proven only for the recorded configuration(s); the B1
//! cross-checks below (unexecuted static references, unexecuted write
//! targets — the idioms validated on LLVM by the sibling `singleuse`
//! pass) suppress the branches this recording didn't take, but
//! divergence in an entirely different preset remains a documented FP
//! class (user guide: matrix/--also intersection).

use anyhow::Result;
use cmakedb_db::Db;
use rusqlite::params;

use crate::singleuse::event_in_function_or_macro;
use crate::{
    event_in_loop, event_in_source, event_in_try_compile, event_span, Finding, Pass, PassConfig,
    Severity,
};

pub struct AliasVariables;

impl Pass for AliasVariables {
    fn id(&self) -> &'static str {
        "alias-variables"
    }
    fn description(&self) -> &'static str {
        "pass-through variables: pure aliases and PARENT_SCOPE round-trips"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut findings = pure_aliases(db, cfg, self.id())?;
        findings.extend(parent_scope_round_trips(db, cfg, self.id())?);
        Ok(findings)
    }
}

/// Shape 1: `set(X ${Y})` where X never diverges from Y.
fn pure_aliases(db: &Db, cfg: &PassConfig, rule: &str) -> Result<Vec<Finding>> {
    // Candidates: a plain set() in a project file that is the name's ONLY
    // write in the recording. The alias *shape* is checked statically
    // below against the raw AST argument text.
    let mut stmt = db.conn.prepare(
        "SELECT w.id, w.name, w.event_id, e.node_id, c.args_text
         FROM var_writes w
         JOIN events e ON e.id = w.event_id
         JOIN files f ON f.id = e.file_id
         JOIN commands c ON c.node_id = e.node_id
         WHERE w.write_kind = 'set'
           AND e.cmd_lower = 'set'
           AND f.in_source = 1
           AND (SELECT count(*) FROM var_writes w2 WHERE w2.name = w.name) = 1
         ORDER BY w.event_id",
    )?;
    let rows: Vec<(i64, String, i64, i64, String)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut findings = Vec::new();
    for (write_id, name, write_event, node_id, args_text) in rows {
        if name.starts_with("CMAKE_") || name.starts_with("_CMAKE_") || cfg.ignored(&name) {
            continue;
        }
        // Static shape check: exactly one variable reference in the whole
        // command, and the raw argument text is exactly `X ${Y}` (or the
        // quoted spelling) — nothing appended, no extra args, no
        // CACHE/PARENT_SCOPE tail. `set(X ${Y}extra)` and friends fail here.
        let Some(source) = single_ref_of(db, node_id)? else {
            continue;
        };
        if source == name || !is_exact_alias_args(&args_text, &name, &source) {
            continue;
        }
        // Every read of X must resolve to this write via expansion (an
        // unresolved read-before-write saw a different value than Y's),
        // and at least one read must exist — a never-read alias is
        // dead-variables territory, not reported twice.
        let (total_reads, alias_reads): (i64, i64) = db.conn.query_row(
            "SELECT count(*),
                    sum(CASE WHEN r.resolved_write_id = ?2 THEN 1 ELSE 0 END)
             FROM var_reads r WHERE r.name = ?1",
            params![name, write_id],
            |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        )?;
        if total_reads == 0 || alias_reads != total_reads {
            continue;
        }
        // Divergence guard, deliberately conservative: ANY executed write
        // of Y after the alias event disqualifies, even one past the last
        // read — proving "between" precisely buys little over this.
        let y_rewrites: i64 = db.conn.query_row(
            "SELECT count(*) FROM var_writes wy
             WHERE wy.name = ?1 AND wy.event_id > ?2",
            params![source, write_event],
            |r| r.get(0),
        )?;
        if y_rewrites > 0 {
            continue;
        }
        if event_in_loop(db, write_event)
            || event_in_try_compile(db, write_event)
            || event_in_function_or_macro(db, write_event)
        {
            continue;
        }
        // Read-site guards, mirroring singleuse: the advice rewrites the
        // read sites, so each must be project code outside try_compile
        // scratch and outside function/macro bodies (where visibility of Y
        // is a dynamic-scoping question this pass doesn't answer).
        let mut reads_stmt = db.conn.prepare(
            "SELECT DISTINCT r.event_id FROM var_reads r
             WHERE r.resolved_write_id = ?1 ORDER BY r.event_id",
        )?;
        let read_events: Vec<i64> = reads_stmt
            .query_map([write_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if read_events.iter().any(|&e| {
            !event_in_source(db, e)
                || event_in_try_compile(db, e)
                || event_in_function_or_macro(db, e)
        }) {
            continue;
        }
        // B1 guard (unexecuted reads): more static project-file reference
        // nodes for X than the executed reads touched means an untaken
        // branch also references it.
        if has_unexecuted_refs(db, &name)? {
            continue;
        }
        // B1 guard (unexecuted writes), for both ends of the alias: an
        // untaken branch that could rewrite X breaks the only-write claim;
        // one that could rewrite Y breaks "never diverging" in the config
        // that takes it (the conditional-default idiom — the dominant raw
        // FP class in the singleuse LLVM validation).
        if has_unexecuted_writers(db, &name)? || has_unexecuted_writers(db, &source)? {
            continue;
        }

        let write_span = event_span(db, write_event);
        let related = read_events
            .iter()
            .take(5)
            .map(|&e| {
                (
                    event_span(db, e),
                    format!("read of {name} — use {source} here"),
                )
            })
            .collect();
        findings.push(Finding {
            rule: rule.into(),
            severity: Severity::Note,
            message: format!(
                "{name} is a pure alias of {source} (set at {write_span} and never \
                 diverging) — use {source} directly"
            ),
            primary: write_span,
            related,
            fix: None,
        });
    }
    Ok(findings)
}

/// Shape 2: `set(V ${V} PARENT_SCOPE)` writing back an inherited value.
fn parent_scope_round_trips(db: &Db, cfg: &PassConfig, rule: &str) -> Result<Vec<Finding>> {
    let mut stmt = db.conn.prepare(
        "SELECT w.name, w.event_id, e.node_id, c.args_text
         FROM var_writes w
         JOIN events e ON e.id = w.event_id
         JOIN files f ON f.id = e.file_id
         JOIN commands c ON c.node_id = e.node_id
         WHERE w.write_kind = 'parent_scope'
           AND e.cmd_lower = 'set'
           AND f.in_source = 1
         ORDER BY w.event_id",
    )?;
    let rows: Vec<(String, i64, i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;

    // One AST site can execute many times (one row per invocation); the
    // finding is per site, and EVERY invocation must be a proven no-op.
    let mut by_node: std::collections::BTreeMap<i64, Vec<(String, i64, String)>> =
        std::collections::BTreeMap::new();
    for (name, event_id, node_id, args_text) in rows {
        by_node
            .entry(node_id)
            .or_default()
            .push((name, event_id, args_text));
    }

    let mut findings = Vec::new();
    'nodes: for (node_id, instances) in by_node {
        let (name, first_event, args_text) = instances[0].clone();
        if name.starts_with("CMAKE_") || name.starts_with("_CMAKE_") || cfg.ignored(&name) {
            continue;
        }
        // Static self-assignment shape: exactly `V ${V} PARENT_SCOPE`.
        match single_ref_of(db, node_id)? {
            Some(r) if r == name => {}
            _ => continue,
        }
        if !is_exact_roundtrip_args(&args_text, &name) {
            continue;
        }
        // An untaken branch that could write V (typically inside the same
        // function: `if(cond) set(V changed) endif()` before the export)
        // means the round-trip is load-bearing in another configuration.
        if has_unexecuted_writers(db, &name)? {
            continue;
        }
        let mut inherited_write: Option<i64> = None;
        for (_, event_id, _) in &instances {
            if event_in_try_compile(db, *event_id) || event_in_loop(db, *event_id) {
                continue 'nodes;
            }
            // The write must sit in a function body, and the `${V}` read at
            // this very event must have resolved to a write OUTSIDE that
            // function invocation — i.e. the inherited value. A local write
            // (including synthesized parameters) makes this a real export.
            let Some(func_scope) = enclosing_function_scope(db, *event_id) else {
                continue 'nodes;
            };
            let resolved: Option<i64> = db
                .conn
                .query_row(
                    "SELECT r.resolved_write_id FROM var_reads r
                     WHERE r.event_id = ?1 AND r.name = ?2
                       AND r.read_kind IN ('expand', 'listarg')",
                    params![event_id, name],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            let Some(resolved_write) = resolved else {
                // Unresolved: V was undefined in the parent, so this write
                // *defines* it there — not a no-op.
                continue 'nodes;
            };
            let write_scope: Option<i64> = db
                .conn
                .query_row(
                    "SELECT scope_id FROM var_writes WHERE id = ?1",
                    [resolved_write],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            if write_scope.is_none_or(|s| scope_within(db, s, func_scope)) {
                continue 'nodes;
            }
            inherited_write.get_or_insert(resolved_write);
        }

        let mut related = Vec::new();
        if let Some(w) = inherited_write {
            let ev: Option<i64> = db
                .conn
                .query_row("SELECT event_id FROM var_writes WHERE id = ?1", [w], |r| {
                    r.get(0)
                })
                .ok();
            if let Some(ev) = ev {
                related.push((
                    event_span(db, ev),
                    format!("the value written back is {name} as inherited from here"),
                ));
            }
        }
        findings.push(Finding {
            rule: rule.into(),
            severity: Severity::Note,
            message: format!(
                "set({name} ${{{name}}} PARENT_SCOPE) writes back the unchanged \
                 inherited value — remove it"
            ),
            primary: event_span(db, first_event),
            related,
            fix: None,
        });
    }
    Ok(findings)
}

/// The single static variable reference of a command node, if the node
/// references exactly one (occurrence, not distinct name — `${Y}${Z}` and
/// `${Y}${Y}` both fail the pure-alias shape).
fn single_ref_of(db: &Db, node_id: i64) -> Result<Option<String>> {
    let mut stmt = db
        .conn
        .prepare("SELECT name FROM ast_var_refs WHERE node_id = ?1 LIMIT 2")?;
    let refs: Vec<String> = stmt
        .query_map([node_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(match refs.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    })
}

/// Raw argument text is exactly `X ${Y}` (or the quoted-value spelling) —
/// `commands.args_text` joins the raw per-argument texts with single
/// spaces, so exact string equality is a whole-shape check.
fn is_exact_alias_args(args_text: &str, x: &str, y: &str) -> bool {
    args_text == format!("{x} ${{{y}}}") || args_text == format!("{x} \"${{{y}}}\"")
}

/// Raw argument text is exactly `V ${V} PARENT_SCOPE` (or quoted value).
fn is_exact_roundtrip_args(args_text: &str, v: &str) -> bool {
    args_text == format!("{v} ${{{v}}} PARENT_SCOPE")
        || args_text == format!("{v} \"${{{v}}}\" PARENT_SCOPE")
}

/// B1 guard shared by both shapes: static project-file reference nodes
/// the executed reads didn't touch (untaken branches/configurations).
fn has_unexecuted_refs(db: &Db, name: &str) -> Result<bool> {
    let static_refs: i64 = db.conn.query_row(
        "SELECT count(DISTINCT vr.node_id) FROM ast_var_refs vr
         JOIN ast_nodes n ON n.id = vr.node_id
         JOIN files f ON f.id = n.file_id
         WHERE vr.name = ?1 AND f.in_source = 1",
        [name],
        |r| r.get(0),
    )?;
    let executed_read_nodes: i64 = db.conn.query_row(
        "SELECT count(DISTINCT e.node_id) FROM var_reads r
         JOIN events e ON e.id = r.event_id
         WHERE r.name = ?1 AND e.node_id IS NOT NULL",
        [name],
        |r| r.get(0),
    )?;
    Ok(static_refs > executed_read_nodes)
}

/// Unexecuted project-file commands that could write `name` (bare write
/// targets are invisible to `ast_var_refs`) — the conditional-default
/// idiom guard from the singleuse LLVM validation, same conservative
/// matching.
fn has_unexecuted_writers(db: &Db, name: &str) -> Result<bool> {
    let n: i64 = db.conn.query_row(
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
        [name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Nearest `function` scope on the event's scope chain (macro scopes are
/// transparent and skipped — a PARENT_SCOPE write inside a macro acts in
/// its caller's function context).
fn enclosing_function_scope(db: &Db, event_id: i64) -> Option<i64> {
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
        let sid = scope?;
        let (kind, parent): (String, Option<i64>) = db
            .conn
            .query_row(
                "SELECT kind, parent_id FROM scopes WHERE id = ?1",
                [sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        if kind == "function" {
            return Some(sid);
        }
        scope = parent;
    }
    None
}

/// Whether `scope` is `ancestor` or sits below it in the scope tree.
fn scope_within(db: &Db, scope: i64, ancestor: i64) -> bool {
    let mut cur = Some(scope);
    for _ in 0..64 {
        let Some(sid) = cur else { return false };
        if sid == ancestor {
            return true;
        }
        cur = db
            .conn
            .query_row("SELECT parent_id FROM scopes WHERE id = ?1", [sid], |r| {
                r.get(0)
            })
            .ok()
            .flatten();
    }
    false
}
