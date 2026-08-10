//! End-to-end tests for the link-graph lints (`duplicate-links`,
//! `cyclic-links`) over the `fixtures/linkgraph` corpus. Requires `cmake`
//! on PATH. Helpers mirror tests/e2e.rs (kept separate on purpose).

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
fn linkgraph_duplicate_and_cyclic_lints() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("linkgraph", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let findings = v["findings"].as_array().unwrap();

    // duplicate-links: exactly two findings.
    let dups: Vec<&serde_json::Value> = findings
        .iter()
        .filter(|f| f["rule"] == "duplicate-links")
        .collect();
    assert_eq!(dups.len(), 2, "{dups:#?}");

    // Conflicting visibilities (core links util PUBLIC + PRIVATE): warning,
    // with the other call as related evidence.
    let conflict = dups
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("`util`"))
        .expect("conflicting-visibility finding");
    assert_eq!(conflict["severity"], "warning", "{conflict}");
    let msg = conflict["message"].as_str().unwrap();
    assert!(msg.contains("conflicting visibilities"), "{msg}");
    assert!(msg.contains("PUBLIC") && msg.contains("PRIVATE"), "{msg}");
    assert_eq!(
        conflict["related"].as_array().unwrap().len(),
        1,
        "{conflict}"
    );
    assert!(
        conflict["primary"]["file"]
            .as_str()
            .unwrap()
            .ends_with("CMakeLists.txt"),
        "{conflict}"
    );

    // Same-visibility duplicate (app links extra twice in one call): note.
    let redundant = dups
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("`extra`"))
        .expect("same-visibility finding");
    assert_eq!(redundant["severity"], "note", "{redundant}");
    assert!(
        redundant["message"].as_str().unwrap().contains("same call"),
        "{redundant}"
    );

    // The debug/optimized per-config pair must be folded — no finding for
    // `app` linking `core`.
    assert!(
        !dups
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("`app`")
                && f["message"].as_str().unwrap().contains("`core`")),
        "{dups:#?}"
    );

    // cyclic-links: exactly one cycle, both hops located.
    let cycles: Vec<&serde_json::Value> = findings
        .iter()
        .filter(|f| f["rule"] == "cyclic-links")
        .collect();
    assert_eq!(cycles.len(), 1, "{cycles:#?}");
    let cycle = cycles[0];
    assert_eq!(cycle["severity"], "note", "{cycle}");
    let msg = cycle["message"].as_str().unwrap();
    assert!(msg.contains("core -> util -> core"), "{msg}");
    assert!(msg.contains("static libraries"), "{msg}");
    assert!(msg.contains("architecture smell"), "{msg}");
    let related = cycle["related"].as_array().unwrap();
    assert_eq!(related.len(), 1, "{cycle}");
    assert!(
        related[0][1]
            .as_str()
            .unwrap()
            .contains("`util` links `core`"),
        "{cycle}"
    );
}
