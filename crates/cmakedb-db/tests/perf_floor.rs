//! Ingestion throughput floor (design §6.6), portable stand-in for the
//! criterion benchmarks: a synthetic trace large enough to expose
//! per-event regressions, with a floor far below the reference numbers
//! (Apple Silicon: ~50k events/s on LLVM) so CI-runner variance can't
//! flake it. Run explicitly, in release mode:
//!
//! ```sh
//! cargo test --release -p cmakedb-db --test perf_floor -- --ignored
//! ```

use std::io::Write;

const EVENTS: usize = 150_000;
const FLOOR_EVENTS_PER_SEC: f64 = 10_000.0;

#[test]
#[ignore = "perf gate; run in release via CI"]
fn ingestion_throughput_floor() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    // A real project file so events join AST nodes (the expensive path).
    let mut cml = String::from("cmake_minimum_required(VERSION 3.20)\nproject(perf NONE)\n");
    for i in 0..200 {
        cml.push_str(&format!("set(VAR_{i} value_{i})\n"));
        cml.push_str(&format!("if(VAR_{i})\nendif()\n"));
    }
    let cml_path = src.join("CMakeLists.txt");
    std::fs::write(&cml_path, &cml).unwrap();
    let file = cml_path.to_string_lossy();

    let trace_path = tmp.path().join("trace.jsonl");
    {
        let mut w = std::io::BufWriter::new(std::fs::File::create(&trace_path).unwrap());
        writeln!(w, r#"{{"version":{{"major":1,"minor":2}}}}"#).unwrap();
        let mut t = 1.0f64;
        for i in 0..EVENTS {
            // Pair each if(VAR_x) read with the set(VAR_x) just before it.
            let vi = (i / 2) % 200;
            // Alternate writes and condition reads on real AST lines.
            let (line, cmd, args) = if i % 2 == 0 {
                (3 + vi * 3, "set", format!(r#"["VAR_{vi}","value_{i}"]"#))
            } else {
                (4 + vi * 3, "if", format!(r#"["VAR_{vi}"]"#))
            };
            t += 1e-6;
            writeln!(
                w,
                r#"{{"args":{args},"cmd":"{cmd}","file":"{file}","line":{line},"time":{t},"frame":1,"global_frame":1}}"#
            )
            .unwrap();
        }
    }

    let db_path = tmp.path().join("perf.db");
    let mut db = cmakedb_db::Db::create(&db_path).unwrap();
    let start = std::time::Instant::now();
    let stats = cmakedb_db::ingest::ingest(
        &mut db,
        &cmakedb_db::ingest::IngestInput {
            trace_path,
            source_dir: src.clone(),
            build_dir: tmp.path().join("build"),
            reply_dir: None,
            snapshots_path: None,
            preseeded_cache: vec![],
            stderr_path: None,
            meta: vec![],
        },
    )
    .expect("ingest");
    let secs = start.elapsed().as_secs_f64();
    let rate = stats.events as f64 / secs;
    eprintln!(
        "ingested {} events in {secs:.2}s = {:.0} events/s (floor {FLOOR_EVENTS_PER_SEC})",
        stats.events, rate
    );
    assert_eq!(stats.events as usize, EVENTS);
    assert!(stats.var_reads > 0 && stats.var_writes > 0);
    assert!(
        rate >= FLOOR_EVENTS_PER_SEC,
        "ingestion regressed below the floor: {rate:.0} events/s"
    );
}
