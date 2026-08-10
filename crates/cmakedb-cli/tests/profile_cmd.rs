//! E2e for `cmakedb profile` over the profiledemo fixture. Assertions
//! are structural only (sections, counts, valid JSON, plausible span) —
//! never on absolute timings. Requires `cmake` on PATH.

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
fn profile_reports_hotspots() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("profiledemo", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy().into_owned();

    // Text: header with span + caveat, all three sections, the fixture
    // function aggregated across its 11 calls.
    let out = cmakedb(&src, &["--db", &dbs, "profile"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    for needle in [
        "configure profile:",
        "time-to-next-event",
        "slowest events (self time):",
        "hottest scopes (inclusive wall time, grouped by name):",
        "hottest commands (total self time):",
        "function build_label",
        "11 calls",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }

    // JSON: valid, all sections populated, plausible span, correct
    // scope aggregation (11 scopes fold into one named row).
    let out = cmakedb(&src, &["--db", &dbs, "profile", "--format", "json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert!(v["total_span_us"].as_i64().unwrap() > 0, "{v}");
    assert!(v["event_count"].as_i64().unwrap() > 0, "{v}");
    assert!(!v["note"].as_str().unwrap().is_empty(), "{v}");
    for section in ["slowest_events", "scopes", "commands"] {
        assert!(
            !v[section].as_array().unwrap().is_empty(),
            "empty {section}: {v}"
        );
    }
    let scope = v["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "build_label")
        .unwrap_or_else(|| panic!("no build_label scope row: {v}"));
    assert_eq!(scope["kind"], "function", "{scope}");
    assert_eq!(scope["calls"].as_i64().unwrap(), 11, "{scope}");
    assert!(scope["total_us"].as_i64().unwrap() >= scope["mean_us"].as_i64().unwrap());
    assert!(
        scope["location"]
            .as_str()
            .unwrap()
            .contains("CMakeLists.txt"),
        "{scope}"
    );
    let string_cmd = v["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["cmd"] == "string")
        .unwrap_or_else(|| panic!("no string command row: {v}"));
    // At least 11 calls x 21 foreach iterations (internal CMake modules
    // may add more).
    assert!(string_cmd["calls"].as_i64().unwrap() >= 231, "{string_cmd}");

    // --top truncates every section.
    let out = cmakedb(
        &src,
        &["--db", &dbs, "profile", "--top", "2", "--format", "json"],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    for section in ["slowest_events", "scopes", "commands"] {
        assert!(
            v[section].as_array().unwrap().len() <= 2,
            "--top 2 not applied to {section}: {v}"
        );
    }
}
