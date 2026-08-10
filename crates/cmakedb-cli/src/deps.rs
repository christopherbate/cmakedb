//! `cmakedb deps`: the build-configuration dependency inventory — every
//! `find_package`, `FetchContent_Declare`, and `ExternalProject_Add` the
//! configure actually evaluated, with how each resolved, exportable as
//! CycloneDX 1.5 JSON (ROADMAP Track B "Build-config SBOM").
//!
//! Evidence model (queries only, no re-execution; try_compile scratch
//! configures excluded, declarations restricted to project files):
//!
//! - **find_package** events carry the request (name, version,
//!   components, REQUIRED/QUIET). Resolution is read back from the
//!   variable writes the find module / config file performed after the
//!   call: a truthy `<Pkg>_FOUND` write means found. Case variants:
//!   FPHSA writes both `<Pkg>_FOUND` and the all-uppercase spelling,
//!   config packages write `<Pkg>_FOUND` — matched case-insensitively.
//!   `<Pkg>_VERSION` / `<Pkg>_VERSION_STRING` writes give the resolved
//!   version; `<Pkg>_DIR` / `<Pkg>_CONFIG` cache writes give a resolved
//!   location hint. Caveat: `<Pkg>_FOUND` set *inside the cmake binary*
//!   (config mode's own bookkeeping) is not a traced command, so a
//!   config package whose files write nothing themselves resolves only
//!   by absence of evidence (documented in the user guide).
//! - **FetchContent_Declare / ExternalProject_Add** events carry the
//!   declared source and its pinning class — the same classification as
//!   the `fetchcontent-pinning` lint (`cmakedb_passes::hygiene`): the
//!   commit-SHA test is the shared `is_commit_sha`, and the
//!   keyword-pairing rules below mirror that pass; keep them in sync.
//! - **Imported targets** (`targets.imported = 1`) group under the
//!   find_package whose name matches their `Ns::` namespace prefix
//!   (case-insensitive); unmatched ones are listed separately.

use anyhow::Result;
use clap::ValueEnum;
use cmakedb_db::Db;
use cmakedb_passes::is_commit_sha;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, ValueEnum)]
pub enum DepsFormat {
    Text,
    Json,
    Cyclonedx,
}

#[derive(Debug, Serialize)]
pub struct Inventory {
    pub schema_version: u32,
    /// meta PROJECT_NAME when stored, else source-dir basename.
    pub project: String,
    pub find_packages: Vec<FindPackage>,
    pub fetched: Vec<Fetched>,
    /// Imported targets whose namespace matches no find_package name.
    pub unmatched_imported_targets: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct FindPackage {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub optional_components: Vec<String>,
    pub required: bool,
    /// True only when every call passed QUIET.
    pub quiet: bool,
    pub calls: i64,
    pub found: bool,
    /// The `<Pkg>_FOUND` write backing the status (`Python3_FOUND=TRUE`),
    /// or a statement that none exists.
    pub evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_version: Option<String>,
    /// `<Pkg>_DIR` / `<Pkg>_CONFIG` cache write, as `NAME=value`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_hint: Option<String>,
    pub declared_at: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub imported_targets: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Fetched {
    pub name: String,
    /// "FetchContent" | "ExternalProject".
    pub mechanism: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_hash: Option<String>,
    /// pinned-commit | mutable-ref | default-branch | hashed-archive |
    /// unpinned-url | unknown-source.
    pub pinning: String,
    pub declared_at: String,
}

pub fn inventory(db: &Db) -> Result<Inventory> {
    let mut find_packages = collect_find_packages(db)?;
    let fetched = collect_fetched(db)?;
    let unmatched_imported_targets = attach_imported_targets(db, &mut find_packages)?;
    Ok(Inventory {
        schema_version: 1,
        project: project_name(db)?,
        find_packages,
        fetched,
        unmatched_imported_targets,
    })
}

fn project_name(db: &Db) -> Result<String> {
    if let Some(n) = db.get_meta("PROJECT_NAME")? {
        if !n.is_empty() {
            return Ok(n);
        }
    }
    let src = db.get_meta("source_dir")?.unwrap_or_default();
    let base = cmakedb_db::cmake_path_spelling(&src)
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    Ok(if base.is_empty() {
        "unknown".to_string()
    } else {
        base
    })
}

// -- find_package ----------------------------------------------------------

fn collect_find_packages(db: &Db) -> Result<Vec<FindPackage>> {
    let mut stmt = db.conn.prepare(
        "SELECT e.id, e.args_json FROM events e
         JOIN files f ON f.id = e.file_id
         WHERE e.cmd_lower = 'find_package' AND f.in_source = 1
         ORDER BY e.id",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    // Group calls by package name (case-sensitive: package names are).
    let mut order: Vec<String> = Vec::new();
    let mut first_event: HashMap<String, i64> = HashMap::new();
    let mut map: HashMap<String, FindPackage> = HashMap::new();
    for (event, args_json) in rows {
        if in_try_compile(db, event) {
            continue;
        }
        // json-v1 shows unquoted args that expanded to nothing as "" even
        // though the command never received them — drop them (same rule
        // as the hygiene passes).
        let args: Vec<String> = serde_json::from_str::<Vec<String>>(&args_json)
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !a.is_empty())
            .collect();
        let Some(name) = args.first().cloned() else {
            continue;
        };
        let req = parse_request(&args[1..]);
        let entry = map.entry(name.clone()).or_insert_with(|| {
            order.push(name.clone());
            first_event.insert(name.clone(), event);
            FindPackage {
                name: name.clone(),
                requested_version: None,
                components: Vec::new(),
                optional_components: Vec::new(),
                required: false,
                quiet: true,
                calls: 0,
                found: false,
                evidence: String::new(),
                resolved_version: None,
                location_hint: None,
                declared_at: db.event_location(event).unwrap_or_default(),
                imported_targets: Vec::new(),
            }
        });
        entry.calls += 1;
        entry.required |= req.required;
        entry.quiet &= req.quiet;
        if entry.requested_version.is_none() {
            entry.requested_version = req.version;
        }
        for c in req.components {
            if !entry.components.contains(&c) {
                entry.components.push(c);
            }
        }
        for c in req.optional_components {
            if !entry.optional_components.contains(&c) {
                entry.optional_components.push(c);
            }
        }
    }

    let mut out = Vec::with_capacity(order.len());
    for name in order {
        let mut p = map.remove(&name).expect("grouped above");
        resolve(db, &mut p, first_event[&name])?;
        out.push(p);
    }
    Ok(out)
}

struct Request {
    version: Option<String>,
    required: bool,
    quiet: bool,
    components: Vec<String>,
    optional_components: Vec<String>,
}

/// find_package keywords (both signatures) that terminate component
/// collection; values of the non-component multi-value keywords (NAMES,
/// HINTS, PATHS, ...) are deliberately ignored.
const FP_KEYWORDS: &[&str] = &[
    "EXACT",
    "QUIET",
    "REQUIRED",
    "COMPONENTS",
    "OPTIONAL_COMPONENTS",
    "MODULE",
    "CONFIG",
    "NO_MODULE",
    "GLOBAL",
    "NO_POLICY_SCOPE",
    "BYPASS_PROVIDER",
    "NAMES",
    "CONFIGS",
    "HINTS",
    "PATHS",
    "PATH_SUFFIXES",
    "REGISTRY_VIEW",
    "NO_DEFAULT_PATH",
    "NO_PACKAGE_ROOT_PATH",
    "NO_CMAKE_PATH",
    "NO_CMAKE_ENVIRONMENT_PATH",
    "NO_SYSTEM_ENVIRONMENT_PATH",
    "NO_CMAKE_PACKAGE_REGISTRY",
    "NO_CMAKE_BUILDS_PATH",
    "NO_CMAKE_SYSTEM_PATH",
    "NO_CMAKE_INSTALL_PREFIX",
    "NO_CMAKE_SYSTEM_PACKAGE_REGISTRY",
    "CMAKE_FIND_ROOT_PATH_BOTH",
    "ONLY_CMAKE_FIND_ROOT_PATH",
    "NO_CMAKE_FIND_ROOT_PATH",
];

/// Parse everything after the package name. `REQUIRED` doubles as a
/// component-list opener (`find_package(Foo REQUIRED comp1 comp2)`).
fn parse_request(rest: &[String]) -> Request {
    #[derive(PartialEq)]
    enum Mode {
        None,
        Components,
        Optional,
    }
    let mut r = Request {
        version: None,
        required: false,
        quiet: false,
        components: Vec::new(),
        optional_components: Vec::new(),
    };
    let mut mode = Mode::None;
    for (i, a) in rest.iter().enumerate() {
        if FP_KEYWORDS.contains(&a.as_str()) {
            mode = match a.as_str() {
                "REQUIRED" => {
                    r.required = true;
                    Mode::Components
                }
                "QUIET" => {
                    r.quiet = true;
                    Mode::None
                }
                "COMPONENTS" => Mode::Components,
                "OPTIONAL_COMPONENTS" => Mode::Optional,
                _ => Mode::None,
            };
            continue;
        }
        if i == 0 && is_version_shape(a) {
            r.version = Some(a.clone());
            continue;
        }
        match mode {
            Mode::Components => {
                if !r.components.contains(a) {
                    r.components.push(a.clone());
                }
            }
            Mode::Optional => {
                if !r.optional_components.contains(a) {
                    r.optional_components.push(a.clone());
                }
            }
            Mode::None => {}
        }
    }
    r
}

/// A find_package version or version range: `3.0`, `1.12.4`,
/// `1.12...1.14`, `1.12...<2.0`.
fn is_version_shape(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '<')
}

/// Fill `found`/`evidence`/`resolved_version`/`location_hint` from the
/// variable writes at or after the package's first find_package event.
fn resolve(db: &Db, p: &mut FindPackage, first_event: i64) -> Result<()> {
    // COLLATE NOCASE folds all observed spellings (`Python3_FOUND`,
    // `PYTHON3_FOUND`, ...) into one probe.
    let mut stmt = db.conn.prepare(
        "SELECT name, value FROM var_writes
         WHERE name = ?1 COLLATE NOCASE AND event_id >= ?2
           AND write_kind IN ('set', 'cache', 'parent_scope')
         ORDER BY event_id",
    )?;
    let writes: Vec<(String, Option<String>)> = stmt
        .query_map(
            rusqlite::params![format!("{}_FOUND", p.name), first_event],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let truthy = writes
        .iter()
        .find(|(_, v)| v.as_deref().is_some_and(cmake_truthy));
    if let Some((n, v)) = truthy {
        p.found = true;
        p.evidence = format!("{n}={}", v.as_deref().unwrap_or(""));
    } else if let Some((n, v)) = writes.iter().rev().find(|(_, v)| v.is_some()) {
        p.evidence = format!("{n}={}", v.as_deref().unwrap_or(""));
    } else {
        p.evidence = format!("no {}_FOUND write", p.name);
    }
    if !p.found {
        return Ok(());
    }

    let mut stmt = db.conn.prepare(
        "SELECT value FROM var_writes
         WHERE (name = ?1 COLLATE NOCASE OR name = ?2 COLLATE NOCASE)
           AND event_id >= ?3 AND value IS NOT NULL AND value <> ''
           AND write_kind IN ('set', 'cache', 'parent_scope')
         ORDER BY CASE WHEN name = ?1 COLLATE NOCASE THEN 0 ELSE 1 END, event_id",
    )?;
    let versions: Vec<String> = stmt
        .query_map(
            rusqlite::params![
                format!("{}_VERSION", p.name),
                format!("{}_VERSION_STRING", p.name),
                first_event
            ],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;
    p.resolved_version = versions.into_iter().find(|v| cmake_truthy(v));

    let mut stmt = db.conn.prepare(
        "SELECT name, value FROM var_writes
         WHERE (name = ?1 COLLATE NOCASE OR name = ?2 COLLATE NOCASE)
           AND event_id >= ?3 AND write_kind = 'cache'
           AND value IS NOT NULL AND value <> ''
         ORDER BY event_id DESC",
    )?;
    let hints: Vec<(String, String)> = stmt
        .query_map(
            rusqlite::params![
                format!("{}_DIR", p.name),
                format!("{}_CONFIG", p.name),
                first_event
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    p.location_hint = hints
        .into_iter()
        .find(|(_, v)| cmake_truthy(v))
        .map(|(n, v)| format!("{n}={}", db.display_path(&v)));
    Ok(())
}

/// CMake truthiness of an expanded value: everything except the false
/// constants (empty, 0, OFF, NO, FALSE, N, IGNORE, NOTFOUND, *-NOTFOUND).
fn cmake_truthy(v: &str) -> bool {
    let u = v.trim().to_ascii_uppercase();
    !(u.is_empty()
        || u == "0"
        || u == "OFF"
        || u == "NO"
        || u == "FALSE"
        || u == "N"
        || u == "IGNORE"
        || u == "NOTFOUND"
        || u.ends_with("-NOTFOUND"))
}

// -- FetchContent / ExternalProject ----------------------------------------

fn collect_fetched(db: &Db) -> Result<Vec<Fetched>> {
    let mut stmt = db.conn.prepare(
        "SELECT e.id, e.node_id, e.cmd_lower, e.args_json FROM events e
         JOIN files f ON f.id = e.file_id
         WHERE e.cmd_lower IN ('fetchcontent_declare', 'externalproject_add')
           AND f.in_source = 1
         ORDER BY e.id",
    )?;
    let rows: Vec<(i64, Option<i64>, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;

    // One entry per declaration site, not per re-execution.
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (event, node, cmd_lower, args_json) in rows {
        if !seen.insert((node, if node.is_some() { 0 } else { event })) {
            continue;
        }
        if in_try_compile(db, event) {
            continue;
        }
        let args: Vec<String> = serde_json::from_str::<Vec<String>>(&args_json)
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !a.is_empty())
            .collect();
        let Some(name) = args.first().cloned() else {
            continue;
        };
        let value_of = |kw: &str| {
            args.iter()
                .position(|a| a == kw)
                .and_then(|p| args.get(p + 1))
                .cloned()
        };
        let git_repository = value_of("GIT_REPOSITORY");
        let git_tag = value_of("GIT_TAG");
        let url = value_of("URL");
        let url_hash = value_of("URL_HASH").or_else(|| value_of("URL_MD5"));
        // Classification mirrors the fetchcontent-pinning lint
        // (hygiene.rs) — keep the two in sync.
        let pinning = if git_repository.is_some() {
            match git_tag.as_deref() {
                Some(tag) if is_commit_sha(tag) => "pinned-commit",
                Some(_) => "mutable-ref",
                None => "default-branch",
            }
        } else if url.is_some() {
            if url_hash.is_some() {
                "hashed-archive"
            } else {
                "unpinned-url"
            }
        } else {
            "unknown-source"
        };
        out.push(Fetched {
            name,
            mechanism: if cmd_lower == "fetchcontent_declare" {
                "FetchContent"
            } else {
                "ExternalProject"
            }
            .to_string(),
            git_repository,
            git_tag,
            url,
            url_hash,
            pinning: pinning.to_string(),
            declared_at: db.event_location(event).unwrap_or_default(),
        });
    }
    Ok(out)
}

// -- imported targets ------------------------------------------------------

/// Attach imported targets to their likely package by namespace prefix
/// (`Foo::bar` → package `Foo`, case-insensitive); return the rest.
fn attach_imported_targets(db: &Db, packages: &mut [FindPackage]) -> Result<Vec<String>> {
    let mut stmt = db
        .conn
        .prepare("SELECT name FROM targets WHERE imported = 1 ORDER BY name")?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut unmatched = Vec::new();
    for t in names {
        let ns = t.split_once("::").map(|(ns, _)| ns);
        let hit = ns.and_then(|ns| {
            packages
                .iter_mut()
                .find(|p| p.name.eq_ignore_ascii_case(ns))
        });
        match hit {
            Some(p) => p.imported_targets.push(t),
            None => unmatched.push(t),
        }
    }
    Ok(unmatched)
}

/// Whether the event's scope chain passes through a try_compile scope.
/// Local copy of the crate-private `cmakedb_passes::event_in_try_compile`
/// helper (scratch configures are an isolated world, see AGENTS.md).
fn in_try_compile(db: &Db, event_id: i64) -> bool {
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
        if kind == "try_compile" {
            return true;
        }
        scope = parent;
    }
    false
}

// -- CycloneDX 1.5 ---------------------------------------------------------

/// Derive a purl only when nothing must be guessed: a github.com
/// GIT_REPOSITORY (https/ssh/git syntax, exactly owner/repo) pinned to a
/// full commit SHA becomes `pkg:github/<owner>/<repo>@<sha>` (owner and
/// repo lowercased per the purl spec). Everything else gets no purl.
fn github_purl(repo: &str, sha: &str) -> Option<String> {
    let rest = repo
        .strip_prefix("https://github.com/")
        .or_else(|| repo.strip_prefix("http://github.com/"))
        .or_else(|| repo.strip_prefix("ssh://git@github.com/"))
        .or_else(|| repo.strip_prefix("git@github.com:"))
        .or_else(|| repo.strip_prefix("git://github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(format!(
        "pkg:github/{}/{}@{sha}",
        owner.to_lowercase(),
        name.to_lowercase()
    ))
}

fn prop(name: &str, value: &str) -> serde_json::Value {
    serde_json::json!({ "name": name, "value": value })
}

/// CycloneDX 1.5 JSON. Required fields: bomFormat + specVersion
/// (serialNumber is optional and deliberately omitted — no RNG in std,
/// and a BOM without one is valid). Not-found packages are included with
/// `cmakedb:found = false`: the misses are what CI wants to see.
pub fn render_cyclonedx(inv: &Inventory) -> serde_json::Value {
    let mut components = Vec::new();
    for p in &inv.find_packages {
        let mut props = vec![
            prop("cmakedb:mechanism", "find_package"),
            prop("cmakedb:found", if p.found { "true" } else { "false" }),
            prop(
                "cmakedb:required",
                if p.required { "true" } else { "false" },
            ),
            prop("cmakedb:declared-at", &p.declared_at),
        ];
        if let Some(v) = &p.requested_version {
            props.push(prop("cmakedb:requested-version", v));
        }
        if !p.components.is_empty() {
            props.push(prop("cmakedb:components", &p.components.join(";")));
        }
        if let Some(h) = &p.location_hint {
            props.push(prop("cmakedb:location-hint", h));
        }
        if !p.imported_targets.is_empty() {
            props.push(prop(
                "cmakedb:imported-targets",
                &p.imported_targets.join(";"),
            ));
        }
        let mut c = serde_json::json!({
            "type": "library",
            "name": p.name,
            "properties": props,
        });
        if let Some(v) = &p.resolved_version {
            c["version"] = serde_json::json!(v);
        }
        components.push(c);
    }
    for f in &inv.fetched {
        let mut props = vec![
            prop("cmakedb:mechanism", &f.mechanism),
            prop("cmakedb:pinning", &f.pinning),
            prop("cmakedb:declared-at", &f.declared_at),
        ];
        if let Some(r) = &f.git_repository {
            props.push(prop("cmakedb:git-repository", r));
        }
        if let Some(t) = &f.git_tag {
            props.push(prop("cmakedb:git-tag", t));
        }
        if let Some(u) = &f.url {
            props.push(prop("cmakedb:url", u));
        }
        if let Some(h) = &f.url_hash {
            props.push(prop("cmakedb:url-hash", h));
        }
        let mut c = serde_json::json!({
            "type": "library",
            "name": f.name,
            "properties": props,
        });
        // A pinned commit is an exact version; mutable refs are not
        // versions and are deliberately left out of the version field.
        if f.pinning == "pinned-commit" {
            if let Some(tag) = &f.git_tag {
                c["version"] = serde_json::json!(tag);
                if let Some(repo) = &f.git_repository {
                    if let Some(purl) = github_purl(repo, tag) {
                        c["purl"] = serde_json::json!(purl);
                    }
                }
            }
        }
        components.push(c);
    }
    serde_json::json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "tools": {
                "components": [{
                    "type": "application",
                    "name": "cmakedb",
                    "version": env!("CARGO_PKG_VERSION"),
                }],
            },
            "component": { "type": "application", "name": inv.project },
        },
        "components": components,
    })
}

// -- text ------------------------------------------------------------------

/// Left-ellipsize to `max` chars keeping the tail.
fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

fn requested_str(p: &FindPackage) -> String {
    let mut parts = Vec::new();
    if let Some(v) = &p.requested_version {
        parts.push(v.clone());
    }
    if p.required {
        parts.push("REQUIRED".to_string());
    }
    if p.quiet {
        parts.push("QUIET".to_string());
    }
    if !p.components.is_empty() {
        parts.push(format!("[{}]", p.components.join(",")));
    }
    if !p.optional_components.is_empty() {
        parts.push(format!("[optional: {}]", p.optional_components.join(",")));
    }
    if parts.is_empty() {
        "(any)".to_string()
    } else {
        parts.join(" ")
    }
}

fn resolution_str(p: &FindPackage) -> String {
    let mut s = p.evidence.clone();
    if let Some(v) = &p.resolved_version {
        s.push_str(&format!(", version {v}"));
    }
    if let Some(h) = &p.location_hint {
        s.push_str(&format!(", {h}"));
    }
    s
}

pub fn render_text(inv: &Inventory) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "dependency inventory: {} — {} find_package, {} FetchContent/ExternalProject",
        inv.project,
        inv.find_packages.len(),
        inv.fetched.len()
    );

    let _ = writeln!(out, "\nfind_package:");
    if inv.find_packages.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        let _ = writeln!(
            out,
            "  {:<10} {:<18} {:<28} {:<56} DECLARED AT",
            "STATUS", "PACKAGE", "REQUESTED", "RESOLUTION"
        );
        for p in &inv.find_packages {
            let _ = writeln!(
                out,
                "  {:<10} {:<18} {:<28} {:<56} {}",
                if p.found { "found" } else { "not found" },
                ellipsize(&p.name, 18),
                ellipsize(&requested_str(p), 28),
                ellipsize(&resolution_str(p), 56),
                ellipsize(&p.declared_at, 60)
            );
            if !p.imported_targets.is_empty() {
                let _ = writeln!(out, "    targets: {}", p.imported_targets.join(", "));
            }
        }
    }

    let _ = writeln!(out, "\nFetchContent / ExternalProject:");
    if inv.fetched.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        let _ = writeln!(
            out,
            "  {:<16} {:<18} {:<56} DECLARED AT",
            "PINNING", "NAME", "SOURCE"
        );
        for f in &inv.fetched {
            let source = match (&f.git_repository, &f.git_tag, &f.url) {
                (Some(repo), Some(tag), _) => format!("{repo} @ {tag}"),
                (Some(repo), None, _) => repo.clone(),
                (None, _, Some(url)) => match &f.url_hash {
                    Some(h) => format!("{url} ({h})"),
                    None => url.clone(),
                },
                _ => "(no recognized source)".to_string(),
            };
            let _ = writeln!(
                out,
                "  {:<16} {:<18} {:<56} {}",
                f.pinning,
                ellipsize(&f.name, 18),
                ellipsize(&source, 56),
                ellipsize(&f.declared_at, 60)
            );
        }
    }

    if !inv.unmatched_imported_targets.is_empty() {
        let _ = writeln!(out, "\nimported targets with no matching find_package:");
        for t in &inv.unmatched_imported_targets {
            let _ = writeln!(out, "  {t}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn request_parsing() {
        let r = parse_request(&a(&["3.0", "REQUIRED", "COMPONENTS", "Interpreter"]));
        assert_eq!(r.version.as_deref(), Some("3.0"));
        assert!(r.required && !r.quiet);
        assert_eq!(r.components, vec!["Interpreter"]);

        // Components directly after REQUIRED, keywords stop collection.
        let r = parse_request(&a(&["REQUIRED", "core", "extra", "QUIET"]));
        assert!(r.required && r.quiet);
        assert_eq!(r.components, vec!["core", "extra"]);

        // NAMES/HINTS values must not be mistaken for components.
        let r = parse_request(&a(&["CONFIG", "NAMES", "foo", "HINTS", "/opt"]));
        assert!(r.components.is_empty() && r.version.is_none());

        let r = parse_request(&a(&["1.12...<2.0", "OPTIONAL_COMPONENTS", "x"]));
        assert_eq!(r.version.as_deref(), Some("1.12...<2.0"));
        assert_eq!(r.optional_components, vec!["x"]);
    }

    #[test]
    fn truthiness() {
        for t in ["TRUE", "1", "ON", "YES", "/usr/lib/z.so", "2.11"] {
            assert!(cmake_truthy(t), "{t}");
        }
        for f in ["", "0", "OFF", "FALSE", "NOTFOUND", "Stub-NOTFOUND", "no"] {
            assert!(!cmake_truthy(f), "{f}");
        }
    }

    #[test]
    fn purl_derivation() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        for repo in [
            "https://github.com/Example/Repo.git",
            "https://github.com/example/repo",
            "git@github.com:example/repo.git",
            "ssh://git@github.com/example/repo.git",
        ] {
            assert_eq!(
                github_purl(repo, sha).as_deref(),
                Some(format!("pkg:github/example/repo@{sha}").as_str()),
                "{repo}"
            );
        }
        // Never fabricate: non-github hosts, malformed paths.
        assert_eq!(github_purl("https://gitlab.com/a/b.git", sha), None);
        assert_eq!(github_purl("https://github.com/onlyowner", sha), None);
        assert_eq!(github_purl("https://github.com/a/b/c", sha), None);
    }

    #[test]
    fn version_shapes() {
        assert!(is_version_shape("3.0"));
        assert!(is_version_shape("1.12...<2.0"));
        assert!(!is_version_shape("REQUIRED"));
        assert!(!is_version_shape("core"));
    }
}
