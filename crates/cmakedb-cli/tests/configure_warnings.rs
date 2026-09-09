//! End-to-end test for the configure-warnings channel: a real cmake
//! configure that emits an author (dev) warning must surface as a
//! `configure-warnings` finding with file/line/severity intact.
//! Requires `cmake` on PATH. Kept out of e2e.rs on purpose (fixture-free:
//! the project is created inline in a tempdir).

use std::path::{Path, PathBuf};
use std::process::Command;

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

#[test]
fn author_warning_becomes_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("proj");
    std::fs::create_dir_all(&src).unwrap();
    // Line 3 genuinely emits, with CMake 4.x:
    //   "CMake Warning (dev) at <src>/CMakeLists.txt:3 (message):"
    std::fs::write(
        src.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.25)\n\
         project(w NONE)\n\
         message(AUTHOR_WARNING \"custom author warning\")\n",
    )
    .unwrap();
    let db = record(&src);

    // Recorder sidecar: raw stderr next to the trace.
    let stderr_txt = std::fs::read_to_string(src.join(".cmakedb/configure-stderr.txt")).unwrap();
    assert!(
        stderr_txt.contains("custom author warning"),
        "captured stderr missing the warning:\n{stderr_txt}"
    );

    let out = Command::new(env!("CARGO_BIN_EXE_cmakedb"))
        .args(["--db", &db.to_string_lossy(), "lint", "--format", "json"])
        .current_dir(&src)
        .output()
        .expect("run lint");
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("lint --format json not JSON ({e}):\n{text}"));
    let findings = v["findings"].as_array().expect("findings array");
    let f = findings
        .iter()
        .find(|f| f["rule"] == "configure-warnings")
        .unwrap_or_else(|| panic!("no configure-warnings finding in:\n{text}"));
    assert_eq!(f["severity"], "warning", "{f}");
    assert_eq!(f["primary"]["line"], 3, "{f}");
    let file = f["primary"]["file"].as_str().unwrap();
    assert!(
        file == "CMakeLists.txt" || file.ends_with("/CMakeLists.txt"),
        "unexpected primary file {file:?}"
    );
    let msg = f["message"].as_str().unwrap();
    assert!(
        ["(dev) ", "(author) "]
            .iter()
            .any(|prefix| msg.starts_with(prefix))
            && msg.contains("custom author warning"),
        "unexpected message {msg:?}"
    );
}
