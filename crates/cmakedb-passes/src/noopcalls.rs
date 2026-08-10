//! `noop-calls` (roadmap Track F, variable simplification): executed
//! effect-bearing commands whose argument list beyond the fixed head was
//! empty in **every** recorded evaluation — `target_link_libraries(x
//! ${EXTRA_LIBS})` where the list expanded to nothing, or a literally
//! argument-less `add_definitions()`.
//!
//! Trace-format facts this pass leans on (AGENTS.md): json-v1 args are
//! *per-source-argument* expansions — an unquoted argument that expanded
//! to nothing appears as `''` but the real command never received it,
//! while a quoted `""` IS a real (empty) argument. The joined AST's
//! argument kinds tell the two apart; when the source/trace argument
//! counts don't line up 1:1 (splice-y edge cases) we fall back to
//! treating `''` as never-real, which is right far more often. A quoted
//! empty argument therefore always makes the call non-empty and is never
//! flagged — writing `""` (or `"${X}"`) is a deliberate request to pass
//! an empty string.
//!
//! Report-only, note severity by design: the recorded configuration(s)
//! prove the emptiness, but the variable may be populated under other
//! presets/platforms (B1) — see the user-guide entry for the FP profile.

use anyhow::Result;
use cmakedb_db::Db;
use cmakedb_syntax::ArgKind;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::{event_in_try_compile, event_span, Finding, Pass, PassConfig, Severity};

pub struct NoopCalls;

const NO_KEYWORDS: &[&str] = &[];
/// Scope keywords of the target_* family. `BEFORE`/`SYSTEM` are modifier
/// keywords with no effect of their own when no items follow.
const TGT_KEYWORDS: &[&str] = &["PRIVATE", "PUBLIC", "INTERFACE", "BEFORE", "SYSTEM"];
/// Placement keywords of `include_directories` — same reasoning.
const INCDIR_KEYWORDS: &[&str] = &["AFTER", "BEFORE", "SYSTEM"];

/// Fixed head shape: (number of leading positional args that are not
/// payload, keywords ignored when counting payload). `None` = not a
/// command this pass understands.
fn head_spec(cmd: &str) -> Option<(usize, &'static [&'static str])> {
    match cmd {
        "target_link_libraries"
        | "target_include_directories"
        | "target_compile_definitions"
        | "target_compile_options"
        | "target_sources"
        | "target_link_options" => Some((1, TGT_KEYWORDS)),
        "include_directories" => Some((0, INCDIR_KEYWORDS)),
        "add_definitions"
        | "add_compile_definitions"
        | "add_compile_options"
        | "link_libraries" => Some((0, NO_KEYWORDS)),
        // list(APPEND V …) / list(PREPEND V …): subcommand + list name.
        "list" => Some((2, NO_KEYWORDS)),
        _ => None,
    }
}

/// Number of real payload arguments beyond the fixed head in one
/// evaluation, or `None` when the call doesn't match the shape this pass
/// handles (e.g. a `list()` subcommand other than APPEND/PREPEND, or a
/// missing head). An expanded `''` counts only when the source argument
/// was quoted/bracketed (see module docs).
fn payload_count(cmd: &str, args: &[String], kinds: Option<&[ArgKind]>) -> Option<usize> {
    let (head, keywords) = head_spec(cmd)?;
    if cmd == "list" && !matches!(args.first().map(|s| s.as_str()), Some("APPEND" | "PREPEND")) {
        return None;
    }
    if args.len() < head {
        return None;
    }
    let mut n = 0usize;
    for (i, a) in args.iter().enumerate().skip(head) {
        if keywords.contains(&a.as_str()) {
            continue;
        }
        let real = !a.is_empty()
            || matches!(
                kinds.and_then(|k| k.get(i)),
                Some(ArgKind::Quoted | ArgKind::Bracket)
            );
        if real {
            n += 1;
        }
    }
    Some(n)
}

/// Parsed project file + byte-offset → command-invocation index, so a
/// db node row can be matched back to its source argument kinds. Parsing
/// recorded `files.content` keeps the pass a pure function over the db.
struct FileIndex {
    parsed: cmakedb_syntax::ParsedFile,
    by_byte: HashMap<usize, usize>,
}

fn file_index(db: &Db, cache: &mut HashMap<i64, Option<FileIndex>>, file_id: i64) {
    if cache.contains_key(&file_id) {
        return;
    }
    let row: Option<(String, Option<String>)> = db
        .conn
        .query_row(
            "SELECT path, content FROM files WHERE id = ?1",
            [file_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let idx = row.and_then(|(path, content)| {
        let parsed = cmakedb_syntax::parse_source(PathBuf::from(path), content?).ok()?;
        let by_byte = parsed
            .commands
            .iter()
            .enumerate()
            .map(|(i, c)| (parsed.nodes[c.node].byte_start, i))
            .collect();
        Some(FileIndex { parsed, by_byte })
    });
    cache.insert(file_id, idx);
}

impl Pass for NoopCalls {
    fn id(&self) -> &'static str {
        "noop-calls"
    }
    fn description(&self) -> &'static str {
        "commands whose arguments expanded to nothing in every evaluation"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        // Every evaluation of the candidate commands in project files,
        // with the joined node (payload analysis needs the source shape).
        let mut stmt = db.conn.prepare(
            "SELECT e.id, e.node_id, c.name, e.cmd_lower, e.args_json, c.args_text,
                    n.file_id, n.byte_start
             FROM events e
             JOIN ast_nodes n ON n.id = e.node_id
             JOIN commands c ON c.node_id = e.node_id
             JOIN files f ON f.id = e.file_id
             WHERE f.in_source = 1
               AND e.cmd_lower IN ('target_link_libraries', 'target_include_directories',
                                   'target_compile_definitions', 'target_compile_options',
                                   'target_sources', 'target_link_options',
                                   'include_directories', 'add_definitions',
                                   'add_compile_definitions', 'add_compile_options',
                                   'link_libraries', 'list')
             ORDER BY e.id",
        )?;
        let rows: Vec<(i64, i64, String, String, String, String, i64, i64)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;

        struct Group {
            first_event: i64,
            cmd_name: String,
            cmd_lower: String,
            args_text: String,
            file_id: i64,
            byte_start: i64,
            evals: Vec<Vec<String>>,
        }
        let mut order: Vec<i64> = Vec::new();
        let mut groups: HashMap<i64, Group> = HashMap::new();
        for (eid, node, cmd_name, cmd_lower, args_json, args_text, file_id, byte_start) in rows {
            // try_compile scratch configures are an isolated world
            // (AGENTS.md); their events must not be linted.
            if event_in_try_compile(db, eid) {
                continue;
            }
            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            match groups.entry(node) {
                std::collections::hash_map::Entry::Occupied(mut o) => o.get_mut().evals.push(args),
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(Group {
                        first_event: eid,
                        cmd_name,
                        cmd_lower,
                        args_text,
                        file_id,
                        byte_start,
                        evals: vec![args],
                    });
                    order.push(node);
                }
            }
        }

        let mut parse_cache: HashMap<i64, Option<FileIndex>> = HashMap::new();
        let mut findings = Vec::new();
        for node in order {
            let g = &groups[&node];
            file_index(db, &mut parse_cache, g.file_id);
            let kinds: Option<Vec<ArgKind>> = parse_cache
                .get(&g.file_id)
                .and_then(|o| o.as_ref())
                .and_then(|idx| {
                    let ci = *idx.by_byte.get(&(g.byte_start as usize))?;
                    Some(
                        idx.parsed.commands[ci]
                            .args
                            .iter()
                            .map(|a| a.kind)
                            .collect(),
                    )
                });

            // Every evaluation must be payload-free. A node inside a loop
            // where SOME iteration had real arguments is doing its job;
            // all-iterations-empty is still a no-op (and the count
            // strengthens the finding).
            let mut all_empty = true;
            let mut shape_ok = true;
            for args in &g.evals {
                // Kinds are only usable when source and trace argument
                // counts line up 1:1 (same rule as ingestion).
                let k = kinds
                    .as_deref()
                    .and_then(|k| (k.len() == args.len()).then_some(k));
                match payload_count(&g.cmd_lower, args, k) {
                    None => {
                        shape_ok = false;
                        break;
                    }
                    Some(0) => {}
                    Some(_) => {
                        all_empty = false;
                        break;
                    }
                }
            }
            if !shape_ok || !all_empty {
                continue;
            }

            // Belt-and-braces effect cross-check: nothing in the semantic
            // model may point back at these events. Skipped for list():
            // an empty APPEND still records a (value-preserving) write of
            // the list variable — the empty-items check above is the
            // authority there.
            if g.cmd_lower != "list" {
                let effects: i64 = db.conn.query_row(
                    "SELECT (SELECT count(*) FROM tgt_edges t WHERE t.origin_event IN
                              (SELECT id FROM events WHERE node_id = ?1))
                          + (SELECT count(*) FROM usage_reqs u WHERE u.origin_event IN
                              (SELECT id FROM events WHERE node_id = ?1))
                          + (SELECT count(*) FROM tgt_props p WHERE p.origin_event IN
                              (SELECT id FROM events WHERE node_id = ?1))",
                    [node],
                    |r| r.get(0),
                )?;
                if effects > 0 {
                    continue;
                }
            }

            let ref_names: Vec<String> = {
                let mut s = db.conn.prepare(
                    "SELECT DISTINCT name FROM ast_var_refs WHERE node_id = ?1 ORDER BY name",
                )?;
                let v = s
                    .query_map([node], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                v
            };
            let has_refs = !ref_names.is_empty() || g.args_text.contains("${");
            let call = render_call(&g.cmd_name, &g.args_text);
            let evals = g.evals.len();

            if !has_refs {
                // Literally-empty call: nothing beyond the head in the
                // source either (a genex or literal would have survived
                // expansion and failed the all-empty check above).
                if g.cmd_lower == "list" {
                    let list_name = g.evals[0].get(1).cloned().unwrap_or_default();
                    if cfg.ignored(&list_name) {
                        continue;
                    }
                    findings.push(finding(
                        self.id(),
                        db,
                        g.first_event,
                        format!("{call} adds no items — remove it"),
                    ));
                } else if head_spec(&g.cmd_lower).map(|(h, _)| h) == Some(1) {
                    findings.push(finding(
                        self.id(),
                        db,
                        g.first_event,
                        format!("{call} has no arguments beyond the target — remove it"),
                    ));
                } else {
                    findings.push(finding(
                        self.id(),
                        db,
                        g.first_event,
                        format!("{call} has no arguments — remove it"),
                    ));
                }
                continue;
            }

            // Which referenced variables actually expanded to nothing at
            // this node: no read at these events resolved to a non-empty
            // value (head refs like a `${TGT}` target name resolve
            // non-empty and drop out here).
            let mut empty_refs: Vec<String> = Vec::new();
            for name in &ref_names {
                let nonempty: i64 = db.conn.query_row(
                    "SELECT count(*) FROM var_reads r
                     JOIN events e ON e.id = r.event_id
                     JOIN var_writes w ON w.id = r.resolved_write_id
                     WHERE e.node_id = ?1 AND r.name = ?2
                       AND w.value IS NOT NULL AND w.value != ''",
                    rusqlite::params![node, name],
                    |r| r.get(0),
                )?;
                if nonempty == 0 {
                    empty_refs.push(name.clone());
                }
            }
            if g.cmd_lower == "list" {
                let list_name = g.evals[0].get(1).cloned().unwrap_or_default();
                if cfg.ignored(&list_name) {
                    continue;
                }
            }
            let kept: Vec<&String> = empty_refs.iter().filter(|n| !cfg.ignored(n)).collect();
            if !empty_refs.is_empty() && kept.is_empty() {
                continue;
            }
            let what = if kept.is_empty() {
                "its variable arguments".to_string()
            } else {
                kept.iter()
                    .map(|n| format!("${{{n}}}"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            };
            let notes: Vec<String> = kept
                .iter()
                .take(2)
                .filter_map(|n| write_note(db, n).transpose())
                .collect::<Result<_>>()?;
            let note = if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join("; "))
            };
            findings.push(finding(
                self.id(),
                db,
                g.first_event,
                format!(
                    "{call} was a no-op in all {evals} evaluation(s) — {what} expanded to \
                 nothing{note}"
                ),
            ));
        }
        Ok(findings)
    }
}

fn finding(rule: &str, db: &Db, event: i64, message: String) -> Finding {
    Finding {
        rule: rule.into(),
        severity: Severity::Note,
        message,
        primary: event_span(db, event),
        related: vec![],
        fix: None,
    }
}

/// `cmd(args…)` as written, with long argument text truncated.
fn render_call(cmd: &str, args_text: &str) -> String {
    let mut a = args_text.trim().to_string();
    if a.chars().count() > 80 {
        a = a.chars().take(77).collect::<String>() + "…";
    }
    format!("{cmd}({a})")
}

/// Why the variable was empty, when the recording can tell: never written
/// at all vs written only by code that never executed (the same
/// unexecuted-write-target probe as `single-use-variables`, which catches
/// bare write-target names `ast_var_refs` cannot see).
fn write_note(db: &Db, name: &str) -> Result<Option<String>> {
    let executed: i64 = db.conn.query_row(
        "SELECT count(*) FROM var_writes WHERE name = ?1",
        [name],
        |r| r.get(0),
    )?;
    if executed > 0 {
        return Ok(None);
    }
    let unexecuted: i64 = db.conn.query_row(
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
    Ok(Some(if unexecuted > 0 {
        format!("{name} is populated only in code that never ran (B1)")
    } else {
        format!("{name} is never set — see undefined-reads")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn payload_counting() {
        use ArgKind::*;
        // Unquoted empty expansion was never a real argument.
        assert_eq!(
            payload_count(
                "target_link_libraries",
                &s(&["app", ""]),
                Some(&[Unquoted, Unquoted])
            ),
            Some(0)
        );
        // Quoted "" IS a real argument.
        assert_eq!(
            payload_count(
                "target_compile_definitions",
                &s(&["app", "PRIVATE", ""]),
                Some(&[Unquoted, Unquoted, Quoted])
            ),
            Some(1)
        );
        // Without kinds, '' is treated as never-real (the safer default).
        assert_eq!(
            payload_count("target_link_libraries", &s(&["app", "PRIVATE", ""]), None),
            Some(0)
        );
        // Keywords are head, items are payload.
        assert_eq!(
            payload_count("target_link_libraries", &s(&["app", "PRIVATE", "m"]), None),
            Some(1)
        );
        assert_eq!(payload_count("add_definitions", &s(&[]), None), Some(0));
        assert_eq!(
            payload_count("list", &s(&["APPEND", "FLAGS"]), None),
            Some(0)
        );
        assert_eq!(
            payload_count("list", &s(&["APPEND", "FLAGS", ""]), None),
            Some(0)
        );
        assert_eq!(
            payload_count("list", &s(&["APPEND", "FLAGS", "x"]), None),
            Some(1)
        );
        // Non-APPEND/PREPEND list subcommands are out of scope.
        assert_eq!(
            payload_count("list", &s(&["LENGTH", "FLAGS", "out"]), None),
            None
        );
        assert_eq!(
            payload_count("include_directories", &s(&["SYSTEM", ""]), None),
            Some(0)
        );
    }
}
