//! E2e for `cmakedb why-not` (negative provenance) over fixtures/whynot.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

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
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    db
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn why_not_explains_all_absence_classes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("whynot", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy();

    // Guard failed, with the observed value and its provenance; the
    // executed link into a *different* target is called out as such.
    let t = stdout(&cmakedb(
        &src,
        &["--db", &dbs, "why-not", "links", "app", "zlib_stub"],
    ));
    assert!(t.contains("does not link"), "{t}");
    assert!(t.contains("guard if(ENABLE_COMPRESSION)"), "{t}");
    assert!(t.contains("chose another branch"), "{t}");
    assert!(t.contains("ENABLE_COMPRESSION = \"OFF\""), "{t}");
    assert!(t.contains("CMakeLists.txt:3"), "value origin: {t}");
    assert!(t.contains("links zlib_stub into other, not app"), "{t}");

    // Wrapper command (add_* with the target as first argument) counts as
    // a definition site and gets the guard treatment.
    let t = stdout(&cmakedb(
        &src,
        &["--db", &dbs, "why-not", "target", "ztool"],
    ));
    assert!(t.contains("add_wrapped_tool(ztool)"), "{t}");
    assert!(t.contains("guard if(ENABLE_COMPRESSION)"), "{t}");

    // Uncalled function.
    let t = stdout(&cmakedb(
        &src,
        &["--db", &dbs, "why-not", "target", "extra"],
    ));
    assert!(t.contains("define_extra_target"), "{t}");
    assert!(t.contains("never called"), "{t}");

    // Never-added directory.
    let t = stdout(&cmakedb(
        &src,
        &["--db", &dbs, "why-not", "target", "orphan"],
    ));
    assert!(t.contains("directory was never added"), "{t}");
    assert!(t.contains("sub/CMakeLists.txt:1"), "{t}");

    // Guarded set().
    let t = stdout(&cmakedb(
        &src,
        &["--db", &dbs, "why-not", "set", "SPECIAL_FLAG"],
    ));
    assert!(t.contains("was never set"), "{t}");
    assert!(t.contains("_unused_gate = \"OFF\""), "{t}");

    // Existing things redirect.
    let t = stdout(&cmakedb(&src, &["--db", &dbs, "why-not", "target", "app"]));
    assert!(t.contains("DOES exist"), "{t}");
    let t = stdout(&cmakedb(
        &src,
        &["--db", &dbs, "why-not", "set", "_unused_gate"],
    ));
    assert!(t.contains("WAS written"), "{t}");

    // Typo suggestion.
    let t = stdout(&cmakedb(
        &src,
        &["--db", &dbs, "why-not", "links", "app", "zlib_stubb"],
    ));
    assert!(t.contains("did you mean 'zlib_stub'"), "{t}");

    // JSON output is versionable/parsable.
    let out = cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "why-not",
            "links",
            "app",
            "zlib_stub",
            "--format",
            "json",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let kinds: Vec<&str> = v["sites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["status"]["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"guard-failed"), "{v}");
    assert!(kinds.contains(&"executed-differently"), "{v}");
}
