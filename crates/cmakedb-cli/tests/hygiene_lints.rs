//! End-to-end tests for the hygiene lints (`set-cache-force`,
//! `fetchcontent-pinning`) over the `hygiene` fixture. The fixture only
//! *declares* FetchContent dependencies (never makes them available), so
//! the recorded configure stays offline and fast.

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
fn hygiene_fixture_lints() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("hygiene", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let text = stdout(&out);
    let v: serde_json::Value = serde_json::from_str(&text).expect("valid findings json");
    let findings = v["findings"].as_array().expect("findings array");
    let by_rule = |rule: &str| -> Vec<&serde_json::Value> {
        findings.iter().filter(|f| f["rule"] == rule).collect()
    };

    // set-cache-force: the STRING FORCE stomp is a warning, the guarded
    // FORCE is demoted to note, the INTERNAL FORCE never flags.
    let force = by_rule("set-cache-force");
    assert_eq!(force.len(), 2, "set-cache-force findings:\n{text}");
    let mode = force
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("HYGIENE_MODE"))
        .expect("HYGIENE_MODE finding");
    assert_eq!(mode["severity"], "warning", "{text}");
    assert!(
        mode["message"].as_str().unwrap().contains("CACHE STRING"),
        "{text}"
    );
    let guarded = force
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("HYGIENE_GUARDED"))
        .expect("HYGIENE_GUARDED finding");
    assert_eq!(guarded["severity"], "note", "guard not demoted:\n{text}");
    assert!(
        !force
            .iter()
            .any(|f| f["message"].as_str().unwrap().contains("HYGIENE_STAMP")),
        "INTERNAL FORCE must be exempt:\n{text}"
    );

    // fetchcontent-pinning: branch tag + hashless URL flag (as notes);
    // full-SHA and URL_HASH declarations are pinned.
    let pinning = by_rule("fetchcontent-pinning");
    assert_eq!(pinning.len(), 2, "fetchcontent-pinning findings:\n{text}");
    assert!(pinning.iter().all(|f| f["severity"] == "note"), "{text}");
    let tracking = pinning
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("tracking_dep"))
        .expect("tracking_dep finding");
    let msg = tracking["message"].as_str().unwrap();
    assert!(
        msg.contains("GIT_TAG `main` is not a pinned commit hash"),
        "{text}"
    );
    let tarball = pinning
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("tarball_dep"))
        .expect("tarball_dep finding");
    assert!(
        tarball["message"].as_str().unwrap().contains("URL_HASH"),
        "{text}"
    );
    for pinned in ["pinned_dep", "hashed_dep"] {
        assert!(
            !pinning
                .iter()
                .any(|f| f["message"].as_str().unwrap().contains(pinned)),
            "pinned declaration flagged:\n{text}"
        );
    }
}
