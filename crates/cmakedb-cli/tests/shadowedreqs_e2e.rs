//! End-to-end test for the `shadowed-requirements` lint pass over the
//! `shadowedreqs` fixture: a target_include_directories duplicating a
//! same-file include_directories and a define duplicated through a
//! linked PUBLIC dependency are flagged; a genex-valued duplicate, a
//! subdirectory's standalone-build re-declaration, and an unrelated
//! include dir are not.

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
fn shadowed_requirements_fixture_lints() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("shadowedreqs", tmp.path());
    // Restrict the run to this rule via config, exercising the enable knob.
    std::fs::write(
        src.join(".cmakedb.toml"),
        "[lint]\nenable = [\"shadowed-requirements\"]\n",
    )
    .unwrap();
    let db = record(&src);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let text = stdout(&out);
    let v: serde_json::Value = serde_json::from_str(&text).expect("valid findings json");
    let findings = v["findings"].as_array().expect("findings array");
    assert!(
        findings
            .iter()
            .all(|f| f["rule"] == "shadowed-requirements"),
        "rule restriction leaked other rules:\n{text}"
    );
    assert_eq!(findings.len(), 2, "expected exactly 2 findings:\n{text}");
    assert!(findings.iter().all(|f| f["severity"] == "note"), "{text}");

    // Positive 1: the direct target_include_directories duplicating the
    // same-file include_directories. Primary is the MOST SPECIFIC
    // (direct) declaration; the directory command is the evidence span.
    let inc = findings
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("include dir"))
        .expect("include finding");
    let msg = inc["message"].as_str().unwrap();
    assert!(msg.starts_with("app:"), "{text}");
    assert!(
        msg.contains("directory-scope include_directories"),
        "{text}"
    );
    assert!(msg.contains("redundant in this configuration"), "{text}");
    // The include-order caveat is surfaced in the message itself.
    assert!(msg.contains("reorder includes"), "{text}");
    // Primary points at the target_include_directories call (line 20),
    // related at the include_directories call (line 7).
    assert_eq!(inc["primary"]["line"], 20, "{text}");
    let related = inc["related"].as_array().expect("related array");
    assert_eq!(related.len(), 1, "{text}");
    assert_eq!(related[0][0]["line"], 7, "{text}");
    assert!(
        related[0][1]
            .as_str()
            .unwrap()
            .contains("also supplied by directory-scope include_directories"),
        "{text}"
    );

    // Positive 2: the define reaching app directly AND via dep's PUBLIC
    // interface (one-hop transitive route).
    let def = findings
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("SHARED_DEF=1"))
        .expect("define finding");
    let msg = def["message"].as_str().unwrap();
    assert!(msg.starts_with("app:"), "{text}");
    assert!(
        msg.contains("public interface of linked dependency 'dep'"),
        "{text}"
    );
    // The include-order caveat does not apply to definitions.
    assert!(!msg.contains("reorder includes"), "{text}");
    let related = def["related"].as_array().expect("related array");
    assert_eq!(related.len(), 1, "{text}");
    assert!(
        related[0][1]
            .as_str()
            .unwrap()
            .contains("linked dependency 'dep'"),
        "{text}"
    );

    // Negative: the genex-valued duplicate is config-dependent — never
    // flagged.
    assert!(!text.contains("GXDEF"), "genex duplicate flagged:\n{text}");
    // Negative: subtool's re-declaration of the parent directory-scope
    // include is standalone-build intent (different directories).
    assert!(
        !text.contains("subtool"),
        "standalone-build heuristic failed:\n{text}"
    );
    // Negative: the unrelated include dir has only one route.
    assert!(!text.contains("/other"), "unrelated dir flagged:\n{text}");
}
