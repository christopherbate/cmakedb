//! E2e for profile v2: `--flamegraph` folded-stack export and
//! `--compare` recording comparison. Assertions are structural (folded
//! format shape, value-sum bounds, alignment/marking, valid JSON) —
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

/// Record `src` into an explicit build dir + db path (lets one staged
/// tree carry several recordings).
fn record_to(src: &Path, build: &str, db_rel: &str) -> PathBuf {
    let db = src.join(db_rel);
    let out = Command::new(env!("CARGO_BIN_EXE_cmakedb"))
        .args([
            "record",
            "--source-dir",
            &src.to_string_lossy(),
            "--build-dir",
            &src.join(build).to_string_lossy(),
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

fn json(o: &Output) -> serde_json::Value {
    assert!(
        o.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_str(&stdout(o)).expect("valid JSON")
}

#[test]
fn flamegraph_emits_folded_stacks() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("profiledemo", tmp.path());
    let db = record_to(&src, "build", ".cmakedb/trace.db");
    let dbs = db.to_string_lossy().into_owned();

    let out = cmakedb(&src, &["--db", &dbs, "profile", "--flamegraph"]);
    let folded = stdout(&out);
    assert!(
        out.status.success(),
        "{folded}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Every line is `frame(;frame)* <integer>` and every stack starts at
    // the root frame.
    let mut sum: i64 = 0;
    let mut build_label_stack = None;
    for line in folded.lines() {
        let (stack, value) = line.rsplit_once(' ').unwrap_or_else(|| {
            panic!("line without value separator: {line:?}");
        });
        let value: i64 = value
            .parse()
            .unwrap_or_else(|_| panic!("non-integer value in {line:?}"));
        assert!(value > 0, "zero/negative value emitted: {line:?}");
        sum += value;
        let frames: Vec<&str> = stack.split(';').collect();
        assert!(
            !frames.is_empty() && frames.iter().all(|f| !f.is_empty()),
            "{line:?}"
        );
        assert_eq!(frames[0], "root", "stack not rooted: {line:?}");
        for f in &frames {
            assert!(
                !f.contains(' '),
                "unsanitized space inside frame breaks the format: {line:?}"
            );
        }
        if frames.contains(&"function:build_label") {
            build_label_stack = Some(stack.to_string());
        }
    }
    let stack = build_label_stack.expect("no function:build_label frame in output");
    assert!(stack.starts_with("root;"), "{stack}");

    // Value-sum property: exclusive times must telescope to (at most) the
    // total configure span, and cover the bulk of it. Bounds, not
    // equality: rounding and clamping are allowed a little slack.
    let v = json(&cmakedb(
        &src,
        &["--db", &dbs, "profile", "--format", "json"],
    ));
    let span = v["total_span_us"].as_i64().unwrap();
    assert!(span > 0);
    assert!(
        sum <= span + 1_000,
        "folded values sum {sum} exceeds configure span {span}"
    );
    assert!(
        sum * 10 >= span * 8,
        "folded values sum {sum} covers <80% of configure span {span}"
    );

    // Folded stacks are a text format; --format json must fail cleanly.
    let out = cmakedb(
        &src,
        &["--db", &dbs, "profile", "--flamegraph", "--format", "json"],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("folded stacks"),
        "unhelpful error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn compare_aligns_recordings_of_the_same_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let src = stage("profiledemo", tmp.path());
    let db1 = record_to(&src, "build", ".cmakedb/a.db");
    let db2 = record_to(&src, "build2", ".cmakedb/b.db");
    let (d1, d2) = (
        db1.to_string_lossy().into_owned(),
        db2.to_string_lossy().into_owned(),
    );

    // Self-compare (same file on both sides): exact zero everywhere.
    let v = json(&cmakedb(
        &src,
        &["--db", &d1, "profile", "--compare", &d1, "--format", "json"],
    ));
    assert_eq!(v["schema_version"], 1, "{v}");
    assert_eq!(v["span_delta_us"].as_i64().unwrap(), 0, "{v}");
    let rows = v["scopes"].as_array().unwrap();
    assert!(!rows.is_empty());
    for r in rows {
        assert_eq!(r["delta_us"].as_i64().unwrap(), 0, "{r}");
        assert_eq!(r["status"], "common", "{r}");
        assert_eq!(r["primary_calls"], r["other_calls"], "{r}");
    }

    // Two independent recordings of the same tree: every scope aligns by
    // name (same call counts, status common); deltas are small but
    // nonzero timing noise — tolerated, not asserted.
    let v = json(&cmakedb(
        &src,
        &["--db", &d1, "profile", "--compare", &d2, "--format", "json"],
    ));
    assert_eq!(
        v["primary_span_us"].as_i64().unwrap() - v["other_span_us"].as_i64().unwrap(),
        v["span_delta_us"].as_i64().unwrap()
    );
    let rows = v["scopes"].as_array().unwrap();
    let bl = rows
        .iter()
        .find(|r| r["kind"] == "function" && r["name"] == "build_label")
        .unwrap_or_else(|| panic!("no build_label row: {v}"));
    assert_eq!(bl["status"], "common", "{bl}");
    assert_eq!(bl["primary_calls"].as_i64().unwrap(), 11, "{bl}");
    assert_eq!(bl["other_calls"].as_i64().unwrap(), 11, "{bl}");
    for r in rows {
        assert_eq!(r["status"], "common", "misaligned scope: {r}");
    }

    // --top truncates; text render has the summary header.
    let v = json(&cmakedb(
        &src,
        &[
            "--db",
            &d1,
            "profile",
            "--compare",
            &d2,
            "--top",
            "3",
            "--format",
            "json",
        ],
    ));
    assert!(v["scopes"].as_array().unwrap().len() <= 3, "{v}");
    let out = cmakedb(&src, &["--db", &d1, "profile", "--compare", &d2]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("profile compare:"), "{text}");
    assert!(text.contains("span delta"), "{text}");
    assert!(
        text.contains("largest inclusive-time deltas by scope:"),
        "{text}"
    );
}

#[test]
fn compare_marks_appeared_and_disappeared_scopes() {
    let tmp = tempfile::tempdir().unwrap();
    let demo = stage("profiledemo", tmp.path());
    let prov = stage("provenance", tmp.path());
    let demo_db = record_to(&demo, "build", ".cmakedb/trace.db");
    let prov_db = record_to(&prov, "build", ".cmakedb/trace.db");
    let (dd, dp) = (
        demo_db.to_string_lossy().into_owned(),
        prov_db.to_string_lossy().into_owned(),
    );

    let v = json(&cmakedb(
        &demo,
        &[
            "--db",
            &dd,
            "profile",
            "--compare",
            &dp,
            "--top",
            "500",
            "--format",
            "json",
        ],
    ));
    let rows = v["scopes"].as_array().unwrap();
    let bl = rows
        .iter()
        .find(|r| r["kind"] == "function" && r["name"] == "build_label")
        .unwrap_or_else(|| panic!("no build_label row: {v}"));
    assert_eq!(bl["status"], "appeared", "{bl}");
    assert_eq!(bl["other_total_us"].as_i64().unwrap(), 0, "{bl}");
    assert_eq!(bl["delta_us"], bl["primary_total_us"], "{bl}");
    let scf = rows
        .iter()
        .find(|r| r["kind"] == "macro" && r["name"] == "set_common_flags")
        .unwrap_or_else(|| panic!("no set_common_flags row: {v}"));
    assert_eq!(scf["status"], "disappeared", "{scf}");
    assert_eq!(scf["primary_total_us"].as_i64().unwrap(), 0, "{scf}");
    assert!(scf["delta_us"].as_i64().unwrap() <= 0, "{scf}");

    let out = cmakedb(
        &demo,
        &["--db", &dd, "profile", "--compare", &dp, "--top", "500"],
    );
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("appeared:"), "{text}");
    assert!(text.contains("disappeared:"), "{text}");
}
