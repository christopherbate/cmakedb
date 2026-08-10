//! L4 analysis passes (design §3.4).
//!
//! Each pass is a pure function over the database — no filesystem or
//! process access — returning [`Finding`]s that map 1:1 onto SARIF results.

// Row tuples straight out of SQL queries are clearer inline than named
// type aliases; the DFS carries its context explicitly on purpose.
#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]

pub mod history;
pub mod provenance;
pub mod report;
pub mod sql_pass;
pub mod whynot;

mod aliasvars;
mod clobbers;
mod configure_warnings;
mod constconds;
mod dead;
mod dircommands;
mod envdep;
mod execproc;
mod genex;
mod hygiene;
mod linkgraph;
pub mod modernize;
mod noopcalls;
mod ordering;
mod overshare;
mod policy;
mod redundantlinks;
mod scope_leaks;
mod shadowedreqs;
mod singleuse;
pub(crate) mod undefined;

// Shared with `cmakedb deps` so the SBOM's pinning classification and the
// fetchcontent-pinning lint can never disagree on what "pinned" means.
pub use hygiene::is_commit_sha;

use anyhow::Result;
use cmakedb_db::Db;
use cmakedb_patch::Patch;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Note,
    Warning,
    Error,
}

impl Severity {
    pub fn sarif_level(self) -> &'static str {
        match self {
            Severity::Note => "note",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

impl std::str::FromStr for Severity {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "note" => Ok(Severity::Note),
            "warning" => Ok(Severity::Warning),
            "error" => Ok(Severity::Error),
            other => anyhow::bail!("unknown severity '{other}'"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSpan {
    pub file: String,
    pub line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col: Option<i64>,
}

impl SourceSpan {
    pub fn new(file: impl Into<String>, line: i64) -> SourceSpan {
        SourceSpan {
            file: file.into(),
            line,
            col: None,
        }
    }
}

impl std::fmt::Display for SourceSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file, self.line)?;
        if let Some(c) = self.col {
            write!(f, ":{c}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub rule: String,
    pub severity: Severity,
    pub message: String,
    pub primary: SourceSpan,
    /// Provenance hops / evidence: (location, note).
    #[serde(default)]
    pub related: Vec<(SourceSpan, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<Patch>,
}

/// Per-pass configuration resolved from `.cmakedb.toml` (§2.5).
#[derive(Debug, Clone, Default)]
pub struct PassConfig {
    /// Compiled `ignore-patterns` (full-match on variable/function names).
    pub ignore: Vec<regex::Regex>,
    /// Pass-specific free-form options.
    pub options: serde_json::Value,
}

impl PassConfig {
    pub fn ignored(&self, name: &str) -> bool {
        self.ignore
            .iter()
            .any(|r| r.find(name).map(|m| m.len() == name.len()).unwrap_or(false))
    }
}

pub trait Pass {
    fn id(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>>;
}

/// All built-in passes, in reporting order.
pub fn builtin_passes() -> Vec<Box<dyn Pass>> {
    vec![
        Box::new(dead::DeadOptions),
        Box::new(dead::DeadFunctions),
        Box::new(dead::DeadModules),
        Box::new(dead::DeadVariables),
        Box::new(dircommands::DirectoryCommands),
        Box::new(ordering::OptionAfterUse),
        Box::new(undefined::UndefinedReads),
        Box::new(genex::GenexInWrongContext),
        Box::new(execproc::ExecuteProcessUnchecked),
        Box::new(constconds::ConstantConditions),
        Box::new(envdep::EnvDependence),
        Box::new(clobbers::Clobbers),
        Box::new(policy::PolicyHygiene),
        Box::new(scope_leaks::ScopeLeaks),
        Box::new(overshare::Overshare),
        Box::new(configure_warnings::ConfigureWarnings),
        Box::new(linkgraph::DuplicateLinks),
        Box::new(linkgraph::CyclicLinks),
        Box::new(redundantlinks::RedundantLinks),
        Box::new(hygiene::SetCacheForce),
        Box::new(hygiene::FetchContentPinning),
        Box::new(singleuse::SingleUseVariables),
        Box::new(aliasvars::AliasVariables),
        Box::new(noopcalls::NoopCalls),
        Box::new(shadowedreqs::ShadowedRequirements),
    ]
}

/// Default ignore patterns applied to variable-name passes when the config
/// doesn't override them (§2.5 example).
pub fn default_ignore_patterns() -> Vec<&'static str> {
    vec![
        "CMAKE_.*",
        "_CMAKE_.*",
        ".*_FOUND",
        ".*_DIR",
        ".*_VERSION.*",
        "PROJECT_.*",
        "ARG[CVN].*",
    ]
}

// -- shared helpers used by pass implementations ---------------------------

pub(crate) fn event_span(db: &Db, event_id: i64) -> SourceSpan {
    let loc: Option<(String, i64)> = db
        .conn
        .query_row(
            "SELECT f.path, e.line FROM events e JOIN files f ON f.id = e.file_id
             WHERE e.id = ?1",
            [event_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    match loc {
        Some((path, line)) => SourceSpan::new(db.display_path(&path), line),
        None => SourceSpan::new("<unknown>", 0),
    }
}

/// Whether an event's AST node sits inside a loop construct (used to
/// suppress false clobbers, §4.2).
pub(crate) fn event_in_loop(db: &Db, event_id: i64) -> bool {
    let node: Option<i64> = db
        .conn
        .query_row(
            "SELECT node_id FROM events WHERE id = ?1",
            [event_id],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    let Some(mut cur) = node else { return false };
    for _ in 0..64 {
        let row: Option<(Option<i64>, String)> = db
            .conn
            .query_row(
                "SELECT parent_id, kind FROM ast_nodes WHERE id = ?1",
                [cur],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((parent, kind)) = row else {
            return false;
        };
        if kind == "foreach_loop" || kind == "while_loop" {
            return true;
        }
        match parent {
            Some(p) => cur = p,
            None => return false,
        }
    }
    false
}

/// Whether the event's scope chain passes through a try_compile scope —
/// scratch configures are an isolated variable/cache world (see AGENTS.md)
/// and must not be linted as project behavior.
pub(crate) fn event_in_try_compile(db: &Db, event_id: i64) -> bool {
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

/// True when the event's file is part of the recorded project source tree
/// (passes only report on project code, not CMake's own modules).
pub(crate) fn event_in_source(db: &Db, event_id: i64) -> bool {
    db.conn
        .query_row(
            "SELECT f.in_source FROM events e JOIN files f ON f.id = e.file_id
             WHERE e.id = ?1",
            [event_id],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .unwrap_or(false)
}
