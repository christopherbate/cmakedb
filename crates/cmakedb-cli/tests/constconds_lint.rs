//! End-to-end tests for the `constant-conditions` lint over the
//! `fixtures/constconds` corpus (helpers mirrored from `e2e.rs`).
//! Requires `cmake` on PATH.

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

fn lint_findings(dir: &Path, db: &Path) -> Vec<serde_json::Value> {
    let out = cmakedb(
        dir,
        &["--db", &db.to_string_lossy(), "lint", "--format", "json"],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "constant-conditions")
        .cloned()
        .collect()
}

#[test]
fn constant_conditions_flags_invariant_and_spares_varying() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("constconds", tmp.path());
    let db = record(&src);
    let hits = lint_findings(&src, &db);

    // Exactly the two invariant conditions: the loop-invariant flag and
    // the call-invariant global.
    assert_eq!(hits.len(), 2, "{hits:?}");

    let loop_inv = hits
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("INVARIANT_FLAG"))
        .expect("loop-invariant finding");
    let msg = loop_inv["message"].as_str().unwrap();
    assert!(msg.contains("evaluated 4 times"), "{msg}");
    assert!(msg.contains("INVARIANT_FLAG = \"ON\""), "{msg}");
    assert_eq!(loop_inv["severity"], "note");

    let call_inv = hits
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("GLOBAL_MODE"))
        .expect("call-invariant finding");
    let msg = call_inv["message"].as_str().unwrap();
    assert!(msg.contains("evaluated 3 times"), "{msg}");
    assert!(msg.contains("GLOBAL_MODE = \"strict\""), "{msg}");

    // The varying, literal, dynamic-dereference, and always-undefined
    // conditions must not be flagged: the loop variable (expanded or
    // bare), the per-call argument, if(TRUE), if(NOT ${flag_name}), and
    // the never-defined-inside-the-loop variable.
    for f in &hits {
        let m = f["message"].as_str().unwrap();
        assert!(!m.contains("item"), "loop variable flagged: {m}");
        assert!(!m.contains("label"), "per-call argument flagged: {m}");
        assert!(!m.contains("TRUE"), "literal condition flagged: {m}");
        assert!(!m.contains("flag_name"), "dynamic deref flagged: {m}");
        assert!(!m.contains("done_marker"), "dynamic deref flagged: {m}");
        assert!(!m.contains("LATER_DEFINED"), "all-undefined flagged: {m}");
    }
}

#[test]
fn constant_conditions_survives_also_intersection() {
    // Cross-recording intersection by (rule, file, line): recording the
    // same fixture twice keeps the findings (invariant in both), raising
    // confidence rather than filtering them out.
    let tmp = tempfile::tempdir().unwrap();
    let a = stage("constconds", tmp.path());
    let db_a = record(&a);
    let b = stage("constconds", tmp.path().join("b").as_path());
    let db_b = record(&b);

    let out = cmakedb(
        &a,
        &[
            "--db",
            &db_a.to_string_lossy(),
            "lint",
            "--also",
            &db_b.to_string_lossy(),
            "--format",
            "json",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let hits: Vec<&serde_json::Value> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "constant-conditions")
        .collect();
    assert_eq!(hits.len(), 2, "{hits:?}");
    for f in &hits {
        let m = f["message"].as_str().unwrap();
        assert!(m.contains("present in all 2 recordings"), "{m}");
    }
}
