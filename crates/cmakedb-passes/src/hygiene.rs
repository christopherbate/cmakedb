//! Cache/dependency hygiene passes (lint roadmap Tier 1).
//!
//! `set-cache-force`: `set(... CACHE <type> <doc> FORCE)` in project files
//! overwrites whatever the user configured (`-D`, ccmake edit) on every
//! configure — the classic stomp anti-pattern. `INTERNAL` entries are
//! exempt (FORCE is idiomatic bookkeeping there), and two low-harm shapes
//! are demoted to note: writes guarded by a condition on the variable
//! itself (`if(NOT DEFINED X) ... FORCE`), and entries with an empty
//! docstring (the "cache as global variable" idiom — the fix is INTERNAL).
//!
//! `fetchcontent-pinning`: `FetchContent_Declare`/`ExternalProject_Add`
//! whose source is not reproducibly pinned — a git clone with no
//! `GIT_TAG`, a `GIT_TAG` that is not a full 40-hex commit hash, or a
//! `URL` download without `URL_HASH`/`URL_MD5`. Note severity by design:
//! tracking a branch is a legitimate dev-mode choice.

use anyhow::Result;
use cmakedb_db::Db;
use std::collections::HashMap;

use crate::{event_in_try_compile, event_span, Finding, Pass, PassConfig, Severity};

pub struct SetCacheForce;

impl Pass for SetCacheForce {
    fn id(&self) -> &'static str {
        "set-cache-force"
    }
    fn description(&self) -> &'static str {
        "set(... CACHE ... FORCE) stomping user configuration on every configure"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut stmt = db.conn.prepare(
            "SELECT e.id, e.node_id, e.args_json FROM events e
             JOIN files f ON f.id = e.file_id
             WHERE e.cmd_lower = 'set' AND f.in_source = 1
               AND e.args_json LIKE '%\"CACHE\"%' AND e.args_json LIKE '%\"FORCE\"%'
             ORDER BY e.id",
        )?;
        let rows: Vec<(i64, Option<i64>, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;

        // One site (AST node) can execute many times with different
        // expanded names (`set(${var} ... FORCE)` in a function) — group
        // by node and report each site once.
        struct Site {
            first_event: i64,
            names: Vec<String>,
            cache_type: String,
            empty_doc: bool,
        }
        let mut sites: HashMap<(Option<i64>, i64), Site> = HashMap::new();
        for (event, node, args_json) in rows {
            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            let Some((cache_type, doc, forced)) = cache_signature(&args) else {
                continue;
            };
            // INTERNAL caches are invisible bookkeeping; FORCE is idiomatic.
            if !forced || cache_type == "INTERNAL" {
                continue;
            }
            let Some(name) = args.first().filter(|n| !n.is_empty()) else {
                continue;
            };
            if cfg.ignored(name) {
                continue;
            }
            let key = (node, if node.is_some() { 0 } else { event });
            let site = sites.entry(key).or_insert_with(|| Site {
                first_event: event,
                names: Vec::new(),
                cache_type: cache_type.to_string(),
                empty_doc: doc.is_empty(),
            });
            if !site.names.contains(name) {
                site.names.push(name.clone());
            }
        }

        let mut ordered: Vec<&Site> = sites.values().collect();
        ordered.sort_by_key(|s| s.first_event);

        let mut findings = Vec::new();
        for site in ordered {
            // try_compile scratch configures write into an isolated cache.
            if event_in_try_compile(db, site.first_event) {
                continue;
            }
            let name = &site.names[0];
            let node: Option<i64> = db
                .conn
                .query_row(
                    "SELECT node_id FROM events WHERE id = ?1",
                    [site.first_event],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let guarded = node.is_some_and(|n| {
                site.names
                    .iter()
                    .all(|nm| guarded_by_condition_on(db, n, nm))
            });

            let also = if site.names.len() > 1 {
                format!(
                    " ({} more variable(s) written at this site)",
                    site.names.len() - 1
                )
            } else {
                String::new()
            };
            let (severity, qualifier) = if guarded {
                (
                    Severity::Note,
                    " — guarded by a condition on the variable, so it only \
                     runs when the cache entry is absent (low harm, but the \
                     guard makes FORCE redundant)",
                )
            } else if site.empty_doc {
                (
                    Severity::Note,
                    " — the empty docstring suggests a derived/internal \
                     value; declare it `CACHE INTERNAL` instead",
                )
            } else {
                (
                    Severity::Warning,
                    " — drop FORCE, or use `CACHE INTERNAL` if the value is \
                     derived rather than user configuration",
                )
            };
            findings.push(Finding {
                rule: self.id().into(),
                severity,
                message: format!(
                    "set({name} ... CACHE {} ... FORCE) overwrites the \
                     user's cache value on every configure{also}{qualifier}",
                    site.cache_type
                ),
                primary: event_span(db, site.first_event),
                related: vec![],
                fix: None,
            });
        }
        Ok(findings)
    }
}

/// Parse the cache tail of a `set` argument list: returns
/// `(type, docstring, has_force)` when the args contain a well-formed
/// `CACHE <type> <doc> [FORCE]` suffix.
fn cache_signature(args: &[String]) -> Option<(&str, &str, bool)> {
    const TYPES: [&str; 5] = ["BOOL", "STRING", "PATH", "FILEPATH", "INTERNAL"];
    let pos = args
        .iter()
        .position(|a| a == "CACHE")
        .filter(|&p| p > 0 && args.get(p + 1).is_some_and(|t| TYPES.contains(&t.as_str())))?;
    let ty = args[pos + 1].as_str();
    let doc = args.get(pos + 2).map(|s| s.as_str())?;
    let forced = args[pos + 3..].iter().any(|a| a == "FORCE");
    Some((ty, doc, forced))
}

/// Whether the AST node sits inside an `if`/`elseif` chain whose condition
/// mentions `name` (`if(NOT DEFINED X)` / `if(NOT X)` guard idiom): the
/// FORCE write only runs when the guard passes, so it cannot stomp a
/// value the user provided up front.
fn guarded_by_condition_on(db: &Db, node_id: i64, name: &str) -> bool {
    let mut cur = Some(node_id);
    for _ in 0..64 {
        let Some(id) = cur else { return false };
        let row: Option<(String, Option<i64>)> = db
            .conn
            .query_row(
                "SELECT kind, parent_id FROM ast_nodes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((kind, parent)) = row else {
            return false;
        };
        if kind == "if_condition" {
            let mut stmt = match db.conn.prepare(
                "SELECT c.args_text FROM ast_nodes n
                 JOIN commands c ON c.node_id = n.id
                 WHERE n.parent_id = ?1 AND c.name_lower IN ('if', 'elseif')",
            ) {
                Ok(s) => s,
                Err(_) => return false,
            };
            let conds: Vec<String> = stmt
                .query_map([id], |r| r.get(0))
                .and_then(|m| m.collect::<rusqlite::Result<_>>())
                .unwrap_or_default();
            if conds.iter().any(|c| mentions_word(c, name)) {
                return true;
            }
        }
        cur = parent;
    }
    false
}

/// Whole-identifier containment: `name` appears in `text` delimited by
/// non-identifier characters (catches both `NOT DEFINED X` and `${X}`).
fn mentions_word(text: &str, name: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|tok| tok == name)
}

pub struct FetchContentPinning;

impl Pass for FetchContentPinning {
    fn id(&self) -> &'static str {
        "fetchcontent-pinning"
    }
    fn description(&self) -> &'static str {
        "FetchContent/ExternalProject sources not pinned to a commit hash or archive hash"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut stmt = db.conn.prepare(
            "SELECT e.id, e.node_id, e.cmd, e.args_json FROM events e
             JOIN files f ON f.id = e.file_id
             WHERE e.cmd_lower IN ('fetchcontent_declare', 'externalproject_add')
               AND f.in_source = 1
             ORDER BY e.id",
        )?;
        let rows: Vec<(i64, Option<i64>, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut seen = std::collections::HashSet::new();
        let mut findings = Vec::new();
        for (event, node, cmd, args_json) in rows {
            // One finding per declaration site, not per re-execution.
            if !seen.insert((node, if node.is_some() { 0 } else { event })) {
                continue;
            }
            if event_in_try_compile(db, event) {
                continue;
            }
            // json-v1 shows unquoted args that expanded to nothing as ""
            // even though the command never received them — drop them so
            // keyword/value pairing matches what cmake actually parsed.
            let args: Vec<String> = serde_json::from_str::<Vec<String>>(&args_json)
                .unwrap_or_default()
                .into_iter()
                .filter(|a| !a.is_empty())
                .collect();
            let Some(name) = args.first() else { continue };
            if cfg.ignored(name) {
                continue;
            }
            let value_of = |kw: &str| {
                args.iter()
                    .position(|a| a == kw)
                    .and_then(|p| args.get(p + 1))
            };

            let message = if value_of("GIT_REPOSITORY").is_some() {
                match value_of("GIT_TAG") {
                    None => Some(format!(
                        "{cmd}({name}) has a GIT_REPOSITORY but no GIT_TAG — \
                         it tracks the remote's default branch; pin a full \
                         40-character commit hash for reproducible builds"
                    )),
                    Some(tag) if !is_commit_sha(tag) => Some(format!(
                        "{cmd}({name}) GIT_TAG `{tag}` is not a pinned commit \
                         hash — branches and tags can move or be rewritten; \
                         pin the full 40-character commit hash"
                    )),
                    Some(_) => None,
                }
            } else if let Some(url) = value_of("URL") {
                if value_of("URL_HASH").is_none() && value_of("URL_MD5").is_none() {
                    Some(format!(
                        "{cmd}({name}) downloads {url} without URL_HASH — the \
                         archive contents are not integrity-checked or pinned"
                    ))
                } else {
                    None
                }
            } else {
                // SOURCE_DIR / SVN / CVS / custom flows: out of scope.
                None
            };
            if let Some(message) = message {
                findings.push(Finding {
                    rule: self.id().into(),
                    severity: Severity::Note,
                    message,
                    primary: event_span(db, event),
                    related: vec![],
                    fix: None,
                });
            }
        }
        Ok(findings)
    }
}

/// A pinned git ref: exactly 40 hex characters (a full commit SHA).
/// Public (re-exported at the crate root) so `cmakedb deps` classifies
/// pinning with exactly the same test as this lint.
pub fn is_commit_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_sha_shape() {
        assert!(is_commit_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_commit_sha("main"));
        assert!(!is_commit_sha("v1.2.3"));
        assert!(!is_commit_sha("0123456")); // short sha
        assert!(!is_commit_sha("0123456789abcdef0123456789abcdef0123456g"));
    }

    #[test]
    fn cache_signature_shapes() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            cache_signature(&a(&["X", "v", "CACHE", "STRING", "doc", "FORCE"])),
            Some(("STRING", "doc", true))
        );
        assert_eq!(
            cache_signature(&a(&["X", "v", "CACHE", "BOOL", "doc"])),
            Some(("BOOL", "doc", false))
        );
        // CACHE as a plain value, not a keyword with a valid type.
        assert_eq!(cache_signature(&a(&["X", "CACHE", "not-a-type"])), None);
        // Name position cannot be the CACHE keyword.
        assert_eq!(cache_signature(&a(&["CACHE", "BOOL", "doc"])), None);
    }

    #[test]
    fn word_mentions() {
        assert!(mentions_word(
            "NOT DEFINED HYGIENE_GUARDED",
            "HYGIENE_GUARDED"
        ));
        assert!(mentions_word("NOT ${FOO}", "FOO"));
        assert!(!mentions_word("NOT FOO_BAR", "FOO"));
    }
}
