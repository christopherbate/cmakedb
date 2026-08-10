//! LSP query-helper tests against a real recording (§2.4). The wire layer
//! is deliberately thin; these cover everything it delegates to.

use cmakedb_record::{record, RecordOptions};
use std::path::Path;

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn recorded_project() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path();
    write(
        &src.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.20)\nproject(lsp C)\nset(GREETING hello)\nmessage(STATUS \"${GREETING}\")\nfunction(my_fn)\nendfunction()\nmy_fn()\nadd_library(mylib STATIC a.c)\ntarget_link_libraries(mylib PRIVATE m)\noption(DEAD_OPT \"unused\" OFF)\n",
    );
    write(&src.join("a.c"), "int a(void) { return 1; }\n");
    let result = record(&RecordOptions {
        source_dir: Some(src.to_path_buf()),
        build_dir: Some(src.join("build")),
        ..Default::default()
    })
    .expect("record");
    let top_native = src.join("CMakeLists.txt");
    (tmp, result.db_path, top_native)
}

#[test]
fn word_extraction() {
    let text = "set(GREETING hello)\ntarget_link_libraries(app PRIVATE ns::lib)\n";
    assert_eq!(cmakedb_lsp::word_at(text, 0, 6), Some("GREETING".into()));
    assert_eq!(cmakedb_lsp::word_at(text, 1, 36), Some("ns::lib".into()));
    // On punctuation just after a word: snaps back to the adjacent word.
    assert_eq!(cmakedb_lsp::word_at(text, 0, 3), Some("set".into()));
}

#[test]
fn hover_definition_diagnostics_and_stale() {
    let (_tmp, db_path, top_native) = recorded_project();
    let db = cmakedb_db::Db::open(&db_path).unwrap();
    // Resolve the editor-native path to the recording's spelling exactly
    // as the wire layer does (on Windows cmake records C:/... while the
    // native path uses backslashes — this lookup is the production path).
    let top = cmakedb_lsp::db_path_for(&db, &top_native);

    // Hover a variable read: precise value + history.
    let h = cmakedb_lsp::hover(&db, &top, 4, "GREETING")
        .unwrap()
        .unwrap();
    assert!(h.contains("hello"), "{h}");
    assert!(h.contains("write history"), "{h}");

    // Hover a target: type + links.
    let h = cmakedb_lsp::hover(&db, &top, 8, "mylib").unwrap().unwrap();
    assert!(h.contains("target mylib"), "{h}");
    assert!(h.contains("STATIC_LIBRARY"), "{h}");
    assert!(h.contains("PRIVATE m"), "{h}");

    // Definition: read -> dominating write (line 3); function -> def site;
    // target -> add_library line.
    let d = cmakedb_lsp::definition(&db, &top, 4, "GREETING")
        .unwrap()
        .unwrap();
    assert_eq!(d.1, 3, "{d:?}");
    let d = cmakedb_lsp::definition(&db, &top, 7, "my_fn")
        .unwrap()
        .unwrap();
    assert_eq!(d.1, 5, "{d:?}");
    let d = cmakedb_lsp::definition(&db, &top, 9, "mylib")
        .unwrap()
        .unwrap();
    assert_eq!(d.1, 8, "{d:?}");

    // Diagnostics grouped by absolute path include the dead option.
    let diags = cmakedb_lsp::diagnostics(&db).unwrap();
    let file_diags = diags.get(&top).expect("diagnostics for top CMakeLists");
    assert!(
        file_diags
            .iter()
            .any(|d| d.rule == "dead-options" && d.message.contains("DEAD_OPT")),
        "{file_diags:?}"
    );

    // Staleness tracks content changes.
    let recorded_text = std::fs::read_to_string(&top_native).unwrap();
    assert!(!cmakedb_lsp::is_stale(&db, &top, &recorded_text));
    assert!(cmakedb_lsp::is_stale(
        &db,
        &top,
        "# edited since recording\n"
    ));
}

#[test]
fn code_actions_and_tree_data() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path();
    write(
        &src.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.20)\nproject(ca C)\ninclude_directories(${CMAKE_CURRENT_SOURCE_DIR}/inc)\nadd_library(lib1 STATIC a.c)\nadd_library(lib2 STATIC b.c)\ntarget_link_libraries(lib2 PRIVATE lib1)\n",
    );
    write(&src.join("inc/h.h"), "int h;\n");
    write(&src.join("a.c"), "int a(void) { return 1; }\n");
    write(&src.join("b.c"), "int b(void) { return 2; }\n");
    let result = record(&RecordOptions {
        source_dir: Some(src.to_path_buf()),
        build_dir: Some(src.join("build")),
        ..Default::default()
    })
    .expect("record");
    let db = cmakedb_db::Db::open(&result.db_path).unwrap();
    let top = src.join("CMakeLists.txt").to_string_lossy().to_string();

    // Code action on the include_directories line: full modernize patch as
    // position-based edits; applying them matches the byte-anchored patch.
    let current =
        std::collections::HashMap::from([(top.clone(), std::fs::read_to_string(&top).unwrap())]);
    let actions = cmakedb_lsp::code_actions(&db, &top, 3, &current).unwrap();
    assert_eq!(
        actions.len(),
        1,
        "actions: {actions:?}\nplan (incl. skip notes): {:#?}\ntop: {top}",
        cmakedb_passes::modernize::plan(&db, &[]).map(|fs| fs
            .iter()
            .map(|f| format!(
                "{} @{}:{} fix={}",
                f.message,
                f.primary.file,
                f.primary.line,
                f.fix.is_some()
            ))
            .collect::<Vec<_>>())
    );
    assert!(
        actions[0].edits.len() >= 3,
        "delete + 2 insertions: {actions:?}"
    );

    // Stale buffer -> no actions (offsets must never be misapplied).
    let stale = std::collections::HashMap::from([(top.clone(), "# changed\n".to_string())]);
    assert!(cmakedb_lsp::code_actions(&db, &top, 3, &stale)
        .unwrap()
        .is_empty());

    // Tree data for the provenance view.
    let targets = cmakedb_lsp::targets_list(&db).unwrap();
    assert!(
        targets
            .iter()
            .any(|(n, t)| n == "lib2" && t == "STATIC_LIBRARY"),
        "{targets:?}"
    );
    let edges = cmakedb_lsp::edges_of(&db, "lib2").unwrap();
    assert_eq!(edges.len(), 1, "{edges:?}");
    assert_eq!(edges[0].0, "lib1");
    assert_eq!(edges[0].1, "PRIVATE");
    assert!(
        edges[0]
            .2
            .as_deref()
            .unwrap_or("")
            .contains("CMakeLists.txt:6"),
        "{edges:?}"
    );
    assert!(edges[0].3, "lib1 is a real target");
}

#[test]
fn byte_position_mapping() {
    let text = "ab\ncdé f\ng";
    assert_eq!(cmakedb_lsp::byte_to_position(text, 0), (0, 0));
    assert_eq!(cmakedb_lsp::byte_to_position(text, 3), (1, 0)); // start of line 2
                                                                // é is 2 bytes in UTF-8, 1 UTF-16 unit: byte 7 is after "cdé "
    assert_eq!(cmakedb_lsp::byte_to_position(text, 8), (1, 4));
    assert_eq!(cmakedb_lsp::byte_to_position(text, text.len()), (2, 1));
}
