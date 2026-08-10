//! Modernization codemods (design §4.6): directory-scope legacy commands
//! rewritten as target-scoped calls, anchored to AST nodes and verified by
//! re-recording (the verification loop itself lives in the recorder layer;
//! this module only *plans* patches — it is a pure function over the db).
//!
//! Engine (§4.6 steps 1–5), shared by all four codemods:
//!  1. find legacy-command events evaluated in a directory-kind scope;
//!  2. affected set = targets defined after the event within that scope's
//!     directory subtree (replaying recorded scope/event order);
//!  3. keep only targets whose *final* File API state actually contains
//!     the value (shadowing check);
//!  4. emit: delete the legacy AST node, insert a target-scoped call after
//!     each affected target's defining node. Visibility is PRIVATE — the
//!     directory command never propagated to consumers, so PRIVATE is the
//!     behavior-preserving translation (run `overshare` for PUBLIC
//!     evidence);
//!  5. patches are byte-anchored against recorded content hashes.

use anyhow::Result;
use cmakedb_db::Db;
use cmakedb_patch::{Edit, Patch};
use rusqlite::params;
use std::collections::HashMap;

use crate::{Finding, Severity, SourceSpan};

pub const ALL_PASSES: &[&str] = &[
    "include-directories",
    "add-definitions",
    "add-compile-options",
    "link-libraries",
];

struct CodemodSpec {
    pass: &'static str,
    legacy_cmd: &'static str,
    target_cmd: &'static str,
    /// usage_reqs.kind for the File API cross-check.
    check_kind: &'static str,
}

const SPECS: &[CodemodSpec] = &[
    CodemodSpec {
        pass: "include-directories",
        legacy_cmd: "include_directories",
        target_cmd: "target_include_directories",
        check_kind: "include",
    },
    CodemodSpec {
        pass: "add-definitions",
        legacy_cmd: "add_definitions",
        target_cmd: "target_compile_definitions",
        check_kind: "define",
    },
    CodemodSpec {
        pass: "add-compile-options",
        legacy_cmd: "add_compile_options",
        target_cmd: "target_compile_options",
        check_kind: "option",
    },
    CodemodSpec {
        pass: "link-libraries",
        legacy_cmd: "link_libraries",
        target_cmd: "target_link_libraries",
        check_kind: "link",
    },
];

/// Plan all patches for the enabled passes (empty `enabled` = all).
pub fn plan(db: &Db, enabled: &[String]) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for spec in SPECS {
        if !enabled.is_empty() && !enabled.iter().any(|e| e == spec.pass) {
            continue;
        }
        findings.extend(plan_spec(db, spec)?);
    }
    Ok(findings)
}

fn plan_spec(db: &Db, spec: &CodemodSpec) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();

    // Legacy events in project files with a joined AST node.
    let mut stmt = db.conn.prepare(
        "SELECT e.id, e.node_id, e.args_json, e.scope_id, f.path, f.content_hash, e.line
         FROM events e JOIN files f ON f.id = e.file_id
         WHERE e.cmd_lower = ?1 AND f.in_source = 1 AND e.node_id IS NOT NULL
         ORDER BY e.id",
    )?;
    let rows: Vec<(i64, i64, String, i64, String, Option<String>, i64)> = stmt
        .query_map([spec.legacy_cmd], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    // Group events by node: a node evaluated multiple times with differing
    // arguments (loops, macros, re-included files) cannot be rewritten
    // safely as a single target-scoped call.
    let mut by_node: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        by_node.entry(r.1).or_default().push(i);
    }

    for (node, idxs) in by_node {
        let (eid, _, ref args_json, scope_id, ref path, ref hash, line) = rows[idxs[0]];
        let span = SourceSpan::new(db.display_path(path), line);
        let mk_note = |msg: String| Finding {
            rule: format!("modernize-{}", spec.pass),
            severity: Severity::Note,
            message: msg,
            primary: span.clone(),
            related: vec![],
            fix: None,
        };

        if idxs.iter().any(|&i| rows[i].2 != *args_json) {
            findings.push(mk_note(format!(
                "{} evaluated multiple times with different arguments — not rewriting \
                 automatically",
                spec.legacy_cmd
            )));
            continue;
        }

        // Must execute in a directory-kind scope (root/directory), not
        // inside a function/macro/block.
        let Some(dir_scope) = effective_dir_scope(db, scope_id)? else {
            findings.push(mk_note(format!(
                "{} executed inside a function/macro — not a directory-scope call, skipped",
                spec.legacy_cmd
            )));
            continue;
        };

        let args: Vec<String> = serde_json::from_str(args_json).unwrap_or_default();
        let Some(values) = extract_values(spec, &args) else {
            findings.push(mk_note(format!(
                "{} carries arguments this codemod does not understand — skipped",
                spec.legacy_cmd
            )));
            continue;
        };
        if values.is_empty() {
            continue;
        }
        // Values used for the File API cross-check and the generated call:
        // include dirs are absolutized against the evaluating directory
        // (relative paths would re-resolve differently in other files).
        let base_dir = scope_source_dir(db, dir_scope)?;
        let values: Vec<String> = values
            .into_iter()
            .map(|v| absolutize(spec, &v, base_dir.as_deref()))
            .collect();

        // Affected set (§4.6 step 2 + 3).
        let affected = affected_targets(db, eid, dir_scope, spec, &values)?;
        if affected.is_empty() {
            findings.push(mk_note(format!(
                "{} has no observable effect on any target's final state in this \
                 configuration — candidate for manual removal",
                spec.legacy_cmd
            )));
            continue;
        }

        // Build the patch: delete the legacy node, insert one target call
        // per affected target after its defining command.
        let Some(mut patch) = delete_node_patch(db, node, path, hash.as_deref(), spec)? else {
            continue;
        };
        let mut related = Vec::new();
        let mut ok = true;
        for t in &affected {
            match insertion_edit(db, t, spec, &values)? {
                Some((edit, loc)) => {
                    related.push((
                        loc,
                        format!("add {}({} PRIVATE …)", spec.target_cmd, t.name),
                    ));
                    patch.edits.push(edit);
                }
                None => {
                    ok = false;
                    findings.push(mk_note(format!(
                        "cannot anchor an insertion for target '{}' (definition not in \
                         project source) — skipped",
                        t.name
                    )));
                }
            }
        }
        if !ok {
            continue;
        }
        findings.push(Finding {
            rule: format!("modernize-{}", spec.pass),
            severity: Severity::Warning,
            message: format!(
                "directory-scope {}({}) affects {} target(s): {} — replace with {} \
                 PRIVATE calls (run `cmakedb overshare` for PUBLIC evidence)",
                spec.legacy_cmd,
                values.join(" "),
                affected.len(),
                affected
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                spec.target_cmd,
            ),
            primary: span,
            related,
            fix: Some(patch),
        });
    }
    findings
        .sort_by(|a, b| (&a.primary.file, a.primary.line).cmp(&(&b.primary.file, b.primary.line)));
    Ok(findings)
}

/// Nearest non-transparent scope; Some(scope_id) only if it is a
/// directory-kind scope (root/directory).
fn effective_dir_scope(db: &Db, mut scope_id: i64) -> Result<Option<i64>> {
    for _ in 0..64 {
        let (kind, transparent, parent): (String, i64, Option<i64>) = db.conn.query_row(
            "SELECT kind, transparent, parent_id FROM scopes WHERE id = ?1",
            [scope_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if transparent == 0 {
            return Ok(match kind.as_str() {
                "root" | "directory" => Some(scope_id),
                _ => None,
            });
        }
        match parent {
            Some(p) => scope_id = p,
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// Source directory a directory scope evaluates in: for the root scope its
/// name is the top CMakeLists path; for add_subdirectory scopes it is the
/// opener's directory joined with the (relative) subdir argument.
fn scope_source_dir(db: &Db, scope_id: i64) -> Result<Option<String>> {
    let (kind, name, opened_by): (String, Option<String>, Option<i64>) = db.conn.query_row(
        "SELECT kind, name, opened_by_event FROM scopes WHERE id = ?1",
        [scope_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    match kind.as_str() {
        "root" => Ok(name.and_then(|f| {
            std::path::Path::new(&f)
                .parent()
                .map(|p| cmakedb_db::cmake_path_spelling(&p.to_string_lossy()))
        })),
        "directory" => {
            let Some(open_eid) = opened_by else {
                return Ok(None);
            };
            let opener_file: String = db.conn.query_row(
                "SELECT f.path FROM events e JOIN files f ON f.id = e.file_id WHERE e.id = ?1",
                [open_eid],
                |r| r.get(0),
            )?;
            let base = std::path::Path::new(&opener_file).parent();
            Ok(match (base, name) {
                (Some(b), Some(n)) => {
                    let sub = std::path::Path::new(&n);
                    Some(if sub.is_absolute() {
                        n
                    } else {
                        cmakedb_db::cmake_path_spelling(&b.join(sub).to_string_lossy())
                    })
                }
                _ => None,
            })
        }
        _ => Ok(None),
    }
}

fn extract_values(spec: &CodemodSpec, args: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for a in args.iter().flat_map(|a| a.split(';')) {
        if a.is_empty() {
            continue;
        }
        match spec.pass {
            "include-directories" => match a {
                "AFTER" | "BEFORE" | "SYSTEM" => {}
                v => out.push(v.to_string()),
            },
            "add-definitions" => {
                // Only -D definitions migrate cleanly; anything else means
                // the call is really add_compile_options in disguise.
                {
                    let d = a.strip_prefix("-D")?;
                    out.push(d.to_string())
                }
            }
            "link-libraries" => match a {
                "debug" | "optimized" | "general" => {}
                v => out.push(v.to_string()),
            },
            _ => out.push(a.to_string()),
        }
    }
    Some(out)
}

fn absolutize(spec: &CodemodSpec, value: &str, base_dir: Option<&str>) -> String {
    if spec.pass != "include-directories" {
        return value.to_string();
    }
    let p = std::path::Path::new(value);
    if p.is_absolute() || value.contains("$<") {
        return value.to_string();
    }
    match base_dir {
        Some(b) => std::path::Path::new(b)
            .join(p)
            .to_string_lossy()
            .to_string(),
        None => value.to_string(),
    }
}

struct AffectedTarget {
    name: String,
    defined_event: i64,
}

/// Targets defined after the legacy event within the directory-scope
/// subtree, filtered by the File API final-state check (§4.6 steps 2–3).
fn affected_targets(
    db: &Db,
    event_id: i64,
    dir_scope: i64,
    spec: &CodemodSpec,
    values: &[String],
) -> Result<Vec<AffectedTarget>> {
    let mut stmt = db.conn.prepare(
        "SELECT t.id, t.name, t.defined_event, e.scope_id
         FROM targets t JOIN events e ON e.id = t.defined_event
         WHERE t.alias_of IS NULL AND t.imported = 0 AND t.defined_event > ?1
           AND t.in_file_api = 1
         ORDER BY t.defined_event",
    )?;
    let rows: Vec<(i64, String, i64, i64)> = stmt
        .query_map([event_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut out = Vec::new();
    for (tid, name, defined_event, tscope) in rows {
        if !scope_within(db, tscope, dir_scope)? {
            continue;
        }
        // Final-state check: every value must appear in the target's
        // resolved File API state, else the directory command was shadowed
        // or filtered for this target.
        // Final-state values for this target/kind, compared in Rust: SQL
        // `=` is case-sensitive, but Windows File API values can differ
        // from trace values in drive-letter case; trailing slashes too.
        let mut stmt = db.conn.prepare(
            "SELECT value FROM usage_reqs
             WHERE target_id = ?1 AND kind = ?2 AND source = 'fileapi'",
        )?;
        let final_values: Vec<String> = stmt
            .query_map(params![tid, spec.check_kind], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let norm = |s: &str| s.trim_end_matches('/').to_string();
        let mut all_present = true;
        for v in values {
            let present = match spec.check_kind {
                // Options/links resolve into command fragments that may
                // bundle several flags; use containment.
                "option" | "link" => final_values.iter().any(|fv| fv.contains(v.as_str())),
                _ => final_values
                    .iter()
                    .any(|fv| cmakedb_db::cmake_path_eq(&norm(fv), &norm(v))),
            };
            if !present {
                all_present = false;
                break;
            }
        }
        if all_present {
            out.push(AffectedTarget {
                name,
                defined_event,
            });
        }
    }
    Ok(out)
}

fn scope_within(db: &Db, mut scope_id: i64, ancestor: i64) -> Result<bool> {
    for _ in 0..64 {
        if scope_id == ancestor {
            return Ok(true);
        }
        let parent: Option<i64> = db.conn.query_row(
            "SELECT parent_id FROM scopes WHERE id = ?1",
            [scope_id],
            |r| r.get(0),
        )?;
        match parent {
            Some(p) => scope_id = p,
            None => return Ok(false),
        }
    }
    Ok(false)
}

/// Delete the legacy command's whole line when it stands alone (leading
/// whitespace and the trailing newline go with it), else just the node.
fn delete_node_patch(
    db: &Db,
    node: i64,
    path: &str,
    hash: Option<&str>,
    spec: &CodemodSpec,
) -> Result<Option<Patch>> {
    let Some(hash) = hash else { return Ok(None) };
    let (byte_start, byte_end): (i64, i64) = db.conn.query_row(
        "SELECT byte_start, byte_end FROM ast_nodes WHERE id = ?1",
        [node],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // Offsets are valid against the *recorded* content; application
    // re-checks the on-disk hash (§3.1 position-stable node IDs).
    let content: Option<String> =
        db.conn
            .query_row("SELECT content FROM files WHERE path = ?1", [path], |r| {
                r.get(0)
            })?;
    let Some(content) = content else {
        return Ok(None);
    };
    let (mut start, mut end) = (
        byte_start as usize,
        byte_end.min(content.len() as i64) as usize,
    );
    let line_start = content[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    if content[line_start..start]
        .chars()
        .all(|c| c == ' ' || c == '\t')
    {
        // Alone on its line: consume indentation and the trailing newline.
        let rest = &content[end..];
        let eol = rest
            .find('\n')
            .map(|i| end + i + 1)
            .unwrap_or(content.len());
        if content[end..eol.saturating_sub(1).max(end)]
            .chars()
            .all(|c| c == ' ' || c == '\t')
        {
            start = line_start;
            end = eol;
        }
    }
    Ok(Some(Patch {
        title: format!("modernize-{}", spec.pass),
        edits: vec![Edit {
            file: path.to_string(),
            content_hash: hash.to_string(),
            byte_start: start,
            byte_end: end,
            replacement: String::new(),
        }],
    }))
}

/// Insert `target_cmd(<name> PRIVATE <values>)` after the target's
/// defining command node, matching its indentation.
fn insertion_edit(
    db: &Db,
    target: &AffectedTarget,
    spec: &CodemodSpec,
    values: &[String],
) -> Result<Option<(Edit, SourceSpan)>> {
    let row: Option<(i64, String, Option<String>, i64, i64, i64, Option<String>)> = db
        .conn
        .query_row(
            "SELECT n.id, f.path, f.content_hash, n.byte_start, n.byte_end, n.line, f.content
             FROM events e
             JOIN ast_nodes n ON n.id = e.node_id
             JOIN files f ON f.id = n.file_id
             WHERE e.id = ?1 AND f.in_source = 1",
            [target.defined_event],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .ok();
    let Some((_nid, path, Some(hash), byte_start, byte_end, line, Some(content))) = row else {
        return Ok(None);
    };
    let line_start = content[..byte_start as usize]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let indent: String = content[line_start..byte_start as usize]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let quoted: Vec<String> = values
        .iter()
        .map(|v| {
            if v.chars().any(|c| c.is_whitespace()) {
                format!("\"{v}\"")
            } else {
                v.clone()
            }
        })
        .collect();
    let text = format!(
        "\n{indent}{}({} PRIVATE {})",
        spec.target_cmd,
        target.name,
        quoted.join(" ")
    );
    // Insert at end of the defining line (after any trailing comment), not
    // at the node's closing paren, so the line keeps its exact bytes.
    let at = content[byte_end as usize..]
        .find('\n')
        .map(|i| byte_end as usize + i)
        .unwrap_or(content.len());
    Ok(Some((
        Edit {
            file: path.clone(),
            content_hash: hash,
            byte_start: at,
            byte_end: at,
            replacement: text,
        },
        SourceSpan::new(db.display_path(&path), line),
    )))
}
