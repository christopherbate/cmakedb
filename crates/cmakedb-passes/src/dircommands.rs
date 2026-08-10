//! `directory-commands` (roadmap Track F, requirement placement):
//! executed directory-scope requirement commands — `include_directories`,
//! `link_libraries`, `add_definitions`, `add_compile_definitions`,
//! `add_compile_options` — reported with the exact targets the recording
//! proves inherit them, proposing the target-scoped equivalent.
//!
//! Report-only by design: the auto-fixable subset (single evaluation,
//! plain arguments) is `modernize`'s job behind the re-record
//! verification loop; this pass reports *every* directory-scope call,
//! including the shapes modernize skips, so nothing legacy stays
//! invisible.
//!
//! Affected set (empirically validated against CMake 4.2.1, see the
//! user guide entry): `include_directories`, `add_definitions` and
//! `add_compile_definitions` are **retroactive** — they also apply to
//! targets already created in the same directory (LLVM's MCTargetDesc
//! "include main target directory" hack depends on this);
//! `add_compile_options` and `link_libraries` affect only targets
//! created *after* the call. Subdirectories snapshot the parent's
//! directory properties at `add_subdirectory` time, so subtree targets
//! are affected only when their `defined_event` comes after the command
//! (add_subdirectory processing is depth-first immediate).

use anyhow::Result;
use cmakedb_db::Db;
use std::collections::HashMap;

use crate::{event_span, Finding, Pass, PassConfig, Severity};

pub struct DirectoryCommands;

/// (directory command, target-scoped replacement, retroactive: whether
/// the command also applies to targets already created in the same
/// directory — validated against real cmake, see module docs).
const COMMANDS: &[(&str, &str, bool)] = &[
    ("include_directories", "target_include_directories", true),
    ("link_libraries", "target_link_libraries", false),
    ("add_definitions", "target_compile_definitions", true),
    (
        "add_compile_definitions",
        "target_compile_definitions",
        true,
    ),
    ("add_compile_options", "target_compile_options", false),
];

// Scope/candidate model and helpers below are shared with the
// `shadowed-requirements` pass (crate::shadowedreqs), which reuses the
// validated directory-scope attribution + retroactivity logic.
pub(crate) struct ScopeInfo {
    pub(crate) kind: String,
    pub(crate) parent: Option<i64>,
}

pub(crate) struct Candidate {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) defined_event: i64,
    pub(crate) scope_id: i64,
}

impl Pass for DirectoryCommands {
    fn id(&self) -> &'static str {
        "directory-commands"
    }
    fn description(&self) -> &'static str {
        "directory-scope include/definition/option/link commands to demote to target scope"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let scopes = load_scopes(db)?;
        let candidates = load_candidates(db, &scopes)?;

        let mut stmt = db.conn.prepare(
            "SELECT e.id, e.node_id, e.file_id, e.line, e.scope_id, e.cmd_lower
             FROM events e JOIN files f ON f.id = e.file_id
             WHERE e.cmd_lower IN ('include_directories', 'link_libraries',
                                   'add_definitions', 'add_compile_definitions',
                                   'add_compile_options')
               AND f.in_source = 1
             ORDER BY e.id",
        )?;
        let rows: Vec<(i64, Option<i64>, i64, i64, Option<i64>, String)> = stmt
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

        // Group by (call site, directory scope): a loop/re-include
        // re-evaluating the same node in the same directory produces one
        // finding with an evaluation count, not N duplicates. The same
        // node evaluated in *different* directories (an include()d
        // settings file) stays one finding per directory — the affected
        // sets genuinely differ.
        struct Group {
            first_event: i64,
            dir_scope: i64,
            cmd: String,
            evals: usize,
        }
        let mut order: Vec<(Option<i64>, i64, i64, i64)> = Vec::new();
        let mut groups: HashMap<(Option<i64>, i64, i64, i64), Group> = HashMap::new();
        for (eid, node, file_id, line, scope_id, cmd) in rows {
            let Some(scope_id) = scope_id else { continue };
            // try_compile scratch configures are an isolated world
            // (AGENTS.md); their events must not be linted.
            if chain_has_try_compile(&scopes, scope_id) {
                continue;
            }
            // Only calls that execute at directory scope: transparent
            // scopes (macro/include) and block() pass through — they do
            // not change the current directory — while function bodies
            // are skipped (the call there is not a directory-scope
            // command at its own site; documented FN).
            let Some(dir_scope) = effective_dir_scope(&scopes, scope_id) else {
                continue;
            };
            let key = (node, file_id, line, dir_scope);
            match groups.entry(key) {
                std::collections::hash_map::Entry::Occupied(mut o) => o.get_mut().evals += 1,
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(Group {
                        first_event: eid,
                        dir_scope,
                        cmd,
                        evals: 1,
                    });
                    order.push(key);
                }
            }
        }

        let mut findings = Vec::new();
        for key in order {
            let g = &groups[&key];
            let (target_cmd, retroactive) = COMMANDS
                .iter()
                .find(|(c, _, _)| *c == g.cmd)
                .map(|(_, t, retro)| (*t, *retro))
                .unwrap_or(("target_*", false));

            // Affected targets (see module docs for the validated
            // semantics): same-directory targets regardless of order for
            // the retroactive commands, plus targets defined after the
            // command anywhere under this directory scope (subdirectories
            // added afterwards inherit the property snapshot).
            let affected: Vec<&Candidate> = candidates
                .iter()
                .filter(|c| {
                    let later = c.defined_event > g.first_event
                        && chain_contains(&scopes, c.scope_id, g.dir_scope);
                    let same_dir_retro =
                        retroactive && nearest_dir_scope(&scopes, c.scope_id) == Some(g.dir_scope);
                    later || same_dir_retro
                })
                .collect();
            let evals = if g.evals > 1 {
                format!(" [evaluated {} times]", g.evals)
            } else {
                String::new()
            };

            if affected.is_empty() {
                let why = if retroactive {
                    "this directory has no targets and none is defined after it below"
                } else {
                    "no target is defined after it under this directory"
                };
                findings.push(Finding {
                    rule: self.id().into(),
                    severity: Severity::Note,
                    message: format!(
                        "{} at directory scope affects no targets in this configuration — \
                         dead directory command ({why}){evals}",
                        g.cmd
                    ),
                    primary: event_span(db, g.first_event),
                    related: vec![],
                    fix: None,
                });
                continue;
            }

            // `ignore-patterns` matches affected-target names — the knob
            // for generated/vendored targets. All names ignored = the
            // whole finding is opted out (not a dead-command claim).
            let kept: Vec<&&Candidate> =
                affected.iter().filter(|c| !cfg.ignored(&c.name)).collect();
            if kept.is_empty() {
                continue;
            }

            // A subdirectory creating targets under the inherited scope
            // must never be silently dropped from the proposal: every
            // affected target is counted and listed (capped), and the
            // inheritance is called out explicitly.
            let inherited = kept
                .iter()
                .filter(|c| nearest_dir_scope(&scopes, c.scope_id) != Some(g.dir_scope))
                .count();
            let names: Vec<&str> = kept.iter().map(|c| c.name.as_str()).collect();
            let mut list = names.iter().take(5).copied().collect::<Vec<_>>().join(", ");
            if names.len() > 5 {
                list.push_str(&format!(", … (+{} more)", names.len() - 5));
            }
            let inherit_note = if inherited > 0 {
                format!(
                    "; {inherited} of these are defined in subdirectories and inherit it — \
                     a target-scoped rewrite must cover those too"
                )
            } else {
                String::new()
            };
            let related = kept
                .iter()
                .take(5)
                .map(|c| {
                    let note = if nearest_dir_scope(&scopes, c.scope_id) != Some(g.dir_scope) {
                        format!(
                            "target '{}' defined in a subdirectory inherits the \
                             directory-scope {}",
                            c.name, g.cmd
                        )
                    } else {
                        format!(
                            "target '{}' defined here inherits the directory-scope {}",
                            c.name, g.cmd
                        )
                    };
                    (event_span(db, c.defined_event), note)
                })
                .collect();
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Note,
                message: format!(
                    "{} at directory scope affects {} target(s); prefer \
                     {target_cmd}(<tgt> PRIVATE …) — affected: {list}{inherit_note}{evals}",
                    g.cmd,
                    kept.len(),
                ),
                primary: event_span(db, g.first_event),
                related,
                fix: None,
            });
        }
        Ok(findings)
    }
}

pub(crate) fn load_scopes(db: &Db) -> Result<HashMap<i64, ScopeInfo>> {
    let mut stmt = db.conn.prepare("SELECT id, kind, parent_id FROM scopes")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            ScopeInfo {
                kind: r.get(1)?,
                parent: r.get(2)?,
            },
        ))
    })?;
    let mut out = HashMap::new();
    for r in rows {
        let (id, info) = r?;
        out.insert(id, info);
    }
    Ok(out)
}

/// Targets a directory-scope command can affect at all: real (non-alias,
/// non-imported) targets that compile or link. INTERFACE libraries have
/// no private compilation and never inherit directory properties; UTILITY
/// (custom) targets neither compile nor link; UNKNOWN libraries are
/// imported wrappers in practice.
pub(crate) fn load_candidates(db: &Db, scopes: &HashMap<i64, ScopeInfo>) -> Result<Vec<Candidate>> {
    let mut stmt = db.conn.prepare(
        "SELECT t.id, t.name, t.defined_event, e.scope_id
         FROM targets t JOIN events e ON e.id = t.defined_event
         WHERE t.imported = 0 AND t.alias_of IS NULL
           AND (t.type IS NULL OR t.type NOT IN
                ('INTERFACE_LIBRARY', 'ALIAS', 'UTILITY', 'UNKNOWN_LIBRARY'))
         ORDER BY t.defined_event",
    )?;
    let rows: Vec<(i64, String, i64, Option<i64>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, name, defined_event, scope_id)| {
            let scope_id = scope_id?;
            // Targets defined inside try_compile scratch configures are
            // not project targets.
            (!chain_has_try_compile(scopes, scope_id)).then_some(Candidate {
                id,
                name,
                defined_event,
                scope_id,
            })
        })
        .collect())
}

/// Nearest directory-kind scope reached without crossing a function or
/// try_compile boundary; macro/include (transparent) and block() scopes
/// pass through — they execute in the current directory.
pub(crate) fn effective_dir_scope(
    scopes: &HashMap<i64, ScopeInfo>,
    mut scope_id: i64,
) -> Option<i64> {
    for _ in 0..64 {
        let s = scopes.get(&scope_id)?;
        match s.kind.as_str() {
            "root" | "directory" => return Some(scope_id),
            "function" | "try_compile" => return None,
            _ => scope_id = s.parent?,
        }
    }
    None
}

/// The directory a scope *belongs to* dynamically: nearest directory-kind
/// scope on the parent chain, crossing function scopes too — a target
/// created inside a function belongs to the directory the function was
/// called from (that's the cmMakefile the target is registered in).
pub(crate) fn nearest_dir_scope(
    scopes: &HashMap<i64, ScopeInfo>,
    mut scope_id: i64,
) -> Option<i64> {
    for _ in 0..64 {
        let s = scopes.get(&scope_id)?;
        match s.kind.as_str() {
            "root" | "directory" => return Some(scope_id),
            "try_compile" => return None,
            _ => scope_id = s.parent?,
        }
    }
    None
}

pub(crate) fn chain_has_try_compile(scopes: &HashMap<i64, ScopeInfo>, mut scope_id: i64) -> bool {
    for _ in 0..64 {
        let Some(s) = scopes.get(&scope_id) else {
            return false;
        };
        if s.kind == "try_compile" {
            return true;
        }
        match s.parent {
            Some(p) => scope_id = p,
            None => return false,
        }
    }
    false
}

/// Whether `ancestor` is on `scope_id`'s parent chain without a
/// try_compile boundary in between (scratch scopes are opaque).
pub(crate) fn chain_contains(
    scopes: &HashMap<i64, ScopeInfo>,
    mut scope_id: i64,
    ancestor: i64,
) -> bool {
    for _ in 0..64 {
        if scope_id == ancestor {
            return true;
        }
        let Some(s) = scopes.get(&scope_id) else {
            return false;
        };
        if s.kind == "try_compile" {
            return false;
        }
        match s.parent {
            Some(p) => scope_id = p,
            None => return false,
        }
    }
    false
}
