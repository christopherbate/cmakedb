//! `shadowed-requirements` (roadmap Track F, requirement placement):
//! the same usage requirement (include dir / compile definition)
//! reaching one target through **multiple routes** — a direct
//! `target_include_directories`/`target_compile_definitions` call, a
//! directory-scope `include_directories`/`add_definitions`/
//! `add_compile_definitions` whose scope contains the target, or the
//! PUBLIC/INTERFACE interface of a directly linked dependency. CMake
//! dedups the compile line, so the build is identical either way; the
//! narrower declaration is maintenance noise.
//!
//! Routes are modeled with the directory-scope attribution and
//! retroactivity logic validated for `directory-commands`
//! (crate::dircommands helpers). The transitive route is **one hop**
//! only in v1: D's own inherited requirements (from D's dependencies)
//! are not propagated onward — a documented false-negative class, never
//! a false-positive one.
//!
//! Precision guards (each documented in the user guide entry):
//! - values containing a generator expression (`$<`) are skipped —
//!   config-dependent, redundancy is unprovable from one recording;
//! - routes with different SYSTEM-ness (a literal `SYSTEM` keyword on
//!   the declaring call) never shadow each other;
//! - both routes must originate in project files outside try_compile
//!   scratch configures;
//! - the redundant declaration and the shadowing route must sit in the
//!   same directory's files: a subproject re-declaring a parent-scope
//!   requirement is usually *standalone-build* intent, not redundancy
//!   (the precision/recall tradeoff — cross-directory true redundancy
//!   is deliberately not reported).

use anyhow::Result;
use cmakedb_db::{cmake_path_spelling, Db};
use std::collections::HashMap;

use crate::dircommands::{
    chain_contains, chain_has_try_compile, effective_dir_scope, load_candidates, load_scopes,
    nearest_dir_scope,
};
use crate::{event_span, Finding, Pass, PassConfig, Severity};

pub struct ShadowedRequirements;

/// One way a requirement value reaches a target.
struct Route {
    /// "direct" | "directory" | "transitive" — specificity in that order.
    category: &'static str,
    origin_event: i64,
    /// File path (db spelling) of the declaring call, for the same-dir
    /// standalone-build heuristic.
    file: String,
    /// Literal SYSTEM keyword on the declaring call (includes only).
    system: bool,
    /// Human label for the "also supplied by" evidence span.
    label: String,
}

impl Pass for ShadowedRequirements {
    fn id(&self) -> &'static str {
        "shadowed-requirements"
    }
    fn description(&self) -> &'static str {
        "the same include dir/definition reaching a target via multiple routes"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let scopes = load_scopes(db)?;
        let candidates = load_candidates(db, &scopes)?;

        // Trace usage_reqs with project-file origins: the direct route,
        // and (PUBLIC/INTERFACE rows) the material for the transitive
        // route. args_json carries the declaring call's keywords (SYSTEM).
        struct Req {
            target_id: i64,
            kind: String,
            value: String,
            visibility: String,
            origin_event: i64,
            scope_id: Option<i64>,
            file: String,
            system: bool,
        }
        let mut stmt = db.conn.prepare(
            "SELECT r.target_id, r.kind, r.value, r.visibility, r.origin_event,
                    e.scope_id, f.path, e.args_json
             FROM usage_reqs r
             JOIN events e ON e.id = r.origin_event
             JOIN files f ON f.id = e.file_id
             WHERE r.source = 'trace' AND r.kind IN ('include', 'define')
               AND f.in_source = 1
             ORDER BY r.id",
        )?;
        let reqs: Vec<Req> = stmt
            .query_map([], |r| {
                let args_json: String = r.get(7)?;
                Ok(Req {
                    target_id: r.get(0)?,
                    kind: r.get(1)?,
                    value: r.get(2)?,
                    visibility: r.get(3)?,
                    origin_event: r.get(4)?,
                    scope_id: r.get(5)?,
                    file: r.get(6)?,
                    system: has_system_keyword(&args_json),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let reqs: Vec<Req> = reqs
            .into_iter()
            .filter(|q| {
                q.scope_id
                    .map(|s| !chain_has_try_compile(&scopes, s))
                    .unwrap_or(true)
            })
            .collect();

        // Directory-scope requirement events: all three commands here are
        // retroactive (validated, see dircommands module docs), so a
        // target is affected when defined in the same directory
        // (regardless of order) or defined later under the scope.
        struct DirCmd {
            event: i64,
            dir_scope: i64,
            cmd: String,
            file: String,
            system: bool,
            /// (kind, raw value) pairs the call contributes.
            values: Vec<(&'static str, String)>,
        }
        let mut stmt = db.conn.prepare(
            "SELECT e.id, e.node_id, e.file_id, e.line, e.scope_id, e.cmd_lower,
                    e.args_json, f.path
             FROM events e JOIN files f ON f.id = e.file_id
             WHERE e.cmd_lower IN ('include_directories', 'add_definitions',
                                   'add_compile_definitions')
               AND f.in_source = 1
             ORDER BY e.id",
        )?;
        let rows: Vec<(
            i64,
            Option<i64>,
            i64,
            i64,
            Option<i64>,
            String,
            String,
            String,
        )> = stmt
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
        let mut dir_cmds: Vec<DirCmd> = Vec::new();
        // Re-evaluations of one call site in one directory (loops,
        // re-includes) fold into the first evaluation, as in
        // directory-commands.
        let mut seen_sites: HashMap<(Option<i64>, i64, i64, i64), ()> = HashMap::new();
        for (eid, node, file_id, line, scope_id, cmd, args_json, path) in rows {
            let Some(scope_id) = scope_id else { continue };
            if chain_has_try_compile(&scopes, scope_id) {
                continue;
            }
            let Some(dir_scope) = effective_dir_scope(&scopes, scope_id) else {
                continue;
            };
            if seen_sites
                .insert((node, file_id, line, dir_scope), ())
                .is_some()
            {
                continue;
            }
            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            dir_cmds.push(DirCmd {
                event: eid,
                dir_scope,
                system: args.iter().any(|a| a == "SYSTEM"),
                values: dir_cmd_values(&cmd, &args),
                cmd,
                file: path,
            });
        }

        // Project-file link edges to in-project targets, for the one-hop
        // transitive route. Edge visibility is irrelevant for the
        // consumer itself: a PRIVATE link still compiles T with D's
        // interface requirements.
        let mut stmt = db.conn.prepare(
            "SELECT g.src_target, g.dst_target, t.name
             FROM tgt_edges g
             JOIN targets t ON t.id = g.dst_target
             JOIN events e ON e.id = g.origin_event
             JOIN files f ON f.id = e.file_id
             WHERE g.dst_target IS NOT NULL AND f.in_source = 1
             ORDER BY g.id",
        )?;
        let edges: Vec<(i64, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut deps_of: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
        for (src, dst, name) in edges {
            if src != dst {
                let v = deps_of.entry(src).or_default();
                if !v.iter().any(|(d, _)| *d == dst) {
                    v.push((dst, name));
                }
            }
        }
        // Dependency interface: PUBLIC/INTERFACE trace reqs per target.
        let mut iface_of: HashMap<i64, Vec<usize>> = HashMap::new();
        for (i, q) in reqs.iter().enumerate() {
            if q.visibility == "PUBLIC" || q.visibility == "INTERFACE" {
                iface_of.entry(q.target_id).or_default().push(i);
            }
        }

        let mut findings = Vec::new();
        for cand in &candidates {
            if cfg.ignored(&cand.name) {
                continue;
            }
            // (kind, normalized value) -> routes, in first-seen order.
            let mut order: Vec<(String, String)> = Vec::new();
            let mut by_value: HashMap<(String, String), Vec<Route>> = HashMap::new();
            let mut raw_value: HashMap<(String, String), String> = HashMap::new();
            let mut push = |key: (String, String), raw: &str, route: Route| {
                let entry = by_value.entry(key.clone()).or_default();
                if entry.is_empty() {
                    order.push(key.clone());
                    raw_value.insert(key, raw.to_string());
                }
                // One route per (category, origin) — re-evaluations and
                // repeated list entries collapse.
                if !entry
                    .iter()
                    .any(|r| r.category == route.category && r.origin_event == route.origin_event)
                {
                    entry.push(route);
                }
            };

            for q in reqs.iter().filter(|q| q.target_id == cand.id) {
                push(
                    (q.kind.clone(), value_key(&q.kind, &q.value)),
                    &q.value,
                    Route {
                        category: "direct",
                        origin_event: q.origin_event,
                        file: q.file.clone(),
                        system: q.system,
                        label: format!(
                            "declared directly on '{}' ({})",
                            cand.name,
                            q.visibility.to_lowercase()
                        ),
                    },
                );
            }
            for dc in &dir_cmds {
                let same_dir_retro =
                    nearest_dir_scope(&scopes, cand.scope_id) == Some(dc.dir_scope);
                let later = cand.defined_event > dc.event
                    && chain_contains(&scopes, cand.scope_id, dc.dir_scope);
                if !(same_dir_retro || later) {
                    continue;
                }
                for (kind, raw) in &dc.values {
                    push(
                        (kind.to_string(), value_key(kind, raw)),
                        raw,
                        Route {
                            category: "directory",
                            origin_event: dc.event,
                            file: dc.file.clone(),
                            system: dc.system,
                            label: format!("also supplied by directory-scope {}", dc.cmd),
                        },
                    );
                }
            }
            for (dep_id, dep_name) in deps_of.get(&cand.id).into_iter().flatten() {
                for &i in iface_of.get(dep_id).into_iter().flatten() {
                    let q = &reqs[i];
                    push(
                        (q.kind.clone(), value_key(&q.kind, &q.value)),
                        &q.value,
                        Route {
                            category: "transitive",
                            origin_event: q.origin_event,
                            file: q.file.clone(),
                            system: q.system,
                            label: format!(
                                "also supplied by the {} interface of linked dependency '{}'",
                                q.visibility.to_lowercase(),
                                dep_name
                            ),
                        },
                    );
                }
            }

            for key in &order {
                let routes = &by_value[key];
                let raw = &raw_value[key];
                // Genexes are config-dependent; redundancy is unprovable.
                if raw.contains("$<") {
                    continue;
                }
                // Most specific redundant declaration: the direct call
                // when any broader route also supplies the value; a
                // directory command when only a transitive route does.
                let primary = routes
                    .iter()
                    .find(|r| r.category == "direct")
                    .or_else(|| routes.iter().find(|r| r.category == "directory"));
                let Some(primary) = primary else { continue };
                let shadows: Vec<&Route> = routes
                    .iter()
                    .filter(|r| r.category != primary.category)
                    // Different SYSTEM-ness = different compile-line
                    // semantics; never treat as shadowing.
                    .filter(|r| r.system == primary.system)
                    // Standalone-build heuristic: only same-directory
                    // routes prove redundancy (see module docs).
                    .filter(|r| same_dir(&r.file, &primary.file))
                    .collect();
                if shadows.is_empty() {
                    continue;
                }

                let (kind, _) = key;
                let what = if kind == "include" {
                    format!("include dir '{raw}'")
                } else {
                    format!("definition '{raw}'")
                };
                let via = shadows
                    .iter()
                    .map(|r| r.label.trim_start_matches("also supplied by ").to_string())
                    .collect::<Vec<_>>()
                    .join(" and ");
                let redundant_what = if primary.category == "direct" {
                    "this declaration"
                } else {
                    "this directory-scope call's entry"
                };
                // Include ORDER matters when two dirs hold same-named
                // headers and cmake dedup keeps the first occurrence —
                // the pass's biggest caveat (user guide).
                let caution = if kind == "include" {
                    "; caution: removing it can reorder includes (see the user guide)"
                } else {
                    ""
                };
                findings.push(Finding {
                    rule: self.id().into(),
                    severity: Severity::Note,
                    message: format!(
                        "{}: {what} also reaches the target via {via} — {redundant_what} is \
                         redundant in this configuration (cmake dedups the compile line){caution}",
                        cand.name
                    ),
                    primary: event_span(db, primary.origin_event),
                    related: shadows
                        .iter()
                        .map(|r| (event_span(db, r.origin_event), r.label.clone()))
                        .collect(),
                    fix: None,
                });
            }
        }
        Ok(findings)
    }
}

/// Requirement values a directory command contributes. `add_definitions`
/// historically routes non-`-D` flags elsewhere, so only `-D` tokens
/// count there; `add_compile_definitions` takes plain `FOO[=v]` tokens
/// (a leading `-D` is stripped, as cmake does). Expanded list variables
/// arrive `;`-joined (AGENTS.md) — always split.
fn dir_cmd_values(cmd: &str, args: &[String]) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for a in args.iter().flat_map(|a| a.split(';')) {
        if a.is_empty() {
            continue;
        }
        match cmd {
            "include_directories" => {
                if !matches!(a, "AFTER" | "BEFORE" | "SYSTEM") {
                    out.push(("include", a.to_string()));
                }
            }
            "add_definitions" => {
                if let Some(v) = a.strip_prefix("-D") {
                    out.push(("define", v.to_string()));
                }
            }
            _ => out.push(("define", a.strip_prefix("-D").unwrap_or(a).to_string())),
        }
    }
    out
}

/// Comparison key for a requirement value. Includes compare in the
/// recording's own path spelling (never canonicalized — AGENTS.md) with
/// trailing slashes trimmed; on Windows the spelling comparison is
/// case-insensitive (matching `cmake_path_eq`), folded here so the key
/// is usable in a map. Defines compare as the full `FOO=BAR` token.
fn value_key(kind: &str, value: &str) -> String {
    if kind != "include" {
        return value.to_string();
    }
    let spelled = cmake_path_spelling(value);
    let trimmed = if spelled.len() > 1 {
        spelled.trim_end_matches('/')
    } else {
        &spelled
    };
    if cfg!(windows) {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed.to_string()
    }
}

/// Same-directory test for the standalone-build heuristic: the two
/// declaring calls' files live in the same directory (same file counts).
fn same_dir(a: &str, b: &str) -> bool {
    let dir = |p: &str| -> String {
        let s = cmake_path_spelling(p);
        match s.rfind('/') {
            Some(i) => s[..i].to_string(),
            None => String::new(),
        }
    };
    cmakedb_db::cmake_path_eq(&dir(a), &dir(b))
}

/// Literal SYSTEM keyword on the declaring call (trace args are
/// per-source-argument expansions; the keyword is always its own token).
fn has_system_keyword(args_json: &str) -> bool {
    serde_json::from_str::<Vec<String>>(args_json)
        .map(|args| args.iter().any(|a| a == "SYSTEM"))
        .unwrap_or(false)
}
