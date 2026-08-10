//! Robustness properties (design §6.5), runnable in the normal test suite
//! on any toolchain: a seeded PRNG stands in for a fuzzer here, and the
//! same properties are wired to real fuzzing under `fuzz/` (nightly).

use std::path::PathBuf;

/// Tiny deterministic PRNG (xorshift64*) — no dev-dependency, same
/// sequence on every platform, so failures reproduce from the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const CMAKE_FRAGMENTS: &[&str] = &[
    "set(",
    ")",
    "\n",
    "${",
    "}",
    "\"",
    "[[",
    "]]",
    "[=[",
    "]=]",
    "if(",
    "endif()",
    "function(",
    "endfunction()",
    "macro(",
    "endmacro()",
    "foreach(",
    "endforeach()",
    "#",
    "\\",
    ";",
    " ",
    "\t",
    "$<",
    ">",
    "A",
    "VAR_NAME",
    "1.2.3",
    "ENV{",
    "()",
    "block()",
    "endblock()",
    "else()",
    "\r\n",
    "é",
    "🎯",
];

fn mutate(rng: &mut Rng, base: &str) -> String {
    let mut s = base.to_string();
    for _ in 0..rng.below(8) {
        match rng.below(3) {
            0 => {
                // insert a fragment at a char boundary
                let frag = CMAKE_FRAGMENTS[rng.below(CMAKE_FRAGMENTS.len())];
                let pos = floor_char_boundary(&s, rng.below(s.len() + 1));
                s.insert_str(pos, frag);
            }
            1 => {
                // delete a random slice
                if !s.is_empty() {
                    let a = floor_char_boundary(&s, rng.below(s.len()));
                    let b = floor_char_boundary(&s, (a + 1 + rng.below(16)).min(s.len()));
                    s.replace_range(a..b, "");
                }
            }
            _ => {
                // duplicate a random slice
                if !s.is_empty() {
                    let a = floor_char_boundary(&s, rng.below(s.len()));
                    let b = floor_char_boundary(&s, (a + 1 + rng.below(32)).min(s.len()));
                    let dup = s[a..b].to_string();
                    let pos = floor_char_boundary(&s, rng.below(s.len() + 1));
                    s.insert_str(pos, &dup);
                }
            }
        }
    }
    s
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// §6.5 parser invariant: never panic, and whatever parses reprints
/// byte-identically (error-tolerant parses included).
#[test]
fn parser_never_panics_and_reprints_losslessly() {
    let seeds = [
        "set(A 1)\nif(FOO)\n  target_link_libraries(a PRIVATE b)\nendif()\n",
        "function(f x)\n  set(${x} \"v\" PARENT_SCOPE)\nendfunction()\n",
        "set(DOC [=[bracket ${X} ]=])\nset(ML \"a\nb\")\n",
        "",
    ];
    let mut rng = Rng(0x5EED_CAFE_F00D_0001);
    for i in 0..2000 {
        let base = seeds[i % seeds.len()];
        let input = mutate(&mut rng, base);
        let parsed = cmakedb_syntax::parse_source(PathBuf::from("fuzz.cmake"), input.clone())
            .unwrap_or_else(|e| {
                panic!("parse errored (not tolerated) on iter {i}: {e}\n{input:?}")
            });
        let printed = cmakedb_syntax::reprint(&parsed)
            .unwrap_or_else(|e| panic!("reprint failed on iter {i}: {e}\n{input:?}"));
        assert_eq!(printed, input, "lossless reprint violated on iter {i}");
    }
}

/// §6.5 ingestion invariant: malformed/truncated traces produce a clean
/// error and an empty (rolled-back) database — never a partial commit.
#[test]
fn malformed_traces_fail_cleanly_without_partial_db() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("CMakeLists.txt"), "set(A 1)\n").unwrap();

    let good_line = r#"{"args":["A","1"],"cmd":"set","file":"FILE","line":1,"time":1.0,"frame":1,"global_frame":1}"#
        .replace("FILE", &src.join("CMakeLists.txt").to_string_lossy());

    let cases: Vec<(&str, String)> = vec![
        ("empty file", String::new()),
        ("no version header", format!("{good_line}\n")),
        (
            "bad version",
            "{\"version\":{\"major\":9,\"minor\":0}}\n".to_string(),
        ),
        (
            "truncated json",
            format!(
                "{{\"version\":{{\"major\":1,\"minor\":2}}}}\n{}",
                &good_line[..good_line.len() / 2]
            ),
        ),
        (
            "garbage line",
            format!("{{\"version\":{{\"major\":1,\"minor\":2}}}}\n{good_line}\nnot json at all\n"),
        ),
        (
            // Pre-global_frame cmake binaries (the field is absent):
            // must be a loud unsupported-version error, never a silent
            // flat-scope replay.
            "no global_frame",
            format!(
                "{{\"version\":{{\"major\":1,\"minor\":1}}}}\n{}\n",
                good_line.replace(",\"global_frame\":1", "")
            ),
        ),
        (
            "binary noise",
            "{\"version\":{\"major\":1,\"minor\":2}}\n\x00\x01\x02\u{fffd}\n".to_string(),
        ),
    ];

    for (name, trace) in cases {
        let trace_path = tmp.path().join("trace.jsonl");
        std::fs::write(&trace_path, trace).unwrap();
        let db_path = tmp.path().join("t.db");
        let _ = std::fs::remove_file(&db_path);
        let mut db = cmakedb_db::Db::create(&db_path).unwrap();
        let err = cmakedb_db::ingest::ingest(
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
        );
        assert!(err.is_err(), "case '{name}' must fail");
        // Rolled back: no partial rows survive.
        for table in ["events", "files", "var_writes", "targets"] {
            let n: i64 = db
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "case '{name}': partial rows in {table}");
        }
    }
}

/// Snapshot sidecar parsing tolerates arbitrary garbage without panicking.
#[test]
fn snapshot_parsing_never_panics() {
    let tmp = tempfile::tempdir().unwrap();
    let mut rng = Rng(0xBAD5_EED5_0DA5_0002);
    let seeds = [
        "SNAPSHOT\tproject-before\nVAR\t68656c6c6f\n",
        "SNAPSHOT\t\nX\tzznothex\n",
        "no header at all\t\t\t\n",
    ];
    for i in 0..500 {
        let input = mutate(&mut rng, seeds[i % seeds.len()]);
        let p = tmp.path().join("s.tsv");
        std::fs::write(&p, &input).unwrap();
        // Must not panic; any Ok/Err outcome is acceptable.
        let _ = cmakedb_db::ingest::parse_snapshots(&p);
    }
}
