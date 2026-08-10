//! `genex-in-wrong-context` (lint roadmap Tier 1): a literal `$<...>`
//! generator expression reaching a command that never evaluates genexes —
//! the expression is being compared, printed, written, or executed as raw
//! text at configure time. `if("$<CONFIG:Debug>" STREQUAL ...)` is the
//! canonical silent bug: the condition compares the literal string.
//!
//! Evidence: expanded trace args still containing `$<` in a curated list
//! of genex-NON-evaluating commands. Genex-aware commands (`target_*`,
//! `add_custom_command`, `install`, `file(GENERATE)`, ...) are never
//! candidates. Storing commands (`set`, `list`, most `string`
//! subcommands) are deliberate genex *string-building* and are never
//! flagged directly: if the stored value later reaches a genex-aware sink
//! that is fine, and if it reaches a non-evaluating command the sink's
//! own expanded args contain the genex, so the read site is what gets
//! flagged — with the storing site attached as related evidence via the
//! dataflow tables.

use anyhow::Result;
use cmakedb_db::Db;

use crate::{event_in_try_compile, event_span, Finding, Pass, PassConfig, Severity};

pub struct GenexInWrongContext;

impl Pass for GenexInWrongContext {
    fn id(&self) -> &'static str {
        "genex-in-wrong-context"
    }
    fn description(&self) -> &'static str {
        "literal $<...> genexes reaching commands that never evaluate them"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut stmt = db.conn.prepare(
            "SELECT e.id, e.cmd_lower, e.args_json FROM events e
             JOIN files f ON f.id = e.file_id
             WHERE f.in_source = 1
               AND e.cmd_lower IN ('if','elseif','while','message','string',
                                   'math','execute_process','file',
                                   'configure_file')
               AND e.args_json LIKE '%$<%'
             ORDER BY e.id",
        )?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;

        // Storing site(s) whose value fed this event's expansion — the
        // provenance hop for genexes that arrived via `${X}`.
        let mut sources = db.conn.prepare(
            "SELECT DISTINCT w.event_id, r.name, w.value FROM var_reads r
             JOIN var_writes w ON w.id = r.resolved_write_id
             WHERE r.event_id = ?1 AND w.value LIKE '%$<%'
             ORDER BY w.event_id LIMIT 4",
        )?;

        let mut findings = Vec::new();
        for (event_id, cmd, args_json) in rows {
            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            let Some((severity, what, why)) = classify(&cmd, &args) else {
                continue;
            };
            let Some(genex) = args.iter().find_map(|a| first_genex(a)) else {
                continue;
            };
            if event_in_try_compile(db, event_id) {
                continue;
            }

            let stored: Vec<(i64, String)> = sources
                .query_map([event_id], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<(i64, String, Option<String>)>>>()?
                .into_iter()
                .filter(|(_, _, v)| v.as_deref().and_then(first_genex).is_some())
                .map(|(e, n, _)| (e, n))
                .collect();
            // Tuning knob: genexes fed by ignore-pattern variables
            // (CMAKE_.* by default) are someone else's string-building.
            if stored.iter().any(|(_, name)| cfg.ignored(name)) {
                continue;
            }
            let related = stored
                .iter()
                .map(|(e, name)| {
                    (
                        event_span(db, *e),
                        format!("the genex was stored into `{name}` here and reached this command via expansion"),
                    )
                })
                .collect();

            findings.push(Finding {
                rule: self.id().into(),
                severity,
                message: format!("literal generator expression `{genex}` in {what} — {why}"),
                primary: event_span(db, event_id),
                related,
                fix: None,
            });
        }
        Ok(findings)
    }
}

/// Whether this (command, args) pair is a genex-non-evaluating context we
/// report on, and how. Returns None for commands/subcommands that are
/// genex-aware or that store the genex for later use (string-building).
fn classify(cmd: &str, args: &[String]) -> Option<(Severity, String, &'static str)> {
    match cmd {
        "if" | "elseif" | "while" => Some((
            Severity::Warning,
            format!("an `{cmd}()` condition"),
            "conditions are evaluated at configure time and never expand genexes; \
             the raw `$<...>` text is what gets compared",
        )),
        "execute_process" => Some((
            Severity::Warning,
            "`execute_process()` arguments".to_string(),
            "the child process receives the raw text; genexes only evaluate at \
             generate time",
        )),
        "message" => Some((
            Severity::Note,
            "`message()` output".to_string(),
            "it prints as raw text; genexes only evaluate at generate time",
        )),
        "configure_file" => Some((
            Severity::Note,
            "`configure_file()` arguments".to_string(),
            "configure_file substitutes @VAR@/${VAR} only; the genex is copied \
             through as raw text",
        )),
        "math" => Some((
            Severity::Note,
            "a `math()` expression".to_string(),
            "math() operates on the raw text and cannot evaluate genexes",
        )),
        "file" => match args.first().map(String::as_str) {
            // Only configure-time content/IO subcommands: file(GENERATE)
            // is genex-aware and every other subcommand is path plumbing.
            Some("WRITE" | "APPEND") => Some((
                Severity::Note,
                format!("`file({})`", args[0]),
                "the raw text lands in the file at configure time; file(GENERATE) \
                 is the genex-aware alternative",
            )),
            Some("READ") => Some((
                Severity::Note,
                "`file(READ)`".to_string(),
                "the genex is treated as literal path/argument text at configure time",
            )),
            _ => None,
        },
        // string(): every mutating/building subcommand is deliberate genex
        // string manipulation (the value's eventual sink is judged
        // instead); only text comparison is a configure-time misuse.
        "string" => match args.first().map(String::as_str) {
            Some("COMPARE") => Some((
                Severity::Note,
                "`string(COMPARE)`".to_string(),
                "the comparison operates on the raw text; genexes only evaluate \
                 at generate time",
            )),
            _ => None,
        },
        _ => None,
    }
}

/// First complete literal genex `$<...>` in `s` (nesting-aware), or None.
/// `\$<` is excluded: an escaped dollar is the regex idiom for *detecting*
/// genexes (`if(x MATCHES "\\$<")`), not a genex. A lone `$<` with no
/// closing `>` (e.g. a `string(FIND ... "$<")` probe) does not count.
fn first_genex(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut from = 0;
    while let Some(pos) = s[from..].find("$<") {
        let start = from + pos;
        from = start + 2;
        if start > 0 && b[start - 1] == b'\\' {
            continue; // escaped: regex/pattern text
        }
        // A genex head is `$<NAME...` or a nested `$<$<...`.
        match s[start + 2..].chars().next() {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '$' => {}
            _ => continue,
        }
        // Scan for the matching `>`, tracking `$<` nesting.
        let mut depth = 1u32;
        let mut i = start + 2;
        while i < b.len() {
            if b[i] == b'$' && i + 1 < b.len() && b[i + 1] == b'<' {
                depth += 1;
                i += 2;
            } else {
                if b[i] == b'>' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(s[start..=i].to_string());
                    }
                }
                i += 1;
            }
        }
        // Unterminated: keep looking after this `$<`.
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genex_extraction() {
        assert_eq!(
            first_genex("x $<CONFIG:Debug> y").as_deref(),
            Some("$<CONFIG:Debug>")
        );
        assert_eq!(
            first_genex("$<$<CONFIG:Debug>:-g>").as_deref(),
            Some("$<$<CONFIG:Debug>:-g>")
        );
        // Escaped dollar = genex-detection regex, not a genex.
        assert_eq!(first_genex("\\$<"), None);
        assert_eq!(first_genex("^\\$<CONFIG"), None);
        // Unterminated probe text and plain strings.
        assert_eq!(first_genex("$<"), None);
        assert_eq!(first_genex("a < b > c"), None);
        // Head must open a real genex.
        assert_eq!(first_genex("$< >"), None);
        // Later well-formed genex after an unterminated/escaped one.
        assert_eq!(
            first_genex("\\$< then $<TARGET_FILE:foo> end").as_deref(),
            Some("$<TARGET_FILE:foo>")
        );
    }

    #[test]
    fn context_classification() {
        let a = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            classify("if", &a(&["$<CONFIG:Debug>"])).unwrap().0,
            Severity::Warning
        );
        assert_eq!(
            classify("execute_process", &a(&["COMMAND"])).unwrap().0,
            Severity::Warning
        );
        assert_eq!(
            classify("message", &a(&["STATUS"])).unwrap().0,
            Severity::Note
        );
        // file(GENERATE) is genex-aware; WRITE is not.
        assert!(classify("file", &a(&["GENERATE", "OUTPUT"])).is_none());
        assert!(classify("file", &a(&["WRITE", "out.txt"])).is_some());
        // string-building subcommands are never flagged directly.
        assert!(classify("string", &a(&["REPLACE", "$<", ""])).is_none());
        assert!(classify("string", &a(&["APPEND", "flags"])).is_none());
        assert!(classify("string", &a(&["COMPARE", "EQUAL"])).is_some());
        // Storing/aware commands are not contexts at all.
        assert!(classify("set", &a(&["X"])).is_none());
        assert!(classify("target_link_libraries", &a(&["t"])).is_none());
    }
}
