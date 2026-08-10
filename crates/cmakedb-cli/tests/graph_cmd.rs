//! E2e for `cmakedb graph` over the provenance fixture (app -PRIVATE->
//! net -PUBLIC-> http -INTERFACE-> ZLIB::ZLIB -INTERFACE-> z, where
//! ZLIB::ZLIB is an imported interface target and `z` is external).
//! Requires `cmake` on PATH.

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

fn graph_json(src: &Path, dbs: &str, extra: &[&str]) -> serde_json::Value {
    let out = cmakedb(
        src,
        &[&["--db", dbs, "graph", "--format", "json"], extra].concat(),
    );
    assert!(
        out.status.success(),
        "graph failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&stdout(&out)).expect("valid JSON")
}

fn node_names(v: &serde_json::Value) -> Vec<String> {
    v["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn graph_exports_with_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("provenance", tmp.path());
    let db = record(&src);
    let dbs = db.to_string_lossy().into_owned();

    // --- DOT (default format): valid quoting, legend, styled edges. ---
    let out = cmakedb(&src, &["--db", &dbs, "graph"]);
    let dot = stdout(&out);
    assert!(out.status.success(), "{dot}");
    assert!(dot.contains("digraph cmakedb {"), "{dot}");
    assert!(
        dot.contains("// edges: solid = PUBLIC, dashed = PRIVATE, dotted = INTERFACE"),
        "legend missing:\n{dot}"
    );
    // Namespaced name stays one quoted ID.
    assert!(dot.contains("\"ZLIB::ZLIB\""), "{dot}");
    // app -> net is PRIVATE: dashed, origin in the tooltip.
    let edge = dot
        .lines()
        .find(|l| l.contains("\"app\" -> \"net\""))
        .unwrap_or_else(|| panic!("no app->net edge:\n{dot}"));
    assert!(edge.contains("style=dashed"), "{edge}");
    assert!(
        edge.contains("tooltip=\"PRIVATE src/app/CMakeLists.txt:2\""),
        "{edge}"
    );
    // External z: distinct octagon node.
    let znode = dot
        .lines()
        .find(|l| l.trim_start().starts_with("\"z\" ["))
        .unwrap_or_else(|| panic!("no external z node:\n{dot}"));
    assert!(znode.contains("shape=octagon"), "{znode}");
    assert!(znode.contains("(external)"), "{znode}");
    // Imported interface target is visually distinct.
    let zlib = dot
        .lines()
        .find(|l| l.trim_start().starts_with("\"ZLIB::ZLIB\" ["))
        .unwrap();
    assert!(zlib.contains("filled"), "{zlib}");
    assert!(zlib.contains("dashed"), "{zlib}");

    // --- Mermaid: synthetic ids, shapes, dashed PRIVATE with link text. ---
    let out = cmakedb(&src, &["--db", &dbs, "graph", "--format", "mermaid"]);
    let mmd = stdout(&out);
    assert!(out.status.success(), "{mmd}");
    assert!(mmd.contains("graph LR"), "{mmd}");
    assert!(mmd.contains("[\"app\"]"), "executable shape:\n{mmd}");
    assert!(mmd.contains("(\"net\")"), "library shape:\n{mmd}");
    assert!(mmd.contains("{{\"z\"}}"), "external shape:\n{mmd}");
    assert!(mmd.contains("(\"ZLIB::ZLIB\")"), "{mmd}");
    // `::` never appears as a node id — only inside quoted labels.
    for line in mmd.lines().filter(|l| !l.starts_with("%%")) {
        assert!(
            !line.contains("::") || line.contains("\"ZLIB::ZLIB\""),
            "raw :: outside a quoted label: {line}"
        );
    }
    assert!(
        mmd.contains("-. \"PRIVATE src/app/CMakeLists.txt:2\" .->"),
        "{mmd}"
    );
    assert!(
        mmd.contains("class ") && mmd.contains(" imported;"),
        "{mmd}"
    );

    // --- JSON: versioned, full node/edge inventory. ---
    let v = graph_json(&src, &dbs, &[]);
    assert_eq!(v["cmakedb_graph_version"], 1, "{v}");
    let names = node_names(&v);
    for t in ["app", "net", "http", "ZLIB::ZLIB", "z"] {
        assert!(names.contains(&t.to_string()), "missing {t}: {v}");
    }
    let by_name = |n: &str| {
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(by_name("z")["external"], true, "{v}");
    assert_eq!(by_name("ZLIB::ZLIB")["imported"], true, "{v}");
    assert_eq!(by_name("ZLIB::ZLIB")["external"], false, "{v}");
    assert_eq!(by_name("app")["type"], "EXECUTABLE", "{v}");
    let edges = v["edges"].as_array().unwrap();
    let app_net = edges
        .iter()
        .find(|e| e["src"] == "app" && e["dst"] == "net")
        .unwrap_or_else(|| panic!("no app->net edge: {v}"));
    assert_eq!(app_net["visibility"], "PRIVATE", "{v}");
    assert_eq!(app_net["origin"], "src/app/CMakeLists.txt:2", "{v}");
    assert!(
        edges
            .iter()
            .any(|e| e["src"] == "ZLIB::ZLIB" && e["dst"] == "z" && e["visibility"] == "INTERFACE"),
        "{v}"
    );

    // --- --target: forward closure only (net's closure excludes app). ---
    let v = graph_json(&src, &dbs, &["--target", "net"]);
    let names = node_names(&v);
    assert!(!names.contains(&"app".to_string()), "{v}");
    for t in ["net", "http", "ZLIB::ZLIB", "z"] {
        assert!(names.contains(&t.to_string()), "missing {t}: {v}");
    }
    // app's closure crosses its PRIVATE edge (full closure by design).
    let v = graph_json(&src, &dbs, &["--target", "app"]);
    assert!(node_names(&v).contains(&"z".to_string()), "{v}");

    // --- --no-external drops z and the edge into it. ---
    let v = graph_json(&src, &dbs, &["--no-external"]);
    assert!(!node_names(&v).contains(&"z".to_string()), "{v}");
    assert!(
        !v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["dst"] == "z"),
        "{v}"
    );

    // --- --path: prefix over defining locations, targets only. ---
    let v = graph_json(&src, &dbs, &["--path", "src"]);
    let names = node_names(&v);
    for t in ["app", "net", "http"] {
        assert!(names.contains(&t.to_string()), "missing {t}: {v}");
    }
    // ZLIB::ZLIB is defined in cmake/FindDeps.cmake — filtered out, and
    // the http -> ZLIB::ZLIB edge goes with it.
    assert!(!names.contains(&"ZLIB::ZLIB".to_string()), "{v}");
    let v = graph_json(&src, &dbs, &["--path", "src/app"]);
    assert_eq!(node_names(&v), vec!["app".to_string()], "{v}");
    assert!(v["edges"].as_array().unwrap().is_empty(), "{v}");

    // Unknown target fails loudly.
    let out = cmakedb(&src, &["--db", &dbs, "graph", "--target", "nope"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown target"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
