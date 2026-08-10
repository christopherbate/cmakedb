//! Policy-hygiene pass (lint roadmap Tier 2).
//!
//! `policy-hygiene`: explicit `cmake_policy(SET CMPxxxx OLD)` pins in
//! project files. An OLD pin freezes deprecated behavior with a removal
//! clock attached — CMake deprecates every OLD behavior by definition and
//! eventually turns the pin into a hard configure error (e.g. CMake 4.0
//! removed OLD for every policy introduced before 3.5). Each pin is
//! therefore tracked tech debt, reported at warning severity.
//!
//! Division of labor with `configure-warnings`: that rule carries CMake's
//! *own* words about policies that are **not set** ("Policy CMPnnnn is not
//! set", rows in `configure_diagnostics`); this rule owns **explicit OLD
//! pins** proven by the trace. The two never overlap — a pinned policy is
//! set, so CMake emits no unset-policy advice for it — and this pass never
//! queries `configure_diagnostics`, so it runs unchanged on databases
//! recorded before that table existed. `cmake_policy(VERSION ...)` /
//! `cmake_minimum_required` interplay is deliberately out of scope.

use anyhow::Result;
use cmakedb_db::Db;

use crate::{event_in_try_compile, event_span, Finding, Pass, PassConfig, Severity};

/// Curated metadata for well-known policies: (policy, NEW-behavior
/// description, CMake version that introduced it). The table is curated
/// and additive — it covers the policies most commonly pinned OLD in the
/// wild, not the full policy list; extend it as new pins show up.
/// Unknown policy numbers still produce a finding, just without the
/// per-policy context.
const KNOWN_POLICIES: &[(&str, &str, &str)] = &[
    (
        "CMP0022",
        "INTERFACE_LINK_LIBRARIES defines the link interface",
        "2.8.12",
    ),
    ("CMP0042", "MACOSX_RPATH is enabled by default", "3.0"),
    ("CMP0048", "project() manages the VERSION variables", "3.0"),
    (
        "CMP0054",
        "if() only dereferences unquoted variable names",
        "3.1",
    ),
    (
        "CMP0060",
        "libraries are linked by full path even in implicit directories",
        "3.3",
    ),
    (
        "CMP0063",
        "visibility properties are honored for all target types",
        "3.3",
    ),
    (
        "CMP0067",
        "try_compile() honors the language standard variables",
        "3.8",
    ),
    (
        "CMP0074",
        "find_package() uses <PackageName>_ROOT variables",
        "3.12",
    ),
    ("CMP0077", "option() honors normal variables", "3.13"),
    (
        "CMP0079",
        "target_link_libraries() may reference targets from other directories",
        "3.13",
    ),
    (
        "CMP0091",
        "MSVC runtime library is selected via CMAKE_MSVC_RUNTIME_LIBRARY",
        "3.15",
    ),
    (
        "CMP0104",
        "CMAKE_CUDA_ARCHITECTURES must be set and is initialized by default",
        "3.18",
    ),
    (
        "CMP0116",
        "add_custom_command() DEPFILE paths are transformed for Ninja",
        "3.20",
    ),
    (
        "CMP0126",
        "set(CACHE) does not remove a normal variable of the same name",
        "3.21",
    ),
    (
        "CMP0135",
        "ExternalProject/FetchContent URL downloads use extraction-time timestamps",
        "3.24",
    ),
];

pub struct PolicyHygiene;

impl Pass for PolicyHygiene {
    fn id(&self) -> &'static str {
        "policy-hygiene"
    }
    fn description(&self) -> &'static str {
        "cmake_policy(SET ... OLD) pins: deprecated behavior kept alive as tech debt"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();

        // Project-file cmake_policy events only; CMake's own scratch
        // configures (try_compile) pin policies OLD constantly and are
        // filtered by both in_source and the scope check below.
        let mut stmt = db.conn.prepare(
            "SELECT e.id, e.args_json FROM events e
             JOIN files f ON f.id = e.file_id
             WHERE e.cmd_lower = 'cmake_policy' AND f.in_source = 1
             ORDER BY e.id",
        )?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;

        for (event_id, args_json) in rows {
            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            // Keywords are case-sensitive in real cmake: exactly SET/OLD.
            let [mode, policy, behavior] = args.as_slice() else {
                continue;
            };
            if mode != "SET" || behavior != "OLD" {
                continue;
            }
            if cfg.ignored(policy) || event_in_try_compile(db, event_id) {
                continue;
            }
            let message = match KNOWN_POLICIES.iter().find(|(p, _, _)| p == policy) {
                Some((_, desc, since)) => format!(
                    "{policy} is pinned to OLD behavior, rejecting the NEW behavior \
                     introduced in CMake {since} ({desc}) — OLD behaviors are \
                     deprecated by definition and removed in later CMake versions"
                ),
                None => format!(
                    "{policy} is pinned to OLD behavior — OLD behaviors are \
                     deprecated by definition and removed in later CMake versions"
                ),
            };
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Warning,
                message,
                primary: event_span(db, event_id),
                related: Vec::new(),
                fix: None,
            });
        }
        Ok(findings)
    }
}
