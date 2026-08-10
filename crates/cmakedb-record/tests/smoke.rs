//! End-to-end smoke test: record a real cmake configure of a tiny C project
//! and sanity-check the resulting database. Requires `cmake` on PATH.

use cmakedb_record::{record, RecordOptions};
use std::path::Path;

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn record_and_ingest_c_project() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path();

    write(
        &src.join("CMakeLists.txt"),
        r#"cmake_minimum_required(VERSION 3.20)
project(smoke C)
option(UNUSED_OPT "never read" OFF)
option(USED_OPT "read below" ON)
function(my_helper out)
  set(${out} "computed" PARENT_SCOPE)
endfunction()
macro(leaky)
  set(mac_tmp "oops")
endmacro()
if(USED_OPT)
  set(FEATURE 1)
endif()
my_helper(HELPED)
leaky()
add_subdirectory(libs/util)
add_subdirectory(app)
"#,
    );
    write(
        &src.join("libs/util/CMakeLists.txt"),
        r#"add_library(util STATIC util.c)
target_include_directories(util PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)
add_library(smoke::util ALIAS util)
"#,
    );
    write(
        &src.join("libs/util/util.c"),
        "int util_fn(void) { return 42; }\n",
    );
    write(
        &src.join("libs/util/include/util.h"),
        "int util_fn(void);\n",
    );
    write(
        &src.join("app/CMakeLists.txt"),
        r#"add_executable(app main.c)
target_link_libraries(app PRIVATE smoke::util)
"#,
    );
    write(
        &src.join("app/main.c"),
        "int util_fn(void);\nint main(void) { return util_fn(); }\n",
    );
    // A module that is never included: dead-modules fodder.
    write(&src.join("cmake/Unused.cmake"), "set(NEVER_RUN 1)\n");

    let opts = RecordOptions {
        source_dir: Some(src.to_path_buf()),
        build_dir: Some(src.join("build")),
        ..Default::default()
    };
    let result = record(&opts).expect("record should succeed");
    assert!(result.db_path.exists());
    let stats = &result.stats;
    assert!(
        stats.events > 50,
        "expected many events, got {}",
        stats.events
    );
    assert!(stats.joined_events > 0);
    assert!(stats.var_writes > 0);
    assert!(stats.var_reads > 0);

    let db = cmakedb_db::Db::open(&result.db_path).unwrap();
    let count = |sql: &str| -> i64 { db.conn.query_row(sql, [], |r| r.get(0)).unwrap() };

    // Targets from both trace and File API, alias resolved.
    assert!(count("SELECT count(*) FROM targets WHERE name='util' AND in_file_api=1") == 1);
    assert!(count("SELECT count(*) FROM targets WHERE name='app'") == 1);
    assert!(
        count("SELECT count(*) FROM targets WHERE name='smoke::util' AND alias_of='util'") == 1
    );

    // The app->util edge, alias-resolved, PRIVATE, with an origin event.
    let (dst, vis): (String, String) = db
        .conn
        .query_row(
            "SELECT dst, visibility FROM tgt_edges e
             JOIN targets s ON s.id = e.src_target WHERE s.name='app'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(dst, "smoke::util");
    assert_eq!(vis, "PRIVATE");
    assert!(
        count(
            "SELECT count(*) FROM tgt_edges e JOIN targets s ON s.id=e.src_target
             JOIN targets d ON d.id=e.dst_target
             WHERE s.name='app' AND d.name='util' AND e.origin_event IS NOT NULL"
        ) == 1
    );

    // util's PUBLIC include dir as written (trace) and as resolved (fileapi).
    assert!(
        count(
            "SELECT count(*) FROM usage_reqs r JOIN targets t ON t.id=r.target_id
         WHERE t.name='util' AND r.kind='include' AND r.source='trace' AND r.visibility='PUBLIC'"
        ) >= 1
    );

    // Dataflow: USED_OPT was read (via if()), UNUSED_OPT was not.
    assert!(count(
        "SELECT count(*) FROM var_reads WHERE name='USED_OPT' AND resolved_write_id IS NOT NULL"
    ) >= 1);
    assert_eq!(
        count("SELECT count(*) FROM var_reads WHERE name='UNUSED_OPT'"),
        0
    );

    // Function scope: PARENT_SCOPE write of HELPED recorded against the
    // caller (root) scope; ${out} read resolved to the synthesized param.
    assert!(
        count(
            "SELECT count(*) FROM var_writes w JOIN scopes s ON s.id=w.scope_id
         WHERE w.name='HELPED' AND w.write_kind='parent_scope' AND s.kind='root'"
        ) >= 1
    );
    assert!(
        count(
            "SELECT count(*) FROM var_reads r JOIN var_writes w ON w.id=r.resolved_write_id
         WHERE r.name='out' AND w.write_kind='synthetic'"
        ) >= 1
    );

    // Macro transparency: mac_tmp's write landed in the caller's (root) scope.
    assert!(
        count(
            "SELECT count(*) FROM var_writes w JOIN scopes s ON s.id=w.scope_id
         WHERE w.name='mac_tmp' AND s.kind='root'"
        ) >= 1
    );
    // ...but the event itself sits in a macro scope.
    assert!(
        count(
            "SELECT count(*) FROM var_writes w JOIN events e ON e.id=w.event_id
         JOIN scopes es ON es.id=e.scope_id
         WHERE w.name='mac_tmp' AND es.kind='macro'"
        ) >= 1
    );

    // Directory scopes for the two subdirectories.
    assert!(count("SELECT count(*) FROM scopes WHERE kind='directory'") >= 2);

    // The unused module was indexed (for dead-modules) with zero events.
    assert!(
        count(
            "SELECT count(*) FROM files f WHERE f.path LIKE '%Unused.cmake' AND f.in_source=1
         AND NOT EXISTS (SELECT 1 FROM events e WHERE e.file_id=f.id)"
        ) == 1
    );

    // TUs recorded from the File API.
    assert!(count("SELECT count(*) FROM tus") >= 2);
}

/// §4.6 step 6 acceptance: a patch that changes the build graph is
/// auto-reverted with the differences reported.
#[test]
fn verification_loop_reverts_graph_breaking_patch() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path();
    write(
        &src.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.20)\nproject(v C)\nadd_library(keep STATIC a.c)\nadd_library(victim STATIC b.c)\n",
    );
    write(&src.join("a.c"), "int a(void) { return 1; }\n");
    write(&src.join("b.c"), "int b(void) { return 2; }\n");

    let opts = RecordOptions {
        source_dir: Some(src.to_path_buf()),
        build_dir: Some(src.join("build")),
        ..Default::default()
    };
    let result = record(&opts).expect("record");
    let original = std::fs::read_to_string(src.join("CMakeLists.txt")).unwrap();

    // A "codemod" that deletes the victim target's definition line.
    let db = cmakedb_db::Db::open(&result.db_path).unwrap();
    let (path, hash, byte_start, byte_end): (String, String, i64, i64) = db
        .conn
        .query_row(
            "SELECT f.path, f.content_hash, n.byte_start, n.byte_end
             FROM events e JOIN ast_nodes n ON n.id = e.node_id
             JOIN files f ON f.id = n.file_id
             WHERE e.cmd_lower='add_library'
               AND e.args_json LIKE '%victim%' AND f.in_source=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    drop(db);
    let bad = cmakedb_patch::Patch {
        title: "break the graph".into(),
        edits: vec![cmakedb_patch::Edit {
            file: path,
            content_hash: hash,
            byte_start: byte_start as usize,
            byte_end: byte_end as usize + 1, // include the newline
            replacement: String::new(),
        }],
    };

    let outcome =
        cmakedb_record::verify::apply_and_verify(&result.db_path, &[bad]).expect("verify runs");
    assert!(
        outcome.reverted,
        "graph-breaking patch must be reverted: {outcome:?}"
    );
    assert!(
        outcome.problems.iter().any(|p| p.contains("victim")),
        "problems should name the lost target: {:?}",
        outcome.problems
    );
    // Sources rolled back byte-identically; old recording intact.
    assert_eq!(
        std::fs::read_to_string(src.join("CMakeLists.txt")).unwrap(),
        original
    );
    assert!(result.db_path.exists());
    assert!(!result.db_path.with_extension("verify.db").exists());
}

/// §3.2 channel 2 + §6.2: scope snapshots captured and the replay
/// cross-validated against CMake's own dumped tables — divergence would
/// fail this record() call outright.
#[test]
fn scope_snapshot_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path();
    write(
        &src.join("CMakeLists.txt"),
        r#"cmake_minimum_required(VERSION 3.20)
project(snap NONE)
set(PLAIN hello)
set(LISTY a b c)
set(QUOTED_EMPTY "" tail)
set(EMPTY_EXPANSION ${UNDEFINED_THING} kept)
function(fn out)
  set(${out} inner PARENT_SCOPE)
endfunction()
fn(FROM_FN)
macro(mc)
  set(from_macro 1)
endmacro()
mc()
add_subdirectory(sub)
"#,
    );
    write(&src.join("sub/CMakeLists.txt"), "set(SUB_LOCAL x)\n");

    let result = record(&RecordOptions {
        source_dir: Some(src.to_path_buf()),
        build_dir: Some(src.join("build")),
        capture_scopes: true,
        ..Default::default()
    })
    .expect("record with snapshot validation must succeed");

    assert!(result.stats.snapshots_validated >= 2, "{:?}", result.stats);
    assert!(
        result.stats.snapshot_vars_checked > 20,
        "{:?}",
        result.stats
    );

    // Spot-check the §6.2-sensitive value semantics the validator guards:
    // quoted empties survive, unquoted empty expansions vanish.
    let db = cmakedb_db::Db::open(&result.db_path).unwrap();
    let value = |name: &str| -> String {
        db.conn
            .query_row(
                "SELECT value FROM var_writes WHERE name = ?1 ORDER BY id DESC LIMIT 1",
                [name],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(value("LISTY"), "a;b;c");
    assert_eq!(value("QUOTED_EMPTY"), ";tail");
    assert_eq!(value("EMPTY_EXPANSION"), "kept");
}
