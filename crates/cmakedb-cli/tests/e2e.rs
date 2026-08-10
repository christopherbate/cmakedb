//! End-to-end tests over the fixture corpus (design §6.1): record real
//! cmake runs, then assert on pass findings and provenance output.
//! Requires `cmake` (and `ninja` for the overshare test) on PATH.

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

fn record(src: &Path, extra: &[&str]) -> PathBuf {
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
    if !extra.is_empty() {
        args.push("--".into());
        args.push("-S".into());
        args.push(src.to_string_lossy().into_owned());
        args.push("-B".into());
        args.push(src.join("build").to_string_lossy().into_owned());
        args.extend(extra.iter().map(|s| s.to_string()));
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
fn provenance_fixture_full_suite() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("provenance", tmp.path());
    let db = record(&src, &[]);
    let dbs = db.to_string_lossy();
    let dbargs = ["--db", &dbs];

    // §2.2-style why-links chain with locations at every hop.
    let out = cmakedb(&src, &[&dbargs[..], &["why-links", "app", "z"]].concat());
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    for needle in [
        "PRIVATE net",
        "src/app/CMakeLists.txt:2",
        "PUBLIC http",
        "src/net/CMakeLists.txt:2",
        "INTERFACE ZLIB::ZLIB",
        "src/http/CMakeLists.txt:3",
        "cmake/FindDeps.cmake:2",
    ] {
        assert!(
            text.contains(needle),
            "why-links missing {needle:?}:\n{text}"
        );
    }

    // Dead options: plain unread + read-only-by-dead-code qualifier.
    let text = stdout(&cmakedb(
        &src,
        &[&dbargs[..], &["dead", "options"]].concat(),
    ));
    assert!(text.contains("BUILD_LEGACY_DRIVER"), "{text}");
    assert!(text.contains("ENABLE_PROFILING"), "{text}");
    assert!(
        text.contains("read only by code that never executed"),
        "{text}"
    );
    assert!(
        !text.contains("ENABLE_LOGGING"),
        "ENABLE_LOGGING is read:\n{text}"
    );

    let text = stdout(&cmakedb(
        &src,
        &[&dbargs[..], &["dead", "functions"]].concat(),
    ));
    assert!(text.contains("setup_prof"), "{text}");
    assert!(
        !text.contains("set_common_flags"),
        "macro was called:\n{text}"
    );

    let text = stdout(&cmakedb(
        &src,
        &[&dbargs[..], &["dead", "modules"]].concat(),
    ));
    assert!(text.contains("Legacy.cmake"), "{text}");
    assert!(
        !text.contains("FindDeps.cmake"),
        "FindDeps was included:\n{text}"
    );

    let text = stdout(&cmakedb(&src, &[&dbargs[..], &["clobbers"]].concat()));
    assert!(text.contains("STALE_VALUE"), "{text}");

    let text = stdout(&cmakedb(&src, &[&dbargs[..], &["scope-leaks"]].concat()));
    assert!(text.contains("set_common_flags"), "{text}");
    assert!(text.contains("tmp_flags"), "{text}");

    // why-flag across a propagation hop, with File API evidence.
    let text = stdout(&cmakedb(
        &src,
        &[&dbargs[..], &["why-flag", "app", "NET_VERSION"]].concat(),
    ));
    assert!(text.contains("PUBLIC define 'NET_VERSION'"), "{text}");
    assert!(text.contains("NET_VERSION=2"), "{text}");

    // why-value history in execution order.
    let text = stdout(&cmakedb(
        &src,
        &[&dbargs[..], &["why-value", "STALE_VALUE"]].concat(),
    ));
    let first = text.find("= first").expect(&text);
    let second = text.find("= second").expect(&text);
    assert!(first < second);

    // SARIF is valid JSON with the expected shape and rules.
    let out = cmakedb(
        &src,
        &[&dbargs[..], &["lint", "--format", "sarif"]].concat(),
    );
    let sarif: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid json");
    assert_eq!(sarif["version"], "2.1.0");
    let rules: Vec<&str> = sarif["runs"][0]["tool"]["driver"]["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(rules.contains(&"dead-options"), "{rules:?}");
    // Findings exist at warning level -> default fail-on=warning exits 1,
    // fail-on=error exits 0 (§2.5 ratchet semantics).
    assert_eq!(out.status.code(), Some(1));
    let out = cmakedb(
        &src,
        &[&dbargs[..], &["lint", "--fail-on", "error"]].concat(),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    // Raw SQL access.
    let text = stdout(&cmakedb(
        &src,
        &[
            &dbargs[..],
            &[
                "query",
                "SELECT name FROM targets WHERE alias_of IS NULL ORDER BY name",
            ],
        ]
        .concat(),
    ));
    for t in ["app", "net", "http", "ZLIB::ZLIB"] {
        assert!(text.contains(t), "{text}");
    }
    // try_compile scratch targets must not leak into the graph.
    assert!(!text.contains("cmTC_"), "scratch target leaked:\n{text}");
}

#[test]
fn scopes_fixture_replay_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("scopes", tmp.path());
    let db = record(&src, &[]);
    let dbs = db.to_string_lossy();
    let q = |sql: &str| stdout(&cmakedb(&src, &["--db", &dbs, "query", sql]));

    // PARENT_SCOPE write of RESULT resolved by the caller's read.
    let t = q(
        "SELECT count(*) FROM var_reads r JOIN var_writes w ON w.id=r.resolved_write_id \
               WHERE r.name='RESULT' AND w.write_kind='parent_scope'",
    );
    assert!(t.lines().nth(1) == Some("1"), "{t}");

    // In-block read of TOP_VAL sees the block write; after-block read sees
    // the original directory-scope write.
    let t = q("SELECT s.kind FROM var_reads r \
               JOIN var_writes w ON w.id=r.resolved_write_id \
               JOIN scopes s ON s.id=w.scope_id \
               WHERE r.name='TOP_VAL' ORDER BY r.id");
    let kinds: Vec<&str> = t.lines().skip(1).collect();
    assert_eq!(kinds, ["block", "root", "root"], "{t}");

    // Subdirectory read inherits the parent directory value (root write).
    let t = q("SELECT count(*) FROM var_reads r \
               JOIN events e ON e.id=r.event_id JOIN files f ON f.id=e.file_id \
               JOIN var_writes w ON w.id=r.resolved_write_id \
               WHERE r.name='TOP_VAL' AND f.path LIKE '%sub/CMakeLists.txt'");
    assert_eq!(t.lines().nth(1), Some("1"), "{t}");

    // Macro write lands in caller scope; function-local write stays local
    // (no read of 'inner' outside; the write's scope kind is 'function').
    let t = q(
        "SELECT s.kind FROM var_writes w JOIN scopes s ON s.id=w.scope_id \
               WHERE w.name='inner'",
    );
    assert_eq!(t.lines().nth(1), Some("function"), "{t}");
    let t = q(
        "SELECT s.kind FROM var_writes w JOIN scopes s ON s.id=w.scope_id \
               WHERE w.name='from_macro'",
    );
    assert_eq!(t.lines().nth(1), Some("root"), "{t}");
}

#[test]
fn diff_detects_graph_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let a = stage("scopes", tmp.path().join("a").as_path());
    let b = stage("scopes", tmp.path().join("b").as_path());
    let db_a = record(&a, &[]);
    let db_b = record(&b, &["-DWITH_EXTRA=ON"]);

    let out = cmakedb(
        tmp.path(),
        &["diff", &db_a.to_string_lossy(), &db_b.to_string_lossy()],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("targets added"), "{text}");
    assert!(text.contains("extra"), "{text}");
    assert!(text.contains("base -[INTERFACE]-> extra"), "{text}");

    // Same recording diffed against itself is clean, exit 0.
    let out = cmakedb(
        tmp.path(),
        &["diff", &db_a.to_string_lossy(), &db_a.to_string_lossy()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("equivalent"));
}

#[test]
fn wrapper_functions_resolve_through_parse_arguments() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("wrapper", tmp.path());
    let db = record(&src, &[]);
    let dbs = db.to_string_lossy();

    // Targets created inside the wrapper exist; edge engine->core PUBLIC
    // originates inside the wrapper body.
    let text = stdout(&cmakedb(
        &src,
        &["--db", &dbs, "why-links", "engine", "core"],
    ));
    assert!(text.contains("PUBLIC core"), "{text}");
    assert!(
        text.contains("CMakeLists.txt:7"),
        "origin inside wrapper body:\n{text}"
    );

    // cmake_parse_arguments outputs were synthesized as writes and the
    // wrapper's reads of them resolved.
    let t = stdout(&cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "query",
            "SELECT count(*) FROM var_reads r JOIN var_writes w ON w.id=r.resolved_write_id \
           WHERE r.name='APL_DEPS'",
        ],
    ));
    assert!(t.lines().nth(1).map(|n| n != "0").unwrap_or(false), "{t}");
}

#[test]
fn pathological_parsing_joins_all_events() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("pathological", tmp.path());
    let db = record(&src, &[]);
    let dbs = db.to_string_lossy();

    // The loop body executes 3 times but is one AST node: 3 events, 1 node.
    let t = stdout(&cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "query",
            "SELECT count(*), count(DISTINCT e.node_id) FROM events e \
           JOIN files f ON f.id=e.file_id \
           WHERE f.in_source=1 AND e.line=9 AND e.cmd_lower='set'",
        ],
    ));
    assert_eq!(t.lines().nth(1), Some("3\t1"), "{t}");

    // Every project-file event joined to an AST node (multi-line strings,
    // bracket args, genexes included).
    let t = stdout(&cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "query",
            "SELECT count(*) FROM events e JOIN files f ON f.id=e.file_id \
           WHERE f.in_source=1 AND e.node_id IS NULL",
        ],
    ));
    assert_eq!(t.lines().nth(1), Some("0"), "{t}");

    // Bracket argument suppressed the fake ${NOT_REF} reference.
    let t = stdout(&cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "query",
            "SELECT count(*) FROM var_reads WHERE name='NOT_REF'",
        ],
    ));
    assert_eq!(t.lines().nth(1), Some("0"), "{t}");
}

#[test]
fn overshare_with_ninja_deps() {
    if Command::new("ninja").arg("--version").output().is_err() {
        eprintln!("skipping: ninja not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("provenance", tmp.path());
    let db = record(&src, &["-G", "Ninja"]);
    let dbs = db.to_string_lossy();
    let build = src.join("build");
    assert!(Command::new("ninja")
        .arg("-C")
        .arg(&build)
        .status()
        .unwrap()
        .success());

    let out = cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "overshare",
            "--deps-from",
            &build.to_string_lossy(),
        ],
    );
    let text = stdout(&out);
    // http's PUBLIC include dir is used only by http itself (main.c never
    // includes http.h) -> PRIVATE suggestion (§4.4).
    assert!(text.contains("consider PRIVATE"), "{text}");
    assert!(text.contains("http"), "{text}");
}

#[test]
fn modernize_check_fix_and_verify() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("legacy", tmp.path());
    let db = record(&src, &[]);
    let dbs = db.to_string_lossy();
    let original_top = std::fs::read_to_string(src.join("CMakeLists.txt")).unwrap();

    // --check: reports both legacy commands affecting both targets and
    // renders the planned patches as diffs.
    let out = cmakedb(&src, &["--db", &dbs, "modernize", "--check"]);
    let text = stdout(&out);
    assert!(text.contains("include_directories("), "{text}");
    assert!(text.contains("2 target(s): core, child"), "{text}");
    assert!(text.contains("modernize-add-definitions"), "{text}");
    assert!(
        text.contains("-include_directories"),
        "diff shows deletion:\n{text}"
    );
    assert!(
        text.contains("+target_include_directories(core PRIVATE"),
        "{text}"
    );
    assert!(
        text.contains("+target_include_directories(child PRIVATE"),
        "{text}"
    );
    // --check must not touch the tree.
    assert_eq!(
        std::fs::read_to_string(src.join("CMakeLists.txt")).unwrap(),
        original_top
    );

    // --fix: applies, re-records, verifies isomorphism.
    let out = cmakedb(&src, &["--db", &dbs, "modernize", "--fix"]);
    let text = stdout(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("verified"), "{text}");

    let top = std::fs::read_to_string(src.join("CMakeLists.txt")).unwrap();
    let sub = std::fs::read_to_string(src.join("sub/CMakeLists.txt")).unwrap();
    assert!(!top.contains("\ninclude_directories("), "{top}");
    assert!(!top.contains("\nadd_definitions("), "{top}");
    assert!(
        top.contains("target_include_directories(core PRIVATE"),
        "{top}"
    );
    assert!(
        top.contains("target_compile_definitions(core PRIVATE LEGACY_MODE=1"),
        "{top}"
    );
    assert!(
        sub.contains("target_include_directories(child PRIVATE"),
        "{sub}"
    );
    // Lossless outside edits: comments and formatting survive.
    assert!(
        top.contains("# legacy directory-scope commands to modernize"),
        "{top}"
    );
    assert!(top.contains("# keep this trailing comment"), "{top}");

    // The database was replaced by the post-fix recording: planning again
    // finds nothing.
    let out = cmakedb(&src, &["--db", &dbs, "modernize", "--check"]);
    assert!(stdout(&out).contains("nothing to fix"), "{}", stdout(&out));
}

#[test]
fn sql_user_pass_runs_in_lint() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("provenance", tmp.path());
    let db = record(&src, &[]);
    let dbs = db.to_string_lossy();

    // A user pass flagging every option() declaration in project files
    // (result-shape convention: file/line/message[/severity]).
    std::fs::create_dir_all(src.join(".cmakedb/passes")).unwrap();
    std::fs::write(
        src.join(".cmakedb/passes/options-inventory.sql"),
        "-- inventory of option() declarations\n\
         SELECT f.path AS file, e.line AS line,\n\
                'declares option ' || json_extract(e.args_json, '$[0]') AS message,\n\
                'note' AS severity\n\
         FROM events e JOIN files f ON f.id = e.file_id\n\
         WHERE e.cmd_lower = 'option' AND f.in_source = 1;\n",
    )
    .unwrap();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let rules: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["rule"].as_str().unwrap())
        .collect();
    assert!(rules.contains(&"options-inventory"), "{rules:?}");
    let msgs: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "options-inventory")
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    assert_eq!(msgs.len(), 4, "{msgs:?}"); // four option()s in the fixture
    assert!(
        msgs.iter().any(|m| m.contains("BUILD_LEGACY_DRIVER")),
        "{msgs:?}"
    );

    // Malformed shape fails loudly, not silently.
    std::fs::write(
        src.join(".cmakedb/passes/bad-shape.sql"),
        "SELECT 1 AS nope;\n",
    )
    .unwrap();
    let out = cmakedb(&src, &["--db", &dbs, "lint"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("must SELECT columns"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn path_filter_scopes_findings_to_subdirectory() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("provenance", tmp.path());
    let db = record(&src, &[]);
    let dbs = db.to_string_lossy();

    // dead modules: Legacy.cmake lives under cmake/ — visible with
    // --path cmake, absent with --path src.
    let text = stdout(&cmakedb(
        &src,
        &["--db", &dbs, "dead", "modules", "--path", "cmake"],
    ));
    assert!(text.contains("Legacy.cmake"), "{text}");
    let text = stdout(&cmakedb(
        &src,
        &["--db", &dbs, "dead", "modules", "--path", "src"],
    ));
    assert!(text.contains("no findings"), "{text}");

    // Component matching: src/net must not match a hypothetical src/nettle;
    // src/http keeps only http findings in lint.
    let out = cmakedb(
        &src,
        &[
            "--db", &dbs, "lint", "--format", "json", "--path", "src/http",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let files: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["primary"]["file"].as_str().unwrap())
        .collect();
    assert!(
        files.iter().all(|f| f.starts_with("src/http/")),
        "{files:?}"
    );

    // Repeatable: two prefixes OR together.
    let text = stdout(&cmakedb(
        &src,
        &[
            "--db", &dbs, "dead", "modules", "--path", "src", "--path", "cmake",
        ],
    ));
    assert!(text.contains("Legacy.cmake"), "{text}");

    // Filtered lint exit code reflects the filtered set: nothing at
    // warning level under src/app -> exit 0.
    let out = cmakedb(
        &src,
        &[
            "--db",
            &dbs,
            "lint",
            "--path",
            "src/app",
            "--fail-on",
            "warning",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
}

#[test]
fn multi_recording_intersection_sharpens_dead_options() {
    let tmp = tempfile::tempdir().unwrap();
    // Two recordings of the same source under different configurations:
    // FEATURE_X is read only when WITH_EXTRA_READS=ON; ALWAYS_DEAD never.
    let a = stage("multiconfig", tmp.path().join("a").as_path());
    let b = stage("multiconfig", tmp.path().join("b").as_path());
    let db_a = record(&a, &[]);
    let db_b = record(&b, &["-DWITH_EXTRA_READS=ON"]);

    // Single-configuration view over-reports: FEATURE_X looks dead in A.
    let text = stdout(&cmakedb(
        &a,
        &["--db", &db_a.to_string_lossy(), "dead", "options"],
    ));
    assert!(text.contains("FEATURE_X"), "{text}");
    assert!(text.contains("ALWAYS_DEAD"), "{text}");

    // Intersection across both recordings drops the false positive and
    // keeps the finding that is dead everywhere.
    let text = stdout(&cmakedb(
        &a,
        &[
            "--db",
            &db_a.to_string_lossy(),
            "dead",
            "options",
            "--also",
            &db_b.to_string_lossy(),
        ],
    ));
    assert!(!text.contains("FEATURE_X"), "{text}");
    assert!(text.contains("ALWAYS_DEAD"), "{text}");
    assert!(text.contains("present in all 2 recordings"), "{text}");

    // lint --also: fail-on applies to the intersection.
    let out = cmakedb(
        &a,
        &[
            "--db",
            &db_a.to_string_lossy(),
            "lint",
            "--also",
            &db_b.to_string_lossy(),
            "--format",
            "json",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let msgs: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    assert!(msgs.iter().all(|m| !m.contains("FEATURE_X")), "{msgs:?}");
    assert!(msgs.iter().any(|m| m.contains("ALWAYS_DEAD")), "{msgs:?}");
}

#[test]
fn option_after_use_detects_declaration_ordering() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("provenance", tmp.path());
    let db = record(&src, &[]);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let hits: Vec<&serde_json::Value> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "option-after-use")
        .collect();
    // Exactly the late-declared option; well-ordered options
    // (BUILD_LEGACY_DRIVER, ENABLE_LOGGING, ...) must not appear.
    assert_eq!(hits.len(), 1, "{hits:?}");
    let f = hits[0];
    assert!(
        f["message"].as_str().unwrap().contains("LATE_DECLARED_OPT"),
        "{f}"
    );
    // Both early-reference shapes (${X} expansion and bare if(X)) are
    // reported as related evidence.
    assert!(f["related"].as_array().unwrap().len() >= 2, "{f}");

    // Pre-seeding the cache makes the early reads observe a value — the
    // rule's suppression case: no finding.
    let b = stage("provenance", tmp.path().join("b").as_path());
    let db_b = record(&b, &["-DLATE_DECLARED_OPT=ON"]);
    let out = cmakedb(
        &b,
        &["--db", &db_b.to_string_lossy(), "lint", "--format", "json"],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["rule"] != "option-after-use"),
        "pre-seeded cache must suppress the finding"
    );
}

#[test]
fn undefined_reads_suggests_typo_fix() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("provenance", tmp.path());
    let db = record(&src, &[]);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let hits: Vec<&serde_json::Value> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "undefined-reads")
        .collect();
    let typo = hits
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("ENABLE_LOGING"))
        .expect("typo finding");
    assert!(
        typo["message"]
            .as_str()
            .unwrap()
            .contains("did you mean `ENABLE_LOGGING`?"),
        "{typo}"
    );
    assert_eq!(typo["severity"], "note");
    // Defined names never appear; ignore-patterns keep CMAKE_* noise out.
    for f in &hits {
        let m = f["message"].as_str().unwrap();
        assert!(!m.contains("${ENABLE_LOGGING}"), "{m}");
        assert!(!m.contains("${CMAKE_"), "{m}");
    }
}

#[test]
fn genex_in_wrong_context_flags_if_not_flowthrough() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("provenance", tmp.path());
    let db = record(&src, &[]);
    let dbs = db.to_string_lossy();

    let out = cmakedb(&src, &["--db", &dbs, "lint", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    let hits: Vec<&serde_json::Value> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "genex-in-wrong-context")
        .collect();
    // The if() comparing a genex as raw text is flagged at warning level.
    let cond = hits
        .iter()
        .find(|f| f["message"].as_str().unwrap().contains("$<CONFIG:Debug>"))
        .expect("if-condition finding");
    assert_eq!(cond["severity"], "warning");
    let msg = cond["message"].as_str().unwrap();
    assert!(msg.contains("`if()` condition"), "{msg}");
    assert_eq!(cond["primary"]["line"], 31, "{cond}");
    // The deliberate genex string-building flow (set(_genex_ok "$<...>")
    // read by genex-aware target_include_directories) is never flagged.
    for f in &hits {
        let m = f["message"].as_str().unwrap();
        assert!(!m.contains("BUILD_INTERFACE"), "{m}");
    }
    assert_eq!(hits.len(), 1, "{hits:?}");
}
