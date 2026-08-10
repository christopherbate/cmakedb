//! E2e for the `alias-variables` lint over fixtures/aliasvars.
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
fn alias_variables_flags_alias_and_roundtrip_only() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("aliasvars", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let findings = v["findings"].as_array().unwrap();

    // The fixture config enables only this rule; exactly the two
    // positives (pure alias + PARENT_SCOPE round-trip) are reported.
    assert_eq!(findings.len(), 2, "expected two findings: {findings:?}");
    for f in findings {
        assert_eq!(f["rule"], "alias-variables", "{f}");
        assert_eq!(f["severity"], "note", "{f}");
    }

    // Shape 1: ALIAS_LABEL is a pure alias of BASE_LABEL.
    let alias = findings
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("ALIAS_LABEL"))
        .expect("pure-alias finding");
    let msg = alias["message"].as_str().unwrap();
    assert!(msg.contains("pure alias of BASE_LABEL"), "{msg}");
    assert!(msg.contains("use BASE_LABEL directly"), "{msg}");
    // Primary = the alias set() site; related = the read site.
    assert_eq!(alias["primary"]["line"], 8, "{alias}");
    assert_eq!(alias["related"][0][0]["line"], 9, "{alias}");

    // Shape 2: the RELEASE_TAG round-trip inside refresh_tag().
    let rt = findings
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("RELEASE_TAG"))
        .expect("round-trip finding");
    let msg = rt["message"].as_str().unwrap();
    assert!(
        msg.contains("set(RELEASE_TAG ${RELEASE_TAG} PARENT_SCOPE)"),
        "{msg}"
    );
    assert!(msg.contains("unchanged inherited value"), "{msg}");
    assert!(msg.contains("remove it"), "{msg}");
    assert_eq!(rt["primary"]["line"], 36, "{rt}");
    // Evidence points at the inherited definition.
    assert_eq!(rt["related"][0][0]["line"], 34, "{rt}");

    // Each negative shape is absent: source rewritten between alias and
    // read, composite value, reference in a never-executed branch, and a
    // real (locally computed) PARENT_SCOPE export.
    for name in ["STALE_COPY", "COMPOSITE_VAL", "MIRROR_LABEL", "BUILD_STAMP"] {
        assert!(
            !findings
                .iter()
                .any(|f| f["message"].as_str().unwrap().contains(name)),
            "{name} must not be flagged: {findings:?}"
        );
    }
}
