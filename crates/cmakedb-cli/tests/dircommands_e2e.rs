//! End-to-end test for the `directory-commands` lint pass over the
//! `dircommands` fixture: directory-scope commands are reported with the
//! exact affected targets (including a subdirectory target inherited via
//! the scope chain), target-scoped calls are not flagged, and a trailing
//! directory command that affects nothing is called out as dead.

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
fn directory_commands_fixture_lints() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("dircommands", tmp.path());
    // Restrict the run to this rule via config, exercising the enable knob.
    std::fs::write(
        src.join(".cmakedb.toml"),
        "[lint]\nenable = [\"directory-commands\"]\n",
    )
    .unwrap();
    let db = record(&src);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let text = stdout(&out);
    let v: serde_json::Value = serde_json::from_str(&text).expect("valid findings json");
    let findings = v["findings"].as_array().expect("findings array");
    assert!(
        findings.iter().all(|f| f["rule"] == "directory-commands"),
        "rule restriction leaked other rules:\n{text}"
    );
    assert_eq!(findings.len(), 4, "expected exactly 4 findings:\n{text}");
    assert!(findings.iter().all(|f| f["severity"] == "note"), "{text}");

    // Positive: include_directories affects util, app and (inherited)
    // subtool, and proposes the target-scoped equivalent.
    let inc = findings
        .iter()
        .find(|f| {
            f["message"]
                .as_str()
                .unwrap()
                .starts_with("include_directories")
        })
        .expect("include_directories finding");
    let msg = inc["message"].as_str().unwrap();
    assert!(
        msg.contains("include_directories at directory scope affects 3 target(s)"),
        "{text}"
    );
    assert!(
        msg.contains("prefer target_include_directories(<tgt> PRIVATE"),
        "{text}"
    );
    assert!(
        msg.contains("affected: util, app, subtool"),
        "affected names missing/misordered:\n{text}"
    );
    assert!(
        msg.contains("1 of these are defined in subdirectories and inherit it"),
        "inheritance not noted:\n{text}"
    );
    // The utility target never appears in an affected set.
    assert!(!msg.contains("stamp"), "{text}");
    // Related spans point at the affected targets' definition sites,
    // marking the subdirectory one as inherited.
    let related = inc["related"].as_array().expect("related array");
    assert_eq!(related.len(), 3, "{text}");
    assert!(
        related.iter().any(|r| {
            r[1].as_str().unwrap().contains("subtool")
                && r[1].as_str().unwrap().contains("subdirectory")
                && r[0]["file"].as_str().unwrap().contains("sub")
        }),
        "no inherited-target evidence span:\n{text}"
    );

    // Positive: add_definitions gets the same treatment.
    let defs = findings
        .iter()
        .find(|f| {
            f["message"]
                .as_str()
                .unwrap()
                .starts_with("add_definitions")
        })
        .expect("add_definitions finding");
    let msg = defs["message"].as_str().unwrap();
    assert!(msg.contains("affects 3 target(s)"), "{text}");
    assert!(
        msg.contains("prefer target_compile_definitions(<tgt> PRIVATE"),
        "{text}"
    );
    assert!(msg.contains("affected: util, app, subtool"), "{text}");

    // Retroactivity: the trailing add_compile_definitions affects the two
    // targets defined *before* it in the same directory, but not the
    // subdirectory target (properties were snapshotted at
    // add_subdirectory time).
    let retro = findings
        .iter()
        .find(|f| {
            f["message"]
                .as_str()
                .unwrap()
                .starts_with("add_compile_definitions")
        })
        .expect("add_compile_definitions finding");
    let msg = retro["message"].as_str().unwrap();
    assert!(msg.contains("affects 2 target(s)"), "{text}");
    assert!(msg.contains("affected: util, app"), "{text}");
    assert!(
        !msg.contains("subtool"),
        "retroactivity leaked into sub:\n{text}"
    );

    // Bonus catch: the trailing link_libraries (order-sensitive) affects
    // nothing — dead directory command.
    let dead = findings
        .iter()
        .find(|f| f["message"].as_str().unwrap().starts_with("link_libraries"))
        .expect("link_libraries finding");
    let msg = dead["message"].as_str().unwrap();
    assert!(
        msg.contains("affects no targets") && msg.contains("dead directory command"),
        "{text}"
    );

    // Negative: the target-scoped call is never flagged.
    assert!(
        !findings.iter().any(|f| {
            f["message"]
                .as_str()
                .unwrap()
                .starts_with("target_include_directories")
        }),
        "target-scoped call flagged:\n{text}"
    );
}
