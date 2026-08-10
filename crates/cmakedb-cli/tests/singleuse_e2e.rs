//! E2e for the `single-use-variables` lint over fixtures/singleuse.
//! Helpers mirror tests/e2e.rs (staged fixture, real cmake configure);
//! the fixture's .cmakedb.toml restricts lint to the rule under test.

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
        .expect("record");
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
fn single_use_variables_flags_only_the_inlinable_pair() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("singleuse", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let findings = v["findings"].as_array().unwrap();

    // The fixture config enables only this rule; exactly the positive
    // (GREETING) is reported.
    assert_eq!(findings.len(), 1, "expected one finding: {findings:?}");
    let f = &findings[0];
    assert_eq!(f["rule"], "single-use-variables", "{f}");
    assert_eq!(f["severity"], "note", "{f}");
    let msg = f["message"].as_str().unwrap();
    assert!(msg.contains("GREETING"), "{msg}");
    assert!(msg.contains("set once and read once"), "{msg}");
    assert!(msg.contains("consider inlining"), "{msg}");
    // Primary = the set() site; related = the sole read site.
    assert_eq!(f["primary"]["line"], 6, "{f}");
    assert!(
        msg.contains("CMakeLists.txt:7"),
        "read site in message: {msg}"
    );
    assert_eq!(f["related"][0][0]["line"], 7, "{f}");

    // Each negative shape is absent: read twice, referenced in a
    // never-executed branch, function-local, set inside foreach.
    for name in ["TWICE_READ", "MAYBE_SHARED", "local_note", "loop_val"] {
        assert!(
            !findings
                .iter()
                .any(|f| f["message"].as_str().unwrap().contains(name)),
            "{name} must not be flagged: {findings:?}"
        );
    }
}
