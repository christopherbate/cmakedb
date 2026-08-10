//! End-to-end tests for `cmakedb matrix` (record-per-preset + intersected
//! lint in one command) over fixtures/matrixdemo. Requires `cmake` on
//! PATH. Helpers copied from e2e.rs.

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

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A user-presets file adding a preset whose configure must fail (its
/// cacheVariables point CMAKE_TOOLCHAIN_FILE at a nonexistent path).
fn write_broken_user_preset(src: &Path) {
    std::fs::write(
        src.join("CMakeUserPresets.json"),
        r#"{
  "version": 3,
  "configurePresets": [
    {
      "name": "broken",
      "binaryDir": "${sourceDir}/build/broken",
      "cacheVariables": {
        "CMAKE_TOOLCHAIN_FILE": "${sourceDir}/no-such-toolchain.cmake"
      }
    }
  ]
}
"#,
    )
    .unwrap();
}

#[test]
fn matrix_records_all_presets_and_intersects() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("matrixdemo", tmp.path());

    // Default run: both non-hidden presets recorded, hidden one skipped,
    // findings are the intersection. ALWAYS_DEAD (dead in both configs)
    // is a warning, so the default fail-on gate trips → exit 1.
    let out = cmakedb(&src, &["matrix"]);
    let err = stderr(&out);
    let text = stdout(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout:\n{text}\nstderr:\n{err}"
    );

    // Both non-hidden presets recorded, sequentially, to matrix/<preset>.db.
    assert!(src.join(".cmakedb/matrix/base.db").exists(), "{err}");
    assert!(src.join(".cmakedb/matrix/extra.db").exists(), "{err}");
    // The hidden preset is skipped entirely.
    assert!(!src.join(".cmakedb/matrix/common.db").exists(), "{err}");
    assert!(!err.contains("recording preset 'common'"), "{err}");

    // base uses its own (inherited) binaryDir; extra declares none and
    // gets the matrix fallback dir.
    assert!(src.join("build/base/CMakeCache.txt").exists(), "{err}");
    assert!(
        src.join(".cmakedb/matrix-build/extra/CMakeCache.txt")
            .exists(),
        "{err}"
    );

    // Summary table on stderr: one ok row per preset.
    for needle in ["preset", "events", "seconds", "status"] {
        assert!(err.contains(needle), "missing {needle:?} in table:\n{err}");
    }
    let base_row = err.lines().find(|l| l.starts_with("base")).expect(&err);
    let extra_row = err.lines().find(|l| l.starts_with("extra")).expect(&err);
    assert!(base_row.trim_end().ends_with("ok"), "{base_row}");
    assert!(extra_row.trim_end().ends_with("ok"), "{extra_row}");

    // The intersection drops the config-specific dead option and keeps
    // the one dead everywhere.
    assert!(!text.contains("FEATURE_X"), "{text}");
    assert!(text.contains("ALWAYS_DEAD"), "{text}");
    assert!(text.contains("present in all 2 recordings"), "{text}");

    // --fail-on error: same findings, but the gate passes → exit 0.
    // JSON goes through --output (stdout carries cmake's own configure
    // output, so machine formats belong in a file).
    let findings_json = src.join("findings.json");
    let out = cmakedb(
        &src,
        &[
            "matrix",
            "--fail-on",
            "error",
            "--format",
            "json",
            "--output",
            findings_json.to_string_lossy().as_ref(),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&findings_json).unwrap()).expect("json");
    let msgs: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    assert!(msgs.iter().any(|m| m.contains("ALWAYS_DEAD")), "{msgs:?}");
    assert!(msgs.iter().all(|m| !m.contains("FEATURE_X")), "{msgs:?}");
}

#[test]
fn matrix_reports_failed_preset_and_continues() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("matrixdemo", tmp.path());
    write_broken_user_preset(&src);

    // Even with a passing findings gate, a failed preset → exit 1.
    let out = cmakedb(&src, &["matrix", "--fail-on", "error"]);
    let err = stderr(&out);
    let text = stdout(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout:\n{text}\nstderr:\n{err}"
    );

    // The broken preset failed and left no database; the others recorded.
    let broken_row = err.lines().find(|l| l.starts_with("broken")).expect(&err);
    assert!(broken_row.contains("FAILED"), "{broken_row}");
    assert!(!src.join(".cmakedb/matrix/broken.db").exists());
    assert!(src.join(".cmakedb/matrix/base.db").exists(), "{err}");
    assert!(src.join(".cmakedb/matrix/extra.db").exists(), "{err}");

    // Findings still come from the successful recordings' intersection.
    assert!(text.contains("ALWAYS_DEAD"), "{text}");
    assert!(!text.contains("FEATURE_X"), "{text}");
    assert!(text.contains("present in all 2 recordings"), "{text}");
}

#[test]
fn matrix_fail_fast_stops_at_first_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("matrixdemo", tmp.path());
    write_broken_user_preset(&src);

    // User presets are discovered first, so 'broken' records first;
    // --fail-fast must stop before 'base'/'extra' ever run. With no
    // successful recording there is nothing to lint → hard error (2).
    let out = cmakedb(&src, &["matrix", "--fail-fast"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("no preset recorded successfully"), "{err}");
    assert!(!src.join(".cmakedb/matrix/base.db").exists(), "{err}");
    assert!(!src.join(".cmakedb/matrix/extra.db").exists(), "{err}");
}

#[test]
fn matrix_explicit_preset_subset() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("matrixdemo", tmp.path());

    // A single explicit preset: no intersection, single-config view.
    let out = cmakedb(&src, &["matrix", "--preset", "base", "--fail-on", "error"]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(src.join(".cmakedb/matrix/base.db").exists());
    assert!(!src.join(".cmakedb/matrix/extra.db").exists());
    assert!(text.contains("ALWAYS_DEAD"), "{text}");
    assert!(text.contains("FEATURE_X"), "{text}"); // dead in this config only
    assert!(!text.contains("present in all"), "{text}");
}
