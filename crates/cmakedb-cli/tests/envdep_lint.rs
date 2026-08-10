//! End-to-end tests for the `env-dependence` lint over fixtures/envdep.
//! Helpers mirror tests/e2e.rs (staged fixture, real cmake configure).

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

fn record(src: &Path, capture_scopes: bool) -> PathBuf {
    let db = src.join(".cmakedb/trace.db");
    let mut args = vec![
        "record".to_string(),
        "--source-dir".into(),
        src.to_string_lossy().into_owned(),
        "--build-dir".into(),
        src.join("build").to_string_lossy().into_owned(),
        "--db".into(),
        db.to_string_lossy().into_owned(),
    ];
    if capture_scopes {
        args.push("--capture-scopes".into());
    }
    let out = Command::new(env!("CARGO_BIN_EXE_cmakedb"))
        .args(&args)
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
fn env_dependence_flags_ambient_reads_only() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("envdep", tmp.path());
    // --capture-scopes doubles as the §6.2 regression check: env-read
    // recording must not perturb snapshot validation (divergence fails
    // record() outright).
    let db = record(&src, true);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let hits: Vec<&serde_json::Value> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "env-dependence")
        .collect();

    // Both ambient reads are flagged, at note severity.
    let ambient = hits
        .iter()
        .find(|f| {
            f["message"]
                .as_str()
                .unwrap()
                .contains("MY_CUSTOM_TOOL_ROOT")
        })
        .expect("ambient $ENV{} expansion finding");
    assert_eq!(ambient["severity"], "note", "{ambient}");
    assert!(
        ambient["message"]
            .as_str()
            .unwrap()
            .contains("ambient environment"),
        "{ambient}"
    );
    assert!(
        hits.iter()
            .any(|f| f["message"].as_str().unwrap().contains("MY_FEATURE_FLAG")),
        "DEFINED ENV{{X}} condition probe must be flagged: {hits:?}"
    );

    // Project-internal (set(ENV{X}) before the read) and allowlisted
    // names must not appear.
    for f in &hits {
        let m = f["message"].as_str().unwrap();
        assert!(!m.contains("INTERNAL_V"), "resolved env read flagged: {m}");
        assert!(!m.contains("$ENV{PATH}"), "allowlisted name flagged: {m}");
    }
}

#[test]
fn env_dependence_reads_recorded_with_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("envdep", tmp.path());
    let db = record(&src, false);
    let dbs = db.to_string_lossy();
    let q = |sql: &str| stdout(&cmakedb(&src, &["--db", &dbs, "query", sql]));

    // INTERNAL_V resolves to the set(ENV{INTERNAL_V}) write.
    let t = q(
        "SELECT count(*) FROM var_reads r JOIN var_writes w ON w.id=r.resolved_write_id \
         WHERE r.read_kind='env' AND r.name='INTERNAL_V' AND w.write_kind='env'",
    );
    assert_eq!(t.lines().nth(1), Some("1"), "{t}");

    // The ambient reads are recorded and unresolved.
    for name in ["MY_CUSTOM_TOOL_ROOT", "MY_FEATURE_FLAG", "PATH"] {
        let t = q(&format!(
            "SELECT count(*) FROM var_reads WHERE read_kind='env' \
             AND name='{name}' AND resolved_write_id IS NULL"
        ));
        assert_eq!(t.lines().nth(1), Some("1"), "{name}: {t}");
    }

    // Env reads never masquerade as variable reads: no 'expand' rows for
    // env names (ast_var_refs/undefined-reads semantics preserved).
    let t = q("SELECT count(*) FROM var_reads WHERE read_kind!='env' \
         AND name IN ('MY_CUSTOM_TOOL_ROOT','MY_FEATURE_FLAG','INTERNAL_V')");
    assert_eq!(t.lines().nth(1), Some("0"), "{t}");
}
