//! End-to-end test for the `policy-hygiene` lint pass. Requires `cmake`
//! on PATH. Helpers are duplicated from e2e.rs on purpose (kept
//! file-local to avoid cross-test coupling).

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
fn policy_hygiene_flags_old_pins_only() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("policyhygiene", tmp.path());
    let db = record(&src);

    // Backward compatibility: databases recorded before schema v3 have no
    // configure_diagnostics table. Drop it if this recording has one so
    // the pass is exercised against the pre-v3 shape and must not crash.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP TABLE IF EXISTS configure_diagnostics")
            .unwrap();
    }

    let dbs = db.to_string_lossy();
    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let text = stdout(&out);
    let v: serde_json::Value = serde_json::from_str(&text).expect("lint json");
    let findings: Vec<&serde_json::Value> = v["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .filter(|f| f["rule"] == "policy-hygiene")
        .collect();

    // Exactly the OLD pin is flagged, at its declaration site, as a
    // warning, with the curated-table context for a known policy.
    assert_eq!(findings.len(), 1, "findings: {text}");
    let f = findings[0];
    assert_eq!(f["severity"], "warning", "{f}");
    let msg = f["message"].as_str().unwrap();
    assert!(msg.contains("CMP0135"), "{msg}");
    assert!(msg.contains("3.24"), "known-policy context missing: {msg}");
    assert!(
        f["primary"]["file"]
            .as_str()
            .unwrap()
            .ends_with("CMakeLists.txt"),
        "{f}"
    );
    assert_eq!(f["primary"]["line"], 6, "{f}");
    // The NEW pin must not appear anywhere in this rule's findings.
    assert!(!text.contains("CMP0077"), "NEW pin flagged: {text}");
}
