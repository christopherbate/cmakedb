//! End-to-end coverage for the `execute-process-unchecked` lint over the
//! `fixtures/execproc` corpus: the two unchecked shapes flag with
//! distinguishable messages, the two checked shapes stay silent.
//! Requires `cmake` on PATH. Helpers mirror tests/e2e.rs.

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

#[test]
fn execute_process_unchecked_flags_only_unchecked_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("execproc", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let hits: Vec<&serde_json::Value> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "execute-process-unchecked")
        .collect();

    // Shape 1: no RESULT_VARIABLE, no COMMAND_ERROR_IS_FATAL.
    let silent = hits
        .iter()
        .find(|f| f["primary"]["line"] == 6)
        .expect("silently-ignored finding at line 6");
    let msg = silent["message"].as_str().unwrap();
    assert!(msg.contains("silently ignored"), "{msg}");
    assert!(msg.contains("no RESULT_VARIABLE"), "{msg}");
    assert_eq!(silent["severity"], "note");

    // Shape 2: RESULT_VARIABLE captured but never read afterwards.
    let unread = hits
        .iter()
        .find(|f| f["primary"]["line"] == 15)
        .expect("captured-but-never-checked finding at line 15");
    let msg = unread["message"].as_str().unwrap();
    assert!(msg.contains("`ignored_rv`"), "{msg}");
    assert!(msg.contains("never checked"), "{msg}");
    assert_eq!(unread["severity"], "note");

    // The two messages are distinguishable shapes.
    assert_ne!(
        silent["message"].as_str().unwrap(),
        unread["message"].as_str().unwrap()
    );

    // The checked shapes (RESULT_VARIABLE read via if(), and
    // COMMAND_ERROR_IS_FATAL) are never flagged — exactly two findings.
    for f in &hits {
        let m = f["message"].as_str().unwrap();
        assert!(!m.contains("checked_rv"), "{m}");
        let line = f["primary"]["line"].as_i64().unwrap();
        assert!(line != 9 && line != 18, "checked call flagged: {f}");
    }
    assert_eq!(hits.len(), 2, "{hits:?}");
}
