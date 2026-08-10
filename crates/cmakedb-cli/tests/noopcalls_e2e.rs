//! E2e for the `noop-calls` lint over fixtures/noopcalls.
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
fn noop_calls_flags_empty_expansions_and_literal_empties() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("noopcalls", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let findings = v["findings"].as_array().unwrap();
    let msgs: Vec<&str> = findings
        .iter()
        .map(|f| f["message"].as_str().unwrap())
        .collect();

    for f in findings {
        assert_eq!(f["rule"], "noop-calls", "{f}");
        assert_eq!(f["severity"], "note", "{f}");
    }

    // Positive 1: empty-expanding var on target_link_libraries, with the
    // never-set enrichment.
    let tll = msgs
        .iter()
        .find(|m| m.contains("target_link_libraries"))
        .expect("tll finding");
    assert!(tll.contains("was a no-op in all 1 evaluation(s)"), "{tll}");
    assert!(tll.contains("${EXTRA_LIBS} expanded to nothing"), "{tll}");
    assert!(
        tll.contains("EXTRA_LIBS is never set — see undefined-reads"),
        "{tll}"
    );

    // Positive 2: literally-empty add_definitions(), distinct phrasing.
    let adddef = msgs
        .iter()
        .find(|m| m.contains("add_definitions"))
        .expect("add_definitions finding");
    assert!(adddef.contains("has no arguments — remove it"), "{adddef}");

    // Positive 3: list(APPEND FLAGS) with no items.
    let lst = msgs
        .iter()
        .find(|m| m.contains("list(APPEND FLAGS)"))
        .expect("list finding");
    assert!(lst.contains("adds no items — remove it"), "{lst}");

    // Positive 4: populated only in an untaken branch -> B1 enrichment.
    let cond = msgs
        .iter()
        .find(|m| m.contains("${COND_DEFS}"))
        .expect("COND_DEFS finding");
    assert!(cond.contains("was a no-op"), "{cond}");
    assert!(
        cond.contains("COND_DEFS is populated only in code that never ran (B1)"),
        "{cond}"
    );

    // Negatives: the populated list and the quoted empty-string argument
    // must not be flagged.
    assert!(
        !msgs.iter().any(|m| m.contains("APP_DEFS")),
        "populated var must not be flagged: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("\"\"")),
        "quoted empty-string argument must not be flagged: {msgs:?}"
    );

    // Exactly the four positives.
    assert_eq!(findings.len(), 4, "{msgs:?}");
}
