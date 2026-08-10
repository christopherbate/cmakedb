//! Dead-code passes (design §4.5): unread options, uncalled functions,
//! never-included modules, write-only variables.

use anyhow::Result;
use cmakedb_db::Db;

use crate::{event_in_source, event_span, Finding, Pass, PassConfig, Severity, SourceSpan};

pub struct DeadOptions;

impl Pass for DeadOptions {
    fn id(&self) -> &'static str {
        "dead-options"
    }
    fn description(&self) -> &'static str {
        "option()/cache declarations whose variable is never read"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();
        // Guard corpus: preset files may reference options for other
        // configurations (§4.5).
        let preset_text = format!(
            "{}\n{}",
            db.get_meta("presets_json")?.unwrap_or_default(),
            db.get_meta("user_presets_json")?.unwrap_or_default()
        );

        // option() events plus user-facing set(... CACHE <type> ...) writes.
        let mut stmt = db.conn.prepare(
            "SELECT e.id, e.cmd_lower, e.args_json FROM events e
             WHERE e.cmd_lower IN ('option', 'set')
             ORDER BY e.id",
        )?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut seen = std::collections::HashSet::new();
        for (eid, cmd, args_json) in rows {
            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            let name = match args.first() {
                Some(n) => n.clone(),
                None => continue,
            };
            let is_declaration = match cmd.as_str() {
                "option" => true,
                _ => {
                    // set(NAME v CACHE TYPE doc): user-facing types only.
                    match args.iter().position(|a| a == "CACHE") {
                        Some(pos) => matches!(
                            args.get(pos + 1).map(|s| s.as_str()),
                            Some("BOOL" | "STRING" | "PATH" | "FILEPATH")
                        ),
                        None => false,
                    }
                }
            };
            if !is_declaration || cfg.ignored(&name) || !seen.insert(name.clone()) {
                continue;
            }
            if !event_in_source(db, eid) {
                continue;
            }

            let reads: i64 = db.conn.query_row(
                "SELECT count(*) FROM var_reads WHERE name = ?1",
                [&name],
                |r| r.get(0),
            )?;
            if reads > 0 {
                continue;
            }
            if preset_text.contains(&name) {
                continue; // referenced by a preset; likely read in another config
            }

            // Static references in code that never executed (e.g. only
            // inside an uncalled function) — reported as a qualifier, since
            // trace reads can't exist for unexecuted branches.
            let mut related = Vec::new();
            let mut qualifier = "(set, never read)".to_string();
            let mut unexec = db.conn.prepare(
                "SELECT f.path, n.line FROM ast_var_refs vr
                 JOIN ast_nodes n ON n.id = vr.node_id
                 JOIN files f ON f.id = n.file_id
                 WHERE vr.name = ?1
                   AND NOT EXISTS (SELECT 1 FROM events e WHERE e.node_id = vr.node_id)
                 LIMIT 5",
            )?;
            let refs: Vec<(String, i64)> = unexec
                .query_map([&name], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            if !refs.is_empty() {
                qualifier = "(read only by code that never executed)".into();
                for (path, line) in refs {
                    related.push((
                        SourceSpan::new(db.display_path(&path), line),
                        "referenced here, but this code never ran".into(),
                    ));
                }
            }

            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Warning,
                message: format!("UNREAD {name} {qualifier} — not read in this configuration"),
                primary: event_span(db, eid),
                related,
                fix: None,
            });
        }
        Ok(findings)
    }
}

pub struct DeadFunctions;

impl Pass for DeadFunctions {
    fn id(&self) -> &'static str {
        "dead-functions"
    }
    fn description(&self) -> &'static str {
        "functions/macros defined but never invoked (in this configuration)"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();
        let mut stmt = db.conn.prepare(
            "SELECT d.event_id, d.name, d.name_lower, d.kind, f.path, d.line
             FROM func_defs d JOIN files f ON f.id = d.file_id
             WHERE f.in_source = 1
               AND NOT EXISTS (
                 SELECT 1 FROM events e
                 WHERE e.cmd_lower = d.name_lower AND e.id != d.event_id
                   AND e.cmd_lower NOT IN ('function', 'macro'))
               AND NOT EXISTS (
                 SELECT 1 FROM events e
                 WHERE e.cmd_lower = 'cmake_language'
                   AND json_extract(e.args_json, '$[0]') IN ('CALL', 'DEFER')
                   AND (lower(json_extract(e.args_json, '$[1]')) = d.name_lower
                        OR lower(coalesce(json_extract(e.args_json, '$[2]'), '')) = d.name_lower
                        OR lower(coalesce(json_extract(e.args_json, '$[3]'), '')) = d.name_lower))
             ORDER BY d.id",
        )?;
        let rows: Vec<(i64, String, String, String, String, i64)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut seen = std::collections::HashSet::new();
        for (_eid, name, name_lower, kind, path, line) in rows {
            if cfg.ignored(&name) || !seen.insert((name_lower, path.clone(), line)) {
                continue;
            }
            // A "dead" function referenced textually by never-executed code
            // (e.g. called only from another dead function) still counts as
            // dead, but note it.
            let callers: i64 = db.conn.query_row(
                "SELECT count(*) FROM commands c
                 WHERE c.name_lower = lower(?1)
                   AND NOT EXISTS (SELECT 1 FROM events e WHERE e.node_id = c.node_id)",
                [&name],
                |r| r.get(0),
            )?;
            let note = if callers > 0 {
                " (only called from code that never executed)"
            } else {
                ""
            };
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Warning,
                message: format!(
                    "{kind} '{name}' is defined but never called in this configuration{note}"
                ),
                primary: SourceSpan::new(db.display_path(&path), line),
                related: vec![],
                fix: None,
            });
        }
        Ok(findings)
    }
}

pub struct DeadModules;

impl Pass for DeadModules {
    fn id(&self) -> &'static str {
        "dead-modules"
    }
    fn description(&self) -> &'static str {
        "CMake files under the source tree that never executed"
    }

    fn run(&self, db: &Db, _cfg: &PassConfig) -> Result<Vec<Finding>> {
        let preset_text = format!(
            "{}\n{}",
            db.get_meta("presets_json")?.unwrap_or_default(),
            db.get_meta("user_presets_json")?.unwrap_or_default()
        );
        let mut findings = Vec::new();
        let mut stmt = db.conn.prepare(
            "SELECT f.path FROM files f
             WHERE f.in_source = 1
               AND NOT EXISTS (SELECT 1 FROM events e WHERE e.file_id = f.id)
             ORDER BY f.path",
        )?;
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for path in rows {
            let basename = std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // Guard (§4.5): a file referenced by any executed command —
            // install()ed for downstream consumers, configure_file() input,
            // listed in a variable — is consumed even though it never
            // executed. Presets likewise.
            let referenced: i64 = db.conn.query_row(
                "SELECT count(*) FROM events
                 WHERE args_json LIKE '%' || ?1 || '%' LIMIT 1",
                [&basename],
                |r| r.get(0),
            )?;
            if referenced > 0 || preset_text.contains(&basename) {
                continue;
            }
            let display = db.display_path(&path);
            let what = if basename == "CMakeLists.txt" {
                "directory is never added (add_subdirectory) in this configuration"
            } else {
                "module is never included in this configuration"
            };
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Warning,
                message: format!("{display}: {what}"),
                primary: SourceSpan::new(display.clone(), 1),
                related: vec![],
                fix: None,
            });
        }
        Ok(findings)
    }
}

pub struct DeadVariables;

impl Pass for DeadVariables {
    fn id(&self) -> &'static str {
        "dead-variables"
    }
    fn description(&self) -> &'static str {
        "variables set but never read (scoped, noise-filtered)"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();
        // Names with at least one project-code write and zero reads anywhere.
        // Scope liveness (§4.2): PARENT_SCOPE writes are recorded against the
        // parent scope, so "no read anywhere for this name" subsumes the
        // parent-liveness rule at name granularity.
        let mut stmt = db.conn.prepare(
            "SELECT w.name, min(w.event_id)
             FROM var_writes w
             JOIN events e ON e.id = w.event_id
             JOIN files f ON f.id = e.file_id
             WHERE w.write_kind IN ('set', 'parent_scope')
               AND f.in_source = 1
               AND e.cmd_lower IN ('set')
               AND NOT EXISTS (SELECT 1 FROM var_reads r WHERE r.name = w.name)
             GROUP BY w.name
             ORDER BY min(w.event_id)",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (name, first_event) in rows {
            if cfg.ignored(&name) {
                continue;
            }
            // Variables referenced statically in unexecuted code are demoted
            // to notes — they may be read in another configuration.
            let static_refs: i64 = db.conn.query_row(
                "SELECT count(*) FROM ast_var_refs WHERE name = ?1",
                [&name],
                |r| r.get(0),
            )?;
            let (severity, extra) = if static_refs > 0 {
                (
                    Severity::Note,
                    " (referenced only in code that never executed)",
                )
            } else {
                (Severity::Warning, "")
            };
            findings.push(Finding {
                rule: self.id().into(),
                severity,
                message: format!("variable '{name}' is set but never read{extra}"),
                primary: event_span(db, first_event),
                related: vec![],
                fix: None,
            });
        }
        Ok(findings)
    }
}
