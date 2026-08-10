//! `undefined-reads` (lint roadmap Tier 1): `${X}` expansions in project
//! files that resolved to nothing — the expansion produced an empty
//! string. The high-value subclass is typos (`${PROJEC_NAME}`), surfaced
//! via edit-distance suggestions against names that *are* defined.
//!
//! Names with *any* recorded write are excluded here: a read-before-write
//! of a real variable is `option-after-use`/ordering territory, not an
//! undefined name. Note severity by design — "empty if unset" is a
//! legitimate idiom the trace cannot distinguish from a mistake.

use anyhow::Result;
use cmakedb_db::Db;

use crate::{event_span, Finding, Pass, PassConfig, Severity};

pub struct UndefinedReads;

impl Pass for UndefinedReads {
    fn id(&self) -> &'static str {
        "undefined-reads"
    }
    fn description(&self) -> &'static str {
        "${X} expansions that never resolved to any write (typo candidates)"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        // Names that are never written anywhere (any kind, incl. cache and
        // synthesized params), read via forced ${} expansion in project
        // files, outside try_compile scratch scopes.
        let mut stmt = db.conn.prepare(
            "SELECT r.name, min(r.event_id), count(DISTINCT e.node_id)
             FROM var_reads r
             JOIN events e ON e.id = r.event_id
             JOIN files f ON f.id = e.file_id
             LEFT JOIN scopes s ON s.id = e.scope_id
             WHERE r.read_kind = 'expand' AND r.resolved_write_id IS NULL
               AND f.in_source = 1
               AND NOT EXISTS (SELECT 1 FROM var_writes w WHERE w.name = r.name)
             GROUP BY r.name
             ORDER BY min(r.event_id)",
        )?;
        let rows: Vec<(String, i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if rows.is_empty() {
            return Ok(vec![]);
        }

        // Candidate vocabulary for typo suggestions: every defined name.
        let mut names_stmt = db
            .conn
            .prepare("SELECT DISTINCT name FROM var_writes WHERE length(name) >= 4")?;
        let defined: Vec<String> = names_stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;

        let mut findings = Vec::new();
        for (name, first_event, sites) in rows {
            if cfg.ignored(&name) {
                continue;
            }
            // Conditional-set-then-use idiom: the source contains a write
            // targeting this name in a branch that didn't execute
            // (`if(ARG_PREFIX) set(prefix_option ...)` ... `${prefix_option}`).
            // A typo'd name has no write target anywhere in the AST.
            let conditionally_set: i64 = db.conn.query_row(
                "SELECT count(*) FROM commands c
                 WHERE (c.name_lower IN ('set', 'option')
                        AND (c.args_text = ?1 OR c.args_text LIKE ?1 || ' %'
                             OR c.args_text LIKE '\"' || ?1 || '\" %'))
                    OR (c.name_lower IN ('list', 'string')
                        AND (c.args_text LIKE 'APPEND ' || ?1 || ' %'
                             OR c.args_text LIKE 'PREPEND ' || ?1 || ' %'))",
                [&name],
                |r| r.get(0),
            )?;
            if conditionally_set > 0 {
                continue;
            }
            let suggestion = closest_name(&name, &defined);
            let mut related = Vec::new();
            let mut locs = db.conn.prepare(
                "SELECT DISTINCT r.event_id FROM var_reads r
                 JOIN events e ON e.id = r.event_id
                 JOIN files f ON f.id = e.file_id
                 WHERE r.name = ?1 AND r.read_kind = 'expand'
                   AND r.resolved_write_id IS NULL AND f.in_source = 1
                 ORDER BY r.event_id LIMIT 4",
            )?;
            let events: Vec<i64> = locs
                .query_map([&name], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for e in events.iter().skip(1) {
                related.push((event_span(db, *e), "also expanded to nothing here".into()));
            }
            let hint = match &suggestion {
                Some(s) => format!(" — did you mean `{s}`?"),
                None => String::new(),
            };
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Note,
                message: format!(
                    "`${{{name}}}` expanded to nothing at {sites} site(s); \
                     the variable is never written in this recording{hint}"
                ),
                primary: event_span(db, first_event),
                related,
                fix: None,
            });
        }
        Ok(findings)
    }
}

/// Closest defined name within edit distance 2 (ties broken by distance
/// then lexicographically), requiring reasonably similar lengths so short
/// names don't attract everything.
pub(crate) fn closest_name(name: &str, defined: &[String]) -> Option<String> {
    let mut best: Option<(usize, &String)> = None;
    for cand in defined {
        if cand == name || cand.len().abs_diff(name.len()) > 2 {
            continue;
        }
        let d = levenshtein_capped(name, cand, 2);
        if let Some(d) = d {
            let better = match best {
                Some((bd, bc)) => d < bd || (d == bd && cand < bc),
                None => true,
            };
            if better {
                best = Some((d, cand));
            }
        }
    }
    best.map(|(_, c)| c.clone())
}

/// Levenshtein distance, early-exiting when it must exceed `cap`.
fn levenshtein_capped(a: &str, b: &str, cap: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > cap {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > cap {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    (prev[b.len()] <= cap).then_some(prev[b.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_and_suggestions() {
        assert_eq!(
            levenshtein_capped("ENABLE_LOGING", "ENABLE_LOGGING", 2),
            Some(1)
        );
        assert_eq!(levenshtein_capped("abc", "xyz", 2), None);
        let defined = vec!["ENABLE_LOGGING".to_string(), "ENABLE_LTO".to_string()];
        assert_eq!(
            closest_name("ENABLE_LOGING", &defined),
            Some("ENABLE_LOGGING".to_string())
        );
        assert_eq!(closest_name("TOTALLY_DIFFERENT", &defined), None);
    }
}
