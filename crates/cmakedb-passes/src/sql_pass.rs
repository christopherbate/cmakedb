//! SQL-file user passes (design §3.4, the pragmatic v1 plugin story).
//!
//! A pass is a single `.sql` file whose SELECT follows a small
//! result-shape convention:
//!
//! - required columns: `file` (path, absolute or source-relative),
//!   `line` (1-based), `message`;
//! - optional columns: `severity` (`note`|`warning`|`error`, default
//!   `warning`), `col`;
//! - the rule id is the file stem, e.g. `no-glob.sql` → rule `no-glob`.
//!
//! Discovery is filesystem work and therefore happens in the CLI/LSP
//! layer; this module only turns (id, SQL) into a [`Pass`], keeping the
//! purity rule intact. Statements are read-only: the connection rejects
//! writes via SQLite's query_only pragma while a SQL pass runs.
//!
//! Example (`.cmakedb/passes/no-file-glob.sql`):
//! ```sql
//! SELECT f.path AS file, e.line AS line,
//!        'file(GLOB) is not reproducible; list sources explicitly' AS message,
//!        'warning' AS severity
//! FROM events e JOIN files f ON f.id = e.file_id
//! WHERE e.cmd_lower = 'file'
//!   AND json_extract(e.args_json, '$[0]') IN ('GLOB', 'GLOB_RECURSE')
//!   AND f.in_source = 1;
//! ```

use anyhow::{bail, Context, Result};
use cmakedb_db::Db;

use crate::{Finding, Pass, PassConfig, Severity, SourceSpan};

pub struct SqlPass {
    id: String,
    description: String,
    sql: String,
}

impl SqlPass {
    pub fn new(id: impl Into<String>, sql: impl Into<String>) -> SqlPass {
        let sql = sql.into();
        // First `-- comment` line doubles as the description.
        let description = sql
            .lines()
            .find_map(|l| l.trim().strip_prefix("--").map(|s| s.trim().to_string()))
            .unwrap_or_else(|| "user SQL pass".to_string());
        SqlPass {
            id: id.into(),
            description,
            sql,
        }
    }

    /// Load every `*.sql` file in a directory, sorted by name.
    pub fn load_dir(dir: &std::path::Path) -> Result<Vec<SqlPass>> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(out);
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "sql").unwrap_or(false))
            .collect();
        paths.sort();
        for p in paths {
            let id = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let sql = std::fs::read_to_string(&p)
                .with_context(|| format!("reading SQL pass {}", p.display()))?;
            out.push(SqlPass::new(id, sql));
        }
        Ok(out)
    }
}

impl Pass for SqlPass {
    fn id(&self) -> &'static str {
        // Pass ids for built-ins are static; user pass ids are dynamic.
        // Leak is bounded by the number of configured passes per process.
        Box::leak(self.id.clone().into_boxed_str())
    }
    fn description(&self) -> &'static str {
        Box::leak(self.description.clone().into_boxed_str())
    }

    fn run(&self, db: &Db, _cfg: &PassConfig) -> Result<Vec<Finding>> {
        // Enforce read-only for the duration of the user statement.
        db.conn.pragma_update(None, "query_only", true)?;
        let result = self.run_inner(db);
        db.conn.pragma_update(None, "query_only", false)?;
        result
    }
}

impl SqlPass {
    fn run_inner(&self, db: &Db) -> Result<Vec<Finding>> {
        let mut stmt = db
            .conn
            .prepare(&self.sql)
            .with_context(|| format!("SQL pass '{}' failed to prepare", self.id))?;
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let col = |want: &str| names.iter().position(|n| n.eq_ignore_ascii_case(want));
        let (Some(c_file), Some(c_line), Some(c_msg)) = (col("file"), col("line"), col("message"))
        else {
            bail!(
                "SQL pass '{}' must SELECT columns `file`, `line`, `message` \
                 (got: {})",
                self.id,
                names.join(", ")
            );
        };
        let c_sev = col("severity");
        let c_col = col("col");

        let mut findings = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let file: String = row.get(c_file)?;
            let line: i64 = row.get(c_line).unwrap_or(1);
            let message: String = row.get(c_msg)?;
            let severity = match c_sev {
                Some(i) => row
                    .get::<_, String>(i)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(Severity::Warning),
                None => Severity::Warning,
            };
            let mut primary = SourceSpan::new(db.display_path(&file), line);
            if let Some(i) = c_col {
                primary.col = row.get(i).ok();
            }
            findings.push(Finding {
                rule: self.id.clone(),
                severity,
                message,
                primary,
                related: vec![],
                fix: None,
            });
            if findings.len() > 10_000 {
                bail!("SQL pass '{}' produced more than 10000 findings", self.id);
            }
        }
        Ok(findings)
    }
}
