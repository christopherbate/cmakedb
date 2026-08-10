//! Findings baseline for the lint ratchet (user guide §8).
//!
//! A baseline is a committed snapshot of finding identities; `lint
//! --baseline` reports only findings absent from it, so CI gates new
//! findings without forcing a cleanup of the existing ones. Identity is
//! (rule, primary file, primary line) — deliberately the same key the
//! `--also` multi-recording intersection uses (`intersect_with_recordings`
//! in main.rs): stable against message rewording and severity overrides,
//! invalidated when the declaration site moves. Refresh the baseline from
//! the main branch when line drift resurrects old findings.

use anyhow::{bail, Context, Result};
use cmakedb_passes::Finding;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

const VERSION: u64 = 1;

/// The baseline identity of a finding: (rule, file, line).
pub type Key = (String, String, i64);

pub fn key(f: &Finding) -> Key {
    (f.rule.clone(), f.primary.file.clone(), f.primary.line)
}

#[derive(Serialize, Deserialize)]
struct BaselineFile {
    cmakedb_baseline_version: u64,
    findings: Vec<Entry>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    rule: String,
    file: String,
    line: i64,
}

/// Load a baseline into the set of suppressed finding keys.
pub fn load(path: &Path) -> Result<HashSet<Key>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading baseline {}", path.display()))?;
    let parsed: BaselineFile = serde_json::from_str(&text)
        .with_context(|| format!("parsing baseline {}", path.display()))?;
    if parsed.cmakedb_baseline_version != VERSION {
        bail!(
            "baseline {} has version {}; this cmakedb reads version {VERSION}",
            path.display(),
            parsed.cmakedb_baseline_version
        );
    }
    Ok(parsed
        .findings
        .into_iter()
        .map(|e| (e.rule, e.file, e.line))
        .collect())
}

/// Write the findings' identities as a baseline — sorted and deduplicated
/// so the committed file diffs stably across regenerations.
pub fn write(path: &Path, findings: &[Finding]) -> Result<()> {
    let mut entries: Vec<Entry> = findings
        .iter()
        .map(|f| Entry {
            rule: f.rule.clone(),
            file: f.primary.file.clone(),
            line: f.primary.line,
        })
        .collect();
    entries.sort();
    entries.dedup();
    let text = serde_json::to_string_pretty(&BaselineFile {
        cmakedb_baseline_version: VERSION,
        findings: entries,
    })?;
    std::fs::write(path, text + "\n")
        .with_context(|| format!("writing baseline {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cmakedb_passes::{Severity, SourceSpan};

    fn finding(rule: &str, file: &str, line: i64) -> Finding {
        Finding {
            rule: rule.into(),
            severity: Severity::Warning,
            message: "msg".into(),
            primary: SourceSpan::new(file, line),
            related: vec![],
            fix: None,
        }
    }

    #[test]
    fn roundtrip_sorted_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("baseline.json");
        // Unsorted input with a duplicate identity (different messages
        // collapse onto one key).
        let findings = vec![
            finding("z-rule", "b.cmake", 9),
            finding("a-rule", "a.cmake", 2),
            finding("z-rule", "b.cmake", 9),
        ];
        write(&p, &findings).unwrap();

        let text = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["cmakedb_baseline_version"], 1);
        assert_eq!(v["findings"].as_array().unwrap().len(), 2);
        assert_eq!(v["findings"][0]["rule"], "a-rule");

        let keys = load(&p).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&key(&findings[0])));
        assert!(keys.contains(&key(&findings[1])));
    }

    #[test]
    fn unknown_version_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("baseline.json");
        std::fs::write(&p, r#"{"cmakedb_baseline_version": 99, "findings": []}"#).unwrap();
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("version 99"), "{err}");
    }
}
