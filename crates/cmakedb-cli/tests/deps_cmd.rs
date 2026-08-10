//! E2e for `cmakedb deps` over the depsdemo fixture: found/not-found
//! statuses, pinning classifications, and the CycloneDX 1.5 shape
//! (purl only on the pinned github entry). Requires `cmake` on PATH.

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
fn deps_inventory_text_json_cyclonedx() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("depsdemo", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy().into_owned();

    // Text: both mechanisms with statuses, the namespace-grouped and the
    // unmatched imported targets.
    let out = cmakedb(&src, &["--db", &dbs, "deps"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    for needle in [
        "dependency inventory: depsdemo",
        "find_package:",
        "Stub_FOUND=TRUE",
        "version 1.2.3",
        "Stub_DIR=",
        "NoSuchPackage",
        "no NoSuchPackage_FOUND write",
        "FetchContent / ExternalProject:",
        "pinned-commit",
        "mutable-ref",
        "targets: Stub::core",
        "imported targets with no matching find_package:",
        "Orphan::helper",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
    // Status column values (found for Stub, not found for the miss).
    assert!(text.contains("found      Stub"), "{text}");
    assert!(text.contains("not found  NoSuchPackage"), "{text}");

    // JSON: versioned cmakedb schema with full resolution detail.
    let out = cmakedb(&src, &["--db", &dbs, "deps", "--format", "json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(v["schema_version"], 1, "{v}");
    assert_eq!(v["project"], "depsdemo", "{v}");
    let pkgs = v["find_packages"].as_array().unwrap();
    let stub = pkgs.iter().find(|p| p["name"] == "Stub").expect("Stub row");
    assert_eq!(stub["found"], true, "{stub}");
    assert_eq!(stub["quiet"], true, "{stub}");
    assert_eq!(stub["required"], false, "{stub}");
    assert_eq!(stub["requested_version"], "1.0", "{stub}");
    assert_eq!(stub["resolved_version"], "1.2.3", "{stub}");
    assert_eq!(stub["components"][0], "core", "{stub}");
    assert_eq!(stub["imported_targets"][0], "Stub::core", "{stub}");
    assert!(
        stub["declared_at"]
            .as_str()
            .unwrap()
            .contains("CMakeLists.txt"),
        "{stub}"
    );
    let miss = pkgs
        .iter()
        .find(|p| p["name"] == "NoSuchPackage")
        .expect("NoSuchPackage row");
    assert_eq!(miss["found"], false, "{miss}");
    let fetched = v["fetched"].as_array().unwrap();
    let pinned = fetched
        .iter()
        .find(|f| f["name"] == "pinneddep")
        .expect("pinneddep row");
    assert_eq!(pinned["pinning"], "pinned-commit", "{pinned}");
    assert_eq!(pinned["mechanism"], "FetchContent", "{pinned}");
    let tracking = fetched
        .iter()
        .find(|f| f["name"] == "trackingdep")
        .expect("trackingdep row");
    assert_eq!(tracking["pinning"], "mutable-ref", "{tracking}");
    assert_eq!(tracking["git_tag"], "main", "{tracking}");
    assert_eq!(
        v["unmatched_imported_targets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| *t == "Orphan::helper")
            .count(),
        1,
        "{v}"
    );

    // CycloneDX 1.5: required fields, project as metadata.component,
    // every component typed+named, purl ONLY on the pinned github entry.
    let out = cmakedb(&src, &["--db", &dbs, "deps", "--format", "cyclonedx"]);
    assert!(out.status.success());
    let bom: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(bom["bomFormat"], "CycloneDX", "{bom}");
    assert_eq!(bom["specVersion"], "1.5", "{bom}");
    assert!(bom["version"].as_i64().unwrap() >= 1, "{bom}");
    assert_eq!(bom["metadata"]["component"]["name"], "depsdemo", "{bom}");
    assert_eq!(bom["metadata"]["component"]["type"], "application", "{bom}");
    let comps = bom["components"].as_array().unwrap();
    assert!(comps.len() >= 4, "{bom}");
    for c in comps {
        assert_eq!(c["type"], "library", "{c}");
        assert!(!c["name"].as_str().unwrap().is_empty(), "{c}");
        // properties carry the cmakedb facts; mechanism always present.
        assert!(
            c["properties"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"] == "cmakedb:mechanism"),
            "{c}"
        );
    }
    let with_purl: Vec<_> = comps.iter().filter(|c| c.get("purl").is_some()).collect();
    assert_eq!(with_purl.len(), 1, "exactly one purl expected: {bom}");
    assert_eq!(with_purl[0]["name"], "pinneddep", "{bom}");
    assert_eq!(
        with_purl[0]["purl"],
        "pkg:github/example/pinneddep@0123456789abcdef0123456789abcdef01234567",
        "{bom}"
    );
    let stub_c = comps
        .iter()
        .find(|c| c["name"] == "Stub")
        .expect("Stub component");
    assert_eq!(stub_c["version"], "1.2.3", "{stub_c}");
    assert!(
        stub_c["properties"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "cmakedb:found" && p["value"] == "true"),
        "{stub_c}"
    );
    let miss_c = comps
        .iter()
        .find(|c| c["name"] == "NoSuchPackage")
        .expect("NoSuchPackage component");
    assert!(
        miss_c["properties"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "cmakedb:found" && p["value"] == "false"),
        "{miss_c}"
    );
}
