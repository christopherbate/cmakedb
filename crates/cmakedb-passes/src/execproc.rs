//! `execute-process-unchecked` (lint roadmap Tier 1): `execute_process()`
//! calls at configure time whose failure would be silently ignored — no
//! `COMMAND_ERROR_IS_FATAL`, and either no `RESULT_VARIABLE` at all or a
//! result variable that is written but never read afterwards.
//!
//! Evidence: event args (keyword presence) × the dataflow tables — the
//! ingester synthesizes a `var_writes` row for `RESULT_VARIABLE` /
//! `RESULTS_VARIABLE` targets, so "checked" means some `var_reads` row
//! resolves to that write at a later event. Note severity by design:
//! genuinely best-effort probing (optional tools, version sniffing) is
//! the documented false-positive class and the trace cannot distinguish
//! it from a forgotten check. `ERROR_QUIET` is mentioned as aggravating
//! context when present (the failure would not even print), but is not
//! itself a trigger.

use anyhow::Result;
use cmakedb_db::Db;
use rusqlite::params;

use crate::{event_in_try_compile, event_span, Finding, Pass, PassConfig, Severity};

pub struct ExecuteProcessUnchecked;

/// Per-site verdict, aggregated over every dynamic invocation of the same
/// source line (a helper function's `execute_process` runs once per call;
/// one checked invocation means the site is checked).
struct Site {
    first_event: i64,
    checked: bool,
    result_vars: Vec<String>,
    error_quiet: bool,
    program: Option<String>,
}

impl Pass for ExecuteProcessUnchecked {
    fn id(&self) -> &'static str {
        "execute-process-unchecked"
    }
    fn description(&self) -> &'static str {
        "execute_process() calls whose failure would be silently ignored"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut stmt = db.conn.prepare(
            "SELECT e.id, e.file_id, e.line, e.args_json FROM events e
             JOIN files f ON f.id = e.file_id
             WHERE e.cmd_lower = 'execute_process' AND f.in_source = 1
             ORDER BY e.id",
        )?;
        let rows: Vec<(i64, i64, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;

        // A read resolving to *this event's* write of the result variable,
        // at a later event: the exit code was actually looked at.
        // (`w.name` first so the ix_writes_name index narrows the scan.)
        let mut read_after = db.conn.prepare(
            "SELECT count(*) FROM var_writes w
             JOIN var_reads r ON r.resolved_write_id = w.id
             WHERE w.name = ?2 AND w.event_id = ?1 AND r.event_id > ?1",
        )?;

        // Aggregate per source site, in first-appearance order.
        let mut order: Vec<(i64, i64)> = Vec::new();
        let mut sites: std::collections::HashMap<(i64, i64), Site> =
            std::collections::HashMap::new();
        for (event_id, file_id, line, args_json) in rows {
            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            if event_in_try_compile(db, event_id) {
                continue;
            }
            let fatal = args.iter().any(|a| a == "COMMAND_ERROR_IS_FATAL");
            let result_vars = result_variables(&args);
            let mut checked = fatal;
            if !checked {
                for var in &result_vars {
                    let reads: i64 = read_after.query_row(params![event_id, var], |r| r.get(0))?;
                    if reads > 0 {
                        checked = true;
                        break;
                    }
                }
            }
            let site = sites.entry((file_id, line)).or_insert_with(|| {
                order.push((file_id, line));
                Site {
                    first_event: event_id,
                    checked: false,
                    result_vars: Vec::new(),
                    error_quiet: false,
                    program: None,
                }
            });
            site.checked |= checked;
            site.error_quiet |= args.iter().any(|a| a == "ERROR_QUIET");
            for var in result_vars {
                if !site.result_vars.contains(&var) {
                    site.result_vars.push(var);
                }
            }
            if site.program.is_none() {
                site.program = program_name(&args);
            }
        }

        let mut findings = Vec::new();
        for key in order {
            let site = &sites[&key];
            if site.checked {
                continue;
            }
            if site.result_vars.iter().any(|v| cfg.ignored(v)) {
                continue;
            }
            let what = match &site.program {
                Some(p) => format!("`execute_process` running `{p}`"),
                None => "`execute_process`".to_string(),
            };
            let quiet = if site.error_quiet {
                " (and ERROR_QUIET suppresses its stderr, so nothing would be printed either)"
            } else {
                ""
            };
            let message = if site.result_vars.is_empty() {
                format!(
                    "{what} has no RESULT_VARIABLE and no COMMAND_ERROR_IS_FATAL — \
                     if the command fails, the failure is silently ignored{quiet}"
                )
            } else {
                let vars = site.result_vars.join("`, `");
                format!(
                    "{what} captures its exit code into `{vars}`, but the result is \
                     never checked — the variable is not read after the call in this \
                     configure{quiet}"
                )
            };
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Note,
                message,
                primary: event_span(db, site.first_event),
                related: vec![],
                fix: None,
            });
        }
        Ok(findings)
    }
}

/// Result-variable names captured by this call: the argument following
/// each `RESULT_VARIABLE` / `RESULTS_VARIABLE` keyword.
fn result_variables(args: &[String]) -> Vec<String> {
    let mut vars = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "RESULT_VARIABLE" || a == "RESULTS_VARIABLE" {
            if let Some(v) = it.next() {
                if !v.is_empty() && !vars.contains(v) {
                    vars.push(v.clone());
                }
            }
        }
    }
    vars
}

/// The executable of the first COMMAND, for a recognizable message
/// (basename only — expanded args carry absolute tool paths).
fn program_name(args: &[String]) -> Option<String> {
    let pos = args.iter().position(|a| a == "COMMAND")?;
    let prog = args.get(pos + 1)?;
    // Expanded list variables arrive `;`-joined; the program is the head.
    let head = prog.split(';').next().unwrap_or(prog);
    if head.is_empty() {
        return None;
    }
    let base = head.rsplit(['/', '\\']).next().unwrap_or(head);
    Some(base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn result_variable_extraction() {
        assert_eq!(
            result_variables(&a(&["COMMAND", "git", "RESULT_VARIABLE", "rv"])),
            vec!["rv".to_string()]
        );
        assert_eq!(
            result_variables(&a(&["RESULTS_VARIABLE", "rvs", "RESULT_VARIABLE", "rv"])),
            vec!["rvs".to_string(), "rv".to_string()]
        );
        assert!(result_variables(&a(&["COMMAND", "git", "status"])).is_empty());
        // Trailing keyword with no value, and empty expansion.
        assert!(result_variables(&a(&["RESULT_VARIABLE"])).is_empty());
        assert!(result_variables(&a(&["RESULT_VARIABLE", ""])).is_empty());
    }

    #[test]
    fn program_extraction() {
        assert_eq!(
            program_name(&a(&["COMMAND", "/usr/bin/git", "describe"])).as_deref(),
            Some("git")
        );
        // `;`-joined expanded list: the head is the program.
        assert_eq!(
            program_name(&a(&["COMMAND", "/opt/cmake;-E;true"])).as_deref(),
            Some("cmake")
        );
        assert_eq!(program_name(&a(&["OUTPUT_VARIABLE", "x"])), None);
        assert_eq!(program_name(&a(&["COMMAND", ""])), None);
    }
}
