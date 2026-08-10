//! `constant-conditions` (lint roadmap Tier 1): `if`/`elseif`/`while`
//! AST nodes evaluated **at least three times within this recording**
//! whose condition never varied — identical expanded arguments *and*
//! identical resolved variable values on every evaluation. That is a
//! loop-invariant or call-invariant condition: a check inside a
//! `foreach`/`while` body or an often-called function/macro that could
//! not take a different branch no matter how often it ran.
//!
//! Deliberately out of scope: dead-branch claims about conditions that
//! evaluated **once** — the `--also` intersection matches findings by
//! (rule, file, line) without comparing observed values, so a
//! single-evaluation finding would intersect *wrongly* across
//! recordings that took different branches. Multi-evaluation invariance
//! is self-contained evidence within one recording, and cross-recording
//! intersection only strengthens it (see the user guide).
//!
//! Invariance must hold at the *value* level, not just the expanded
//! argument text: CMake's `if()` auto-dereferences bare identifiers
//! *after* argument expansion, so `if(ARG_TARGET)` has byte-identical
//! trace args on every call even when the branch taken differs. The
//! dataflow replay's per-evaluation resolved reads supply the values.
//!
//! Exclusions (each one an intentional construct or an unprovable
//! claim; the last three were discovered validating against LLVM):
//! - conditions with no arguments;
//! - conditions referencing no variables at all (`if(TRUE)` and
//!   friends are deliberate);
//! - conditions whose only variables are never written anywhere in the
//!   recording (invariant by necessity, e.g. string operands) or match
//!   ignore-patterns;
//! - conditions reading a `foreach` loop variable directly (the loop
//!   variable's per-iteration values share one synthesized write, so
//!   invariance cannot be proven for it);
//! - try_compile scratch scopes;
//! - dynamic dereference: an expanded argument naming a variable that
//!   `if()` dereferences again (`if(NOT ${return_var})`) — the real
//!   value is untracked and may vary;
//! - platform pseudo-constants (`APPLE`, `WIN32`, ...): invariant by
//!   necessity on one host;
//! - conditions whose variables were undefined on every evaluation
//!   (fetch-global-into-local guards, assertion guards — and
//!   undefined-reads territory besides).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use cmakedb_db::Db;

use crate::{event_in_try_compile, event_span, Finding, Pass, PassConfig, Severity};

pub struct ConstantConditions;

/// One condition read observation: (variable name, resolved write id).
type Read = (String, Option<i64>);

impl Pass for ConstantConditions {
    fn id(&self) -> &'static str {
        "constant-conditions"
    }
    fn description(&self) -> &'static str {
        "if/elseif/while conditions that never varied across repeated evaluations"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        // 1. Every condition evaluation in project files, grouped by AST
        //    node (one node = one source-level condition; function/macro
        //    bodies re-execute the same node).
        let mut stmt = db.conn.prepare(
            "SELECT e.node_id, e.id, e.cmd_lower, e.args_json
             FROM events e JOIN files f ON f.id = e.file_id
             WHERE e.cmd_lower IN ('if', 'elseif', 'while')
               AND f.in_source = 1 AND e.node_id IS NOT NULL
             ORDER BY e.node_id, e.id",
        )?;
        let rows: Vec<(i64, i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;

        // node -> (cmd, args_json of first eval, event ids, args invariant?)
        let mut groups: Vec<(i64, String, String, Vec<i64>, bool)> = Vec::new();
        for (node, event, cmd, args) in rows {
            match groups.last_mut() {
                Some((n, _, first_args, events, invariant)) if *n == node => {
                    *invariant = *invariant && args == *first_args;
                    events.push(event);
                }
                _ => groups.push((node, cmd, args, vec![event], true)),
            }
        }
        groups.retain(|(_, _, args, events, invariant)| {
            *invariant && events.len() >= 3 && args != "[]"
        });

        // 2. try_compile scratch configures are an isolated world; a node
        //    can be evaluated both inside and outside one.
        for (_, _, _, events, _) in &mut groups {
            events.retain(|e| !event_in_try_compile(db, *e));
        }
        groups.retain(|(_, _, _, events, _)| events.len() >= 3);

        // 3. Per-evaluation resolved reads, one scan (var_reads has no
        //    event_id index by design; the scan is cheaper than per-event
        //    lookups at LLVM scale).
        let wanted: HashSet<i64> = groups
            .iter()
            .flat_map(|(_, _, _, events, _)| events.iter().copied())
            .collect();
        let mut reads: HashMap<i64, Vec<Read>> = HashMap::new();
        let mut rstmt = db.conn.prepare(
            "SELECT r.event_id, r.name, r.resolved_write_id FROM var_reads r
             WHERE r.read_kind IN ('expand', 'condition') ORDER BY r.event_id, r.id",
        )?;
        let mut rrows = rstmt.query([])?;
        while let Some(row) = rrows.next()? {
            let event: i64 = row.get(0)?;
            if wanted.contains(&event) {
                reads
                    .entry(event)
                    .or_default()
                    .push((row.get(1)?, row.get(2)?));
            }
        }
        // Stable order so evaluations of the same node compare positionally.
        for r in reads.values_mut() {
            r.sort_by(|a, b| a.0.cmp(&b.0));
        }

        let mut writes = WriteInfoCache::default();
        let mut written = WrittenCache::default();
        let mut findings = Vec::new();
        for (node, cmd, args_json, events, _) in groups {
            let empty: Vec<Read> = Vec::new();
            let base = reads.get(&events[0]).unwrap_or(&empty);
            if !events
                .iter()
                .skip(1)
                .all(|e| values_invariant(db, base, reads.get(e).unwrap_or(&empty), &mut writes))
            {
                continue;
            }
            // A read resolving to a foreach's loop-variable write is the
            // one case where a single write id spans many values.
            if events.iter().any(|e| {
                reads
                    .get(e)
                    .unwrap_or(&empty)
                    .iter()
                    .any(|(_, wid)| wid.is_some_and(|w| writes.get(db, w).1 == "foreach"))
            }) {
                continue;
            }

            // Dynamic dereference (found on LLVM: `if(NOT ${return_var})`):
            // an expanded argument that is an identifier-like *variable
            // name*, absent from the raw argument tokens, gets a second
            // dereference from if() that the replay does not track — the
            // condition's real value may vary, so claim nothing.
            let raw_text = raw_args_text(db, node);
            if has_dynamic_deref(db, &args_json, &raw_text, &mut written) {
                continue;
            }

            // Variables the condition references at all (static extraction:
            // ${X} refs + bare identifiers, constants/operators excluded).
            let mut names_stmt = db.conn.prepare(
                "SELECT DISTINCT name FROM ast_var_refs WHERE node_id = ?1 ORDER BY name",
            )?;
            let static_names: Vec<String> = names_stmt
                .query_map([node], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            // "Interesting" = not ignore-listed, not a per-machine platform
            // pseudo-constant, and written somewhere in the recording. A
            // name with zero writes anywhere is constant by necessity
            // (string operands of STREQUAL) — a condition on only such
            // names is deliberate, like if(TRUE).
            let interesting: Vec<&String> = static_names
                .iter()
                .filter(|n| !cfg.ignored(n) && !platform_constant(n) && written.get(db, n.as_str()))
                .collect();
            if interesting.is_empty() {
                continue;
            }
            // Require at least one *defined* invariant value. Conditions
            // whose variables were undefined on every evaluation are the
            // fetch-global-into-local and assertion-guard idioms (found on
            // LLVM) and undefined-reads territory besides.
            if !base
                .iter()
                .any(|(n, w)| w.is_some() && !cfg.ignored(n) && !platform_constant(n))
            {
                continue;
            }

            let values = render_values(db, base, &interesting, cfg, &mut writes);
            let cond = condition_text(&raw_text, &cmd);
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Note,
                message: format!(
                    "`{cond}` was evaluated {} times and never varied ({values} on every \
                     evaluation) — a loop- or call-invariant condition",
                    events.len()
                ),
                primary: event_span(db, events[0]),
                related: vec![],
                fix: None,
            });
        }
        Ok(findings)
    }
}

/// Value/origin info per write id: (value if the trace recorded one,
/// cmd_lower of the writing event).
#[derive(Default)]
struct WriteInfoCache(HashMap<i64, (Option<String>, String)>);

impl WriteInfoCache {
    fn get(&mut self, db: &Db, wid: i64) -> &(Option<String>, String) {
        self.0.entry(wid).or_insert_with(|| {
            db.conn
                .query_row(
                    "SELECT w.value, e.cmd_lower FROM var_writes w
                     JOIN events e ON e.id = w.event_id WHERE w.id = ?1",
                    [wid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap_or((None, String::new()))
        })
    }
}

/// Two evaluations observed the same values iff their read lists pair up
/// name-for-name and each pair is either the same write, both-undefined,
/// or two writes with a known identical value (a re-executed `set` of the
/// same string is invariant in effect).
fn values_invariant(db: &Db, a: &[Read], b: &[Read], writes: &mut WriteInfoCache) -> bool {
    if a.len() != b.len() {
        return false; // a name defined on some evaluations only
    }
    a.iter().zip(b.iter()).all(|((an, aw), (bn, bw))| {
        an == bn
            && match (aw, bw) {
                (None, None) => true,
                (Some(x), Some(y)) if x == y => true,
                (Some(x), Some(y)) => {
                    let vx = writes.get(db, *x).0.clone();
                    let vy = writes.get(db, *y).0.clone();
                    vx.is_some() && vx == vy
                }
                _ => false,
            }
    })
}

/// Human-readable invariant values: the first evaluation's resolved reads
/// (deduped, ignore-listed names skipped) plus referenced-but-never-
/// resolved names, which were undefined on every evaluation.
fn render_values(
    db: &Db,
    base: &[Read],
    interesting: &[&String],
    cfg: &PassConfig,
    writes: &mut WriteInfoCache,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for (name, wid) in base {
        if cfg.ignored(name) || platform_constant(name) || !seen.insert(name.as_str()) {
            continue;
        }
        parts.push(match wid {
            Some(w) => match &writes.get(db, *w).0 {
                Some(v) => format!("{name} = \"{v}\""),
                None => format!("{name} = <same unrecorded value>"),
            },
            None => format!("{name} = <undefined>"),
        });
    }
    for name in interesting {
        if seen.insert(name.as_str()) {
            parts.push(format!("{name} = <undefined>"));
        }
    }
    if parts.len() > 5 {
        let more = parts.len() - 5;
        parts.truncate(5);
        parts.push(format!("+{more} more"));
    }
    parts.join(", ")
}

/// The condition as written, `cmd(args...)`, truncated for the message.
fn condition_text(raw_args: &str, cmd: &str) -> String {
    let mut args = raw_args.replace('\n', " ");
    if args.chars().count() > 60 {
        args = format!("{}…", args.chars().take(59).collect::<String>());
    }
    format!("{cmd}({args})")
}

/// Raw (unexpanded) argument text of the condition's AST command.
fn raw_args_text(db: &Db, node: i64) -> String {
    db.conn
        .query_row(
            "SELECT args_text FROM commands WHERE node_id = ?1",
            [node],
            |r| r.get(0),
        )
        .unwrap_or_default()
}

/// Whether any *expanded* argument names a variable that `if()` will
/// dereference again: identifier-like, written somewhere in the recording,
/// and not present as a raw argument token — so it must have come out of a
/// `${...}` expansion the replay's read tracking cannot see through
/// (`is_llvm_target_library(${lib} ${return_var})` … `if(NOT ${return_var})`).
fn has_dynamic_deref(db: &Db, args_json: &str, raw_text: &str, written: &mut WrittenCache) -> bool {
    let args: Vec<String> = serde_json::from_str(args_json).unwrap_or_default();
    let raw_tokens: HashSet<&str> = raw_text
        .split_whitespace()
        .map(|t| t.trim_matches('"'))
        .collect();
    args.iter().any(|a| {
        identifier_like(a) && !raw_tokens.contains(a.as_str()) && written.get(db, a.as_str())
    })
}

/// Same identifier shape the ingester's condition extraction accepts.
fn identifier_like(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.is_ascii_alphabetic() || c == '_')
        .unwrap_or(false)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
}

/// Per-machine platform pseudo-constants: invariant by necessity on one
/// host, so a condition on them is deliberate, not a latent bug. (CMake
/// sets several of these before tracing starts; others are written by the
/// platform modules, so written-anywhere does not exclude them.)
fn platform_constant(name: &str) -> bool {
    matches!(
        name,
        "WIN32"
            | "WIN64"
            | "APPLE"
            | "UNIX"
            | "LINUX"
            | "CYGWIN"
            | "MSYS"
            | "MINGW"
            | "ANDROID"
            | "IOS"
            | "BSD"
            | "AIX"
            | "HAIKU"
            | "EMSCRIPTEN"
            | "MSVC"
            | "MSVC_IDE"
            | "MSVC_VERSION"
            | "MSVC_TOOLSET_VERSION"
            | "BORLAND"
            | "XCODE"
            | "GHSMULTI"
    )
}

/// Cached "is this name written anywhere in the recording" lookups.
#[derive(Default)]
struct WrittenCache(HashMap<String, bool>);

impl WrittenCache {
    fn get(&mut self, db: &Db, name: &str) -> bool {
        *self.0.entry(name.to_string()).or_insert_with(|| {
            db.conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM var_writes WHERE name = ?1)",
                    [name],
                    |r| r.get::<_, i64>(0),
                )
                .map(|v| v != 0)
                .unwrap_or(false)
        })
    }
}
