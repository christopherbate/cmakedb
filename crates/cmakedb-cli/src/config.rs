//! `.cmakedb.toml` configuration (design §2.5).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// Directory the config was loaded from (for resolving relative paths).
    #[serde(skip)]
    pub root: std::path::PathBuf,
    #[serde(default)]
    pub record: RecordConfig,
    #[serde(default)]
    pub lint: LintConfig,
    /// User pass plugins (design §3.4).
    #[serde(default)]
    pub passes: PassesSection,
    #[serde(default)]
    #[allow(dead_code)]
    pub modernize: ModernizeConfig,
    /// Wrapper-function signatures refining argument parsing (§3.1).
    /// Accepted and stored in v1; consumed by the M4 codemod milestone.
    #[serde(default)]
    #[allow(dead_code)]
    pub custom_commands: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PassesSection {
    /// Directory of `*.sql` user passes; default `.cmakedb/passes`.
    pub sql_dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct RecordConfig {
    pub preset: Option<String>,
    /// Scope snapshots (§3.2 channel 2): dump directory-scope variable
    /// tables and cross-validate the replay at ingestion (§6.2).
    #[serde(default)]
    pub capture_scopes: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LintConfig {
    #[serde(default)]
    pub enable: Vec<String>,
    #[serde(default)]
    pub disable: Vec<String>,
    /// CI exit-code threshold: error | warning | note.
    #[serde(default = "default_fail_on")]
    pub fail_on: String,
    /// Per-rule severity overrides: rule id → note|warning|error, e.g.
    /// `[lint.severity] set-cache-force = "error"`. Values are validated
    /// when applied (CLI), so an unknown severity fails loudly at lint
    /// time. Declared as a named field so serde claims the `severity` key
    /// before the flattened per-pass map below can swallow it as a
    /// (shape-incompatible) pass table.
    #[serde(default)]
    pub severity: HashMap<String, String>,
    /// Per-pass tables, e.g. `[lint.dead-variables] ignore-patterns = [...]`.
    #[serde(flatten)]
    pub passes: HashMap<String, PassTable>,
}

impl Default for LintConfig {
    fn default() -> Self {
        LintConfig {
            enable: vec![],
            disable: vec![],
            fail_on: default_fail_on(),
            severity: HashMap::new(),
            passes: HashMap::new(),
        }
    }
}

fn default_fail_on() -> String {
    "warning".into()
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct PassTable {
    #[serde(default)]
    pub ignore_patterns: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ModernizeConfig {
    #[serde(default)]
    #[allow(dead_code)]
    pub passes: Vec<String>,
}

impl Config {
    /// Load from `.cmakedb.toml` in `dir` or any ancestor; defaults if absent.
    pub fn load(dir: &Path) -> Result<Config> {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            let p = d.join(".cmakedb.toml");
            if p.is_file() {
                let text = std::fs::read_to_string(&p)?;
                let mut cfg: Config =
                    toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
                cfg.root = d.to_path_buf();
                return Ok(cfg);
            }
            cur = d.parent();
        }
        Ok(Config {
            root: dir.to_path_buf(),
            ..Config::default()
        })
    }

    /// Discover user SQL passes from `passes.sql-dir` (default
    /// `.cmakedb/passes`), relative to the config root.
    pub fn sql_passes(&self) -> Result<Vec<cmakedb_passes::sql_pass::SqlPass>> {
        let dir = self
            .passes
            .sql_dir
            .clone()
            .unwrap_or_else(|| ".cmakedb/passes".into());
        let dir = if Path::new(&dir).is_absolute() {
            std::path::PathBuf::from(dir)
        } else {
            self.root.join(dir)
        };
        cmakedb_passes::sql_pass::SqlPass::load_dir(&dir)
    }

    pub fn pass_enabled(&self, id: &str) -> bool {
        if self.lint.disable.iter().any(|d| d == id) {
            return false;
        }
        self.lint.enable.is_empty() || self.lint.enable.iter().any(|e| e == id)
    }

    /// Build the PassConfig for a pass id: per-pass ignore-patterns or the
    /// built-in defaults for the variable-name passes.
    pub fn pass_config(&self, id: &str) -> cmakedb_passes::PassConfig {
        let patterns: Vec<String> = match self.lint.passes.get(id) {
            Some(t) if !t.ignore_patterns.is_empty() => t.ignore_patterns.clone(),
            _ => cmakedb_passes::default_ignore_patterns()
                .into_iter()
                .map(String::from)
                .collect(),
        };
        let ignore = patterns
            .iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect();
        cmakedb_passes::PassConfig {
            ignore,
            options: serde_json::Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_table_is_claimed_before_the_pass_table_flatten() {
        // `[lint.severity]` must land in the named `severity` field, not
        // be swallowed by the flattened per-pass map (where PassTable's
        // lenient shape would silently drop the overrides).
        let cfg: Config = toml::from_str(
            r#"
            [lint]
            fail-on = "error"

            [lint.severity]
            set-cache-force = "error"
            overshare = "note"

            [lint.dead-variables]
            ignore-patterns = ["^_"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.lint.fail_on, "error");
        assert_eq!(cfg.lint.severity.len(), 2);
        assert_eq!(cfg.lint.severity["set-cache-force"], "error");
        assert_eq!(cfg.lint.severity["overshare"], "note");
        assert!(cfg.lint.passes.contains_key("dead-variables"));
        assert!(!cfg.lint.passes.contains_key("severity"));
        assert_eq!(cfg.lint.passes["dead-variables"].ignore_patterns, ["^_"]);
    }

    #[test]
    fn severity_table_defaults_empty() {
        let cfg: Config = toml::from_str("[lint]\nenable = [\"clobbers\"]\n").unwrap();
        assert!(cfg.lint.severity.is_empty());
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.lint.severity.is_empty());
    }
}
