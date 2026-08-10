//! `env-dependence` (lint roadmap Tier 2): configure results depending on
//! ambient environment variables — a non-reproducibility signal.
//!
//! Evidence: `read_kind='env'` rows recorded at ingestion for every
//! `$ENV{X}` reference (and `DEFINED ENV{X}` condition token) on a joined
//! event. A read that resolved against a prior `set(ENV{X} ...)` in the
//! same configure is project-internal (reproducible) and never reported;
//! an *unresolved* read took its value from the ambient environment. The
//! rule reports the read, not what the value influenced — no taint
//! tracking is claimed. Note severity by design: env-driven configuration
//! is often intentional (CI), and expected-ambient names are allowlisted.

use anyhow::Result;
use cmakedb_db::Db;
use std::collections::BTreeMap;

use crate::{event_in_try_compile, event_span, Finding, Pass, PassConfig, Severity};

/// Environment names expected to be ambient: reading them is the normal
/// way projects interact with the platform, toolchain, or CI — not a
/// reproducibility surprise worth reporting. `ignore-patterns` extends
/// this list per project.
const EXPECTED_AMBIENT: &[&str] = &[
    // Platform/session basics.
    "PATH",
    "HOME",
    "USER",
    "TMPDIR",
    "TEMP",
    "TMP",
    "PWD",
    "SHELL",
    "HOSTNAME",
    "NUMBER_OF_PROCESSORS",
    // Windows system locations (both spellings seen in the wild).
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "SystemRoot",
    "SYSTEMROOT",
    "WINDIR",
    "windir",
    // Conventional toolchain/build knobs.
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "MAKEFLAGS",
    "VERBOSE",
    "DESTDIR",
    // Generic CI marker.
    "CI",
];

/// Prefixes expected to be ambient: CMake's own environment knobs
/// (`CMAKE_<LANG>_COMPILER_LAUNCHER`, `CMAKE_PREFIX_PATH`, ...) and the
/// major CI providers' injected variable families.
const EXPECTED_AMBIENT_PREFIXES: &[&str] = &[
    "CMAKE_",
    "GITHUB_",
    "GITLAB_",
    "JENKINS_",
    "TRAVIS_",
    "BUILDKITE_",
];

fn expected_ambient(name: &str) -> bool {
    EXPECTED_AMBIENT.contains(&name)
        || EXPECTED_AMBIENT_PREFIXES
            .iter()
            .any(|p| name.starts_with(p))
}

pub struct EnvDependence;

impl Pass for EnvDependence {
    fn id(&self) -> &'static str {
        "env-dependence"
    }
    fn description(&self) -> &'static str {
        "$ENV{X} reads whose value comes from the ambient environment (non-reproducible input)"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        // Unresolved env reads (no prior set(ENV{X}) in this configure) in
        // project files. try_compile scratch scopes are excluded per event.
        let mut stmt = db.conn.prepare(
            "SELECT r.name, r.event_id FROM var_reads r
             JOIN events e ON e.id = r.event_id
             JOIN files f ON f.id = e.file_id
             WHERE r.read_kind = 'env' AND r.resolved_write_id IS NULL
               AND f.in_source = 1
             ORDER BY r.event_id",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;

        // name -> distinct event ids, in first-read order (BTreeMap for
        // deterministic per-name iteration; events arrive ordered).
        let mut by_name: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for (name, event) in rows {
            if expected_ambient(&name) || cfg.ignored(&name) {
                continue;
            }
            let events = by_name.entry(name).or_default();
            if events.last() != Some(&event) && !event_in_try_compile(db, event) {
                events.push(event);
            }
        }

        let mut findings: Vec<(i64, Finding)> = Vec::new();
        for (name, events) in by_name {
            let Some(&first) = events.first() else {
                continue;
            };
            let related = events
                .iter()
                .skip(1)
                .take(4)
                .map(|e| {
                    (
                        event_span(db, *e),
                        "also read from the environment here".into(),
                    )
                })
                .collect();
            findings.push((
                first,
                Finding {
                    rule: self.id().into(),
                    severity: Severity::Note,
                    message: format!(
                        "`$ENV{{{name}}}` is read at {} site(s) with no prior \
                         `set(ENV{{{name}}} ...)` in this configure — its value comes \
                         from the ambient environment, so configure results can vary \
                         between machines and shells",
                        events.len()
                    ),
                    primary: event_span(db, first),
                    related,
                    fix: None,
                },
            ));
        }
        findings.sort_by_key(|(first, _)| *first);
        Ok(findings.into_iter().map(|(_, f)| f).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_names_and_prefixes() {
        for name in [
            "PATH",
            "CI",
            "ProgramFiles(x86)",
            "CMAKE_PREFIX_PATH",
            "GITHUB_ACTIONS",
        ] {
            assert!(expected_ambient(name), "{name} should be expected-ambient");
        }
        for name in ["MY_CUSTOM_TOOL_ROOT", "PATHS", "XCI", "GITHUB"] {
            assert!(!expected_ambient(name), "{name} should not be allowlisted");
        }
    }
}
