//! E2e for `cmakedb explain` over the provenance fixture: an executed
//! set() line (evaluation + write effect + read influence), a
//! target_link_libraries line (edge effect + File API evidence), and a
//! never-executed module line (static context). Requires `cmake` on
//! PATH.

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
fn explain_answers_what_a_line_did() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("provenance", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy().into_owned();

    // 1. An executed set() line: evaluation with expanded args, the
    //    write effect, and the read influence (line 22's message reads
    //    the value written at line 21).
    let out = cmakedb(&src, &["--db", &dbs, "explain", "CMakeLists.txt:21"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    for needle in [
        "explain CMakeLists.txt:21 — set",
        "evaluated 1 time",
        "executed as:",
        "set(STALE_VALUE second)",
        "context (call chain",
        "effects:",
        "wrote STALE_VALUE",
        "\"second\"",
        "value read 1 time: first at CMakeLists.txt:22",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
    assert!(
        !text.contains("NEVER EXECUTED"),
        "executed line flagged as never executed:\n{text}"
    );

    // The clobbered first write next door has zero resolved reads.
    let out = cmakedb(&src, &["--db", &dbs, "explain", "CMakeLists.txt:20"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("set(STALE_VALUE first)"), "{text}");
    assert!(text.contains("never read"), "{text}");

    // 2. A target_link_libraries line: the edge effect with File API
    //    evidence, resolved via a source-relative path.
    let out = cmakedb(&src, &["--db", &dbs, "explain", "src/app/CMakeLists.txt:2"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    for needle in [
        "explain src/app/CMakeLists.txt:2 — target_link_libraries",
        "created link edge app -PRIVATE-> net",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }

    // 3. A line in a module that was never included: never-executed
    //    message plus static (AST) context, and no runtime sections.
    let out = cmakedb(&src, &["--db", &dbs, "explain", "cmake/Legacy.cmake:1"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    for needle in [
        "NEVER EXECUTED",
        "static context (from the AST, not runtime evidence):",
        "set(UNUSED_MODULE_VAR 1)",
        "never included",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
    assert!(!text.contains("executed as:"), "{text}");

    // 4. JSON: valid, versioned, and structurally faithful for the
    //    executed write line.
    let out = cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "explain",
            "CMakeLists.txt:21",
            "--format",
            "json",
        ],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(v["schema_version"].as_i64().unwrap(), 1, "{v}");
    assert_eq!(v["evaluations"].as_i64().unwrap(), 1, "{v}");
    assert_eq!(v["commands"][0], "set", "{v}");
    assert_eq!(v["arg_sets"][0]["args"][0], "STALE_VALUE", "{v}");
    assert_eq!(v["arg_sets"][0]["args"][1], "second", "{v}");
    let w = &v["writes"][0];
    assert_eq!(w["name"], "STALE_VALUE", "{v}");
    assert_eq!(w["resolved_reads"].as_i64().unwrap(), 1, "{v}");
    assert!(
        w["first_reads"][0]
            .as_str()
            .unwrap()
            .contains("CMakeLists.txt:22"),
        "{v}"
    );
    assert!(v["never_executed"].is_null(), "{v}");

    // JSON for the never-executed line carries the static context.
    let out = cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "explain",
            "cmake/Legacy.cmake:1",
            "--format",
            "json",
        ],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(v["evaluations"].as_i64().unwrap(), 0, "{v}");
    let ne = &v["never_executed"];
    assert!(!ne.is_null(), "{v}");
    assert!(!ne["file_ever_executed"].as_bool().unwrap(), "{v}");
    assert_eq!(ne["commands"][0]["name"], "set", "{v}");

    // 5. A location with no command at all is a loud error.
    let out = cmakedb(&src, &["--db", &dbs, "explain", "CMakeLists.txt:9999"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("no CMake command at"), "{err}");
}
