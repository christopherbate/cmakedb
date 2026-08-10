//! End-to-end tests for lint infrastructure: findings baseline
//! (`--baseline` / `--write-baseline`) and `[lint.severity]` overrides.
//! Requires `cmake` on PATH. Helpers are duplicated from e2e.rs on
//! purpose (kept file-local to avoid cross-test coupling).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Copy a fixture into a tempdir so recordings never pollute the repo.
fn stage(fixture: &str, tmp: &Path) -> PathBuf {
    let src_root = fixtures_root().join(fixture);
    let dst = tmp.join(fixture);
    let mut stack = vec![src_root.clone()];
    while let Some(dir) = stack.pop() {
        let rel = dir.strip_prefix(&src_root).unwrap();
        std::fs::create_dir_all(dst.join(rel)).unwrap();
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            let name = e.file_name();
            if p.is_dir() {
                if name == "build" || name == ".cmakedb" {
                    continue;
                }
                stack.push(p);
            } else {
                std::fs::copy(&p, dst.join(rel).join(name)).unwrap();
            }
        }
    }
    dst
}

fn cmakedb(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cmakedb"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run cmakedb")
}

fn record(src: &Path) -> PathBuf {
    let db = src.join(".cmakedb/trace.db");
    let out = Command::new(env!("CARGO_BIN_EXE_cmakedb"))
        .args([
            "record",
            "--source-dir",
            &src.to_string_lossy(),
            "--build-dir",
            &src.join("build").to_string_lossy(),
            "--db",
            &db.to_string_lossy(),
        ])
        .output()
        .expect("run record");
    assert!(
        out.status.success(),
        "record failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    db
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn findings(v: &serde_json::Value) -> &Vec<serde_json::Value> {
    v["findings"].as_array().expect("findings array")
}

/// The full CI ratchet flow (user guide §8): write a baseline from the
/// current findings, gate future runs on findings absent from it, and
/// verify a newly introduced finding is the only thing reported and the
/// only thing fail-on sees.
#[test]
fn baseline_ratchet_reports_only_new_findings() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("multiconfig", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy();

    // The fixture has pre-existing findings (dead options at minimum).
    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let n_before = findings(&v).len();
    assert!(n_before >= 2, "expected pre-existing findings: {v}");

    // Write the baseline; the file is versioned and sorted.
    let base = tmp.path().join("baseline.json");
    let bases = base.to_string_lossy().into_owned();
    cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "lint",
            "--write-baseline",
            &bases,
            "--format",
            "json",
        ],
    );
    let bv: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&base).unwrap()).expect("baseline json");
    assert_eq!(bv["cmakedb_baseline_version"], 1);
    let entries = bv["findings"].as_array().unwrap();
    assert_eq!(entries.len(), n_before, "{bv}");
    let keys: Vec<(String, String, i64)> = entries
        .iter()
        .map(|e| {
            (
                e["rule"].as_str().unwrap().to_string(),
                e["file"].as_str().unwrap().to_string(),
                e["line"].as_i64().unwrap(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "baseline must be sorted for stable diffs");

    // Against the baseline: zero findings, exit 0 even at the strictest
    // gate.
    let out = cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "lint",
            "--baseline",
            &bases,
            "--format",
            "json",
            "--fail-on",
            "note",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert_eq!(findings(&v).len(), 0, "{v}");
    assert_eq!(out.status.code(), Some(0), "{v}");

    // Introduce a new finding source: a user SQL pass (result-shape
    // convention: file/line/message[/severity]).
    std::fs::create_dir_all(src.join(".cmakedb/passes")).unwrap();
    std::fs::write(
        src.join(".cmakedb/passes/extra.sql"),
        "-- demo: flag the project() call\n\
         SELECT f.path AS file, e.line AS line,\n\
                'introduced after the baseline' AS message,\n\
                'warning' AS severity\n\
         FROM events e JOIN files f ON f.id = e.file_id\n\
         WHERE e.cmd_lower = 'project' AND f.in_source = 1;\n",
    )
    .unwrap();

    // Only the new finding surfaces, and fail-on gates it.
    let out = cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "lint",
            "--baseline",
            &bases,
            "--format",
            "json",
            "--fail-on",
            "warning",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let fs = findings(&v);
    assert_eq!(fs.len(), 1, "{v}");
    assert_eq!(fs[0]["rule"], "extra");
    assert_eq!(
        out.status.code(),
        Some(1),
        "fail-on must gate the new finding"
    );

    // Without the baseline the old findings are still all there.
    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert_eq!(findings(&v).len(), n_before + 1, "{v}");
}

#[test]
fn severity_overrides_apply_to_output_and_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("multiconfig", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy();

    // Baseline behavior: dead-options findings are warnings, so
    // --fail-on error passes.
    let out = cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "lint",
            "--format",
            "json",
            "--fail-on",
            "error",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let dead: Vec<_> = findings(&v)
        .iter()
        .filter(|f| f["rule"] == "dead-options")
        .collect();
    assert!(!dead.is_empty(), "{v}");
    assert!(dead.iter().all(|f| f["severity"] == "warning"), "{v}");
    assert_eq!(out.status.code(), Some(0), "{v}");

    // Override the rule to error via .cmakedb.toml: rendered severity
    // changes and the same gate now fails. Other rules are untouched.
    std::fs::write(
        src.join(".cmakedb.toml"),
        "[lint.severity]\ndead-options = \"error\"\n",
    )
    .unwrap();
    let out = cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "lint",
            "--format",
            "json",
            "--fail-on",
            "error",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    for f in findings(&v) {
        if f["rule"] == "dead-options" {
            assert_eq!(f["severity"], "error", "{f}");
        } else {
            assert_ne!(f["severity"], "error", "non-overridden rule changed: {f}");
        }
    }
    assert_eq!(out.status.code(), Some(1), "{v}");

    // An invalid severity value fails loudly, not silently.
    std::fs::write(
        src.join(".cmakedb.toml"),
        "[lint.severity]\ndead-options = \"fatal\"\n",
    )
    .unwrap();
    let out = cmakedb(&src, &["--db", &dbs, "lint"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("dead-options"), "{err}");
    assert!(err.contains("unknown severity"), "{err}");
}

/// `--no-user-passes` is the trust escape hatch for linting a repository
/// you do not control (SECURITY.md): user passes are SQL supplied by the
/// analyzed tree, so the flag must suppress them while leaving the
/// built-ins untouched. The decision is deliberately CLI-only — a
/// `.cmakedb.toml` setting could not be trusted, because that file ships
/// in the same untrusted tree.
#[test]
fn no_user_passes_skips_tree_supplied_sql() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("provenance", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy().into_owned();

    std::fs::create_dir_all(src.join(".cmakedb/passes")).unwrap();
    std::fs::write(
        src.join(".cmakedb/passes/untrusted.sql"),
        "-- stands in for a pass shipped by a repository we do not trust\n\
         SELECT f.path AS file, e.line AS line,\n\
                'user pass executed' AS message\n\
         FROM events e JOIN files f ON f.id = e.file_id\n\
         WHERE e.cmd_lower = 'project' AND f.in_source = 1;\n",
    )
    .unwrap();

    let count = |args: &[&str]| -> (usize, usize) {
        let out = cmakedb(&src, args);
        let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
        let all = findings(&v);
        let user = all
            .iter()
            .filter(|f| f["rule"].as_str() == Some("untrusted"))
            .count();
        (all.len(), user)
    };

    // Default: the tree's pass runs.
    let (total_with, user_with) = count(&["--db", &dbs, "lint", "--format", "json"]);
    assert_eq!(user_with, 1, "user pass should run by default");

    // With the flag: its findings are gone, and only its findings.
    let (total_without, user_without) =
        count(&["--db", &dbs, "lint", "--format", "json", "--no-user-passes"]);
    assert_eq!(user_without, 0, "user pass must not run under the flag");
    assert!(
        total_without > 0,
        "built-in passes must still run under the flag"
    );
    assert_eq!(
        total_without,
        total_with - user_with,
        "the flag must drop exactly the user-pass findings"
    );
}
