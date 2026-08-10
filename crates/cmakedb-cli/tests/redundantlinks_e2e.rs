//! E2e for the `redundant-links` lint over fixtures/redundantlinks.
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
fn redundant_links_flags_provided_edges_only() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("redundantlinks", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let findings = v["findings"].as_array().unwrap();

    // The fixture config enables only this rule; exactly the three
    // positives (one-hop, two-hop, INTERFACE-library hop) are reported.
    assert_eq!(findings.len(), 3, "expected three findings: {findings:?}");
    for f in findings {
        assert_eq!(f["rule"], "redundant-links", "{f}");
        assert_eq!(f["severity"], "note", "{f}");
        // The static-archive link-ordering caveat is part of every message.
        let msg = f["message"].as_str().unwrap();
        assert!(msg.contains("Static-archive link ordering"), "{msg}");
        assert!(msg.contains("redundant for linking"), "{msg}");
    }
    let by_target = |t: &str| {
        findings
            .iter()
            .find(|f| {
                f["message"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("`{t}`"))
            })
            .unwrap_or_else(|| panic!("no finding for {t}: {findings:?}"))
    };

    // One hop: app -> bcore is provided by amid's PUBLIC interface.
    let one = by_target("app");
    let msg = one["message"].as_str().unwrap();
    assert!(msg.contains("`app` links `bcore` directly"), "{msg}");
    assert!(msg.contains("via `amid`'s PUBLIC interface"), "{msg}");
    assert!(msg.contains("(amid -> bcore)"), "{msg}");
    assert!(msg.contains("(CMakeLists.txt:11)"), "{msg}");
    assert_eq!(one["primary"]["line"], 11, "{one}");
    // Evidence: the providing T -> A edge, then each chain hop.
    assert!(
        one["related"][0][1]
            .as_str()
            .unwrap()
            .contains("`app` links `amid` (PRIVATE)"),
        "{one}"
    );
    assert_eq!(one["related"][1][0]["line"], 9, "{one}");

    // Two hops: the full chain is rendered.
    let two = by_target("app2");
    let msg = two["message"].as_str().unwrap();
    assert!(msg.contains("`app2` links `deep` directly"), "{msg}");
    assert!(msg.contains("(top2 -> mid2 -> deep)"), "{msg}");
    assert_eq!(two["related"].as_array().unwrap().len(), 3, "{two}");

    // INTERFACE-library hop counts as a propagating route.
    let ifc = by_target("app4");
    let msg = ifc["message"].as_str().unwrap();
    assert!(msg.contains("`app4` links `bcore2` directly"), "{msg}");
    assert!(msg.contains("via `iface`'s INTERFACE propagation"), "{msg}");
    assert!(msg.contains("(iface -> bcore2)"), "{msg}");

    // Negatives: a PRIVATE-only route provides no interface (app3), and
    // external/string destinations (`m`) are never flagged.
    for needle in ["`app3`", "`privb`", "links `m`"] {
        assert!(
            !findings
                .iter()
                .any(|f| f["message"].as_str().unwrap().contains(needle)),
            "{needle} must not be flagged: {findings:?}"
        );
    }
}
