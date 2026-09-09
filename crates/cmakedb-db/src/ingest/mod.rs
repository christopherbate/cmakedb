//! Ingestion: joins the execution trace, source ASTs, and the File API
//! reply into the semantic database (design §3.3).
//!
//! Two passes over the trace file:
//!  1. collect the set of executed files (plus all CMake files under the
//!     source root, so never-executed modules are represented);
//!  2. stream events in execution order through the scope/dataflow replay
//!     (§4.2), joining each event to its AST node.
//!
//! Then the File API reply is ingested and joined back to events via
//! backtraces, and target edges collected from the trace are resolved.

mod dataflow;
mod deps;
mod fileapi;
mod trace;
mod warnings;

pub use dataflow::parse_snapshots;
pub use deps::ingest_ninja_deps;
pub use trace::scan_trace_bytes;
pub use warnings::{parse_stderr, Diagnostic};

use anyhow::{Context, Result};
use rusqlite::params;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::Db;
use cmakedb_syntax::ParsedFile;

#[derive(Debug, Clone)]
pub struct IngestInput {
    pub trace_path: PathBuf,
    pub source_dir: PathBuf,
    pub build_dir: PathBuf,
    /// `<build>/.cmake/api/v1/reply`, if the File API produced one.
    pub reply_dir: Option<PathBuf>,
    /// Scope-snapshot sidecar (§3.2 channel 2); ingestion cross-validates
    /// the replay against it and fails loudly on divergence (§6.2).
    pub snapshots_path: Option<PathBuf>,
    /// Cache entries the caller pre-seeded (-D args, preset
    /// cacheVariables). They emit no trace events, so the replay seeds
    /// them as initial cache writes. The recorder is the source of truth:
    /// the final cache can't identify them once option()/set(CACHE)
    /// re-declares an entry (its HELPSTRING gets overwritten).
    pub preseeded_cache: Vec<(String, String)>,
    /// Captured configure stderr (recorder sidecar); CMake's own
    /// warning/error blocks are parsed into `configure_diagnostics`.
    pub stderr_path: Option<PathBuf>,
    /// Extra metadata to store (cmake_version, preset, argv, ...).
    pub meta: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub struct IngestStats {
    pub events: u64,
    pub joined_events: u64,
    pub files: u64,
    pub source_files: u64,
    pub var_writes: u64,
    pub var_reads: u64,
    pub targets: u64,
    pub edges: u64,
    pub snapshots_validated: u64,
    pub snapshot_vars_checked: u64,
}

impl IngestStats {
    pub fn summary(&self) -> String {
        format!(
            "{} command evaluations across {} files ({} project files); \
             {} variable writes, {} reads; {} targets, {} link edges",
            self.events,
            self.files,
            self.source_files,
            self.var_writes,
            self.var_reads,
            self.targets,
            self.edges
        )
    }
}

/// In-memory context shared by ingestion phases.
pub(crate) struct FileTable {
    /// canonical path -> (file_id, parsed AST if available)
    pub by_path: HashMap<String, (i64, Option<ParsedFile>)>,
}

impl FileTable {
    pub fn file_id(&self, path: &str) -> Option<i64> {
        self.by_path.get(path).map(|(id, _)| *id)
    }
}

pub fn ingest(db: &mut Db, input: &IngestInput) -> Result<IngestStats> {
    let mut stats = IngestStats::default();

    // Pass 1: which files executed?
    let executed_files = trace::collect_files(&input.trace_path)
        .with_context(|| format!("scanning trace {}", input.trace_path.display()))?;

    // All CMake sources under the source root (for dead-module analysis),
    // excluding the build directory if nested.
    let source_files = find_cmake_sources(&input.source_dir, &input.build_dir);

    // Bulk-load without index maintenance; indexes are rebuilt at the end
    // (§6.6 throughput).
    {
        let mut stmt = db
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'ix_%'")?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        for n in names {
            db.conn
                .execute_batch(&format!("DROP INDEX IF EXISTS {n}"))?;
        }
    }

    let tx = db.conn.transaction()?;

    // Parse + insert files, AST nodes, commands, static var refs.
    let mut files = FileTable {
        by_path: HashMap::new(),
    };
    {
        let mut all: Vec<(String, bool)> = Vec::new();
        for f in &executed_files {
            all.push((f.clone(), false));
        }
        for f in &source_files {
            let s = crate::cmake_path_spelling(&f.to_string_lossy());
            if !executed_files.contains(&s) {
                all.push((s, true));
            }
        }
        let source_dir_str = crate::cmake_path_spelling(&input.source_dir.to_string_lossy());
        let build_dir_str = crate::cmake_path_spelling(&input.build_dir.to_string_lossy());
        let src_prefix = format!("{}/", source_dir_str.trim_end_matches('/'));
        let build_prefix = format!("{}/", build_dir_str.trim_end_matches('/'));

        let mut ins_file = tx.prepare(
            "INSERT INTO files(path, content_hash, in_source, has_errors, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut ins_node = tx.prepare(
            "INSERT INTO ast_nodes(file_id, kind, byte_start, byte_end, line, col, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let mut ins_cmd = tx.prepare(
            "INSERT INTO commands(node_id, name, name_lower, args_text) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut ins_ref = tx.prepare("INSERT INTO ast_var_refs(node_id, name) VALUES (?1, ?2)")?;

        // Parsing is CPU-bound and per-file independent — do it in parallel
        // before the (inherently serial) SQLite insertion loop (§6.6).
        let parsed_all: Vec<(String, Option<cmakedb_syntax::ParsedFile>)> = {
            use rayon::prelude::*;
            all.par_iter()
                .map(|(path, _)| {
                    (
                        path.clone(),
                        cmakedb_syntax::parse_file(Path::new(path)).ok(),
                    )
                })
                .collect()
        };

        for (path, parsed) in parsed_all {
            // Project code = under the source root but not generated into
            // the build tree (which may be nested inside the source dir)
            // and not cmakedb's own injected hooks.
            let in_source = (crate::cmake_path_starts_with(&path, &src_prefix)
                || path == source_dir_str)
                && !crate::cmake_path_starts_with(&path, &build_prefix)
                && !path.contains("/.cmakedb/");
            let (hash, has_errors) = match &parsed {
                Some(p) => (Some(p.content_hash.clone()), p.has_errors),
                None => (None, false),
            };
            // Content is stored for project files so patch planning can be
            // a pure db function with offsets valid against the recording.
            let stored_content = match (&parsed, in_source) {
                (Some(p), true) => Some(p.content.as_str()),
                _ => None,
            };
            ins_file.execute(params![
                path,
                hash,
                in_source as i64,
                has_errors as i64,
                stored_content
            ])?;
            let file_id = tx.last_insert_rowid();
            if in_source {
                stats.source_files += 1;
            }

            if let Some(p) = &parsed {
                // Insert nodes; node ids in SQLite are assigned sequentially,
                // so remember the id of node index 0 to map indices -> ids.
                let mut first_node_id: Option<i64> = None;
                let mut node_ids: Vec<i64> = Vec::with_capacity(p.nodes.len());
                for n in &p.nodes {
                    let parent_db = n.parent.map(|pi| node_ids[pi]);
                    ins_node.execute(params![
                        file_id,
                        n.kind,
                        n.byte_start as i64,
                        n.byte_end as i64,
                        n.line as i64,
                        n.col as i64,
                        parent_db
                    ])?;
                    let id = tx.last_insert_rowid();
                    node_ids.push(id);
                    first_node_id.get_or_insert(id);
                }
                for c in &p.commands {
                    let node_db = node_ids[c.node];
                    let args_text: String = c
                        .args
                        .iter()
                        .map(|a| a.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    ins_cmd.execute(params![node_db, c.name, c.name_lower, args_text])?;
                    let mut refs: Vec<String> = c.var_refs.clone();
                    // Bare identifiers in conditions are variable reads too
                    // (if(FOO)); index them so unexecuted references are
                    // visible to the dead-code passes (§4.5).
                    if matches!(c.name_lower.as_str(), "if" | "elseif" | "while") {
                        refs.extend(
                            dataflow::condition_reads(
                                c.args.iter().map(|a| (a.kind, a.value.as_str())),
                            )
                            .into_iter()
                            .map(|(name, _)| name),
                        );
                        refs.sort();
                        refs.dedup();
                    }
                    for r in &refs {
                        ins_ref.execute(params![node_db, r])?;
                    }
                }
                // Stash mapping for the dataflow pass (index->db id) inside
                // the ParsedFile-adjacent structure below.
                files.by_path.insert(path.clone(), (file_id, parsed));
                // Store node id base for later resolution.
                NODE_ID_BASE.with(|m| {
                    m.borrow_mut().insert(file_id, node_ids);
                });
            } else {
                files.by_path.insert(path.clone(), (file_id, None));
            }
            stats.files += 1;
        }
    }

    // Pass 2: stream events through scope replay + dataflow, cross-checked
    // against scope snapshots when captured (§6.2: divergence is a hard
    // error and the whole transaction rolls back — no partial database).
    let snapshots = match &input.snapshots_path {
        Some(p) => Some((
            crate::cmake_path_spelling(&p.to_string_lossy()),
            dataflow::parse_snapshots(p).context("parsing scope snapshots")?,
        )),
        None => None,
    };
    let collected = dataflow::run(
        &tx,
        &input.trace_path,
        &files,
        snapshots,
        &input.preseeded_cache,
        &mut stats,
    )?;

    // File API: final resolved graph.
    if let Some(reply) = &input.reply_dir {
        if reply.is_dir() {
            fileapi::ingest(&tx, reply, &files, &collected, &mut stats)
                .context("ingesting File API reply")?;
        }
    }

    // Resolve and insert trace-collected targets/edges/props/reqs that the
    // File API didn't already cover.
    dataflow::finalize_graph(&tx, &collected, &mut stats)?;

    // CMake's own configure diagnostics, parsed from captured stderr.
    // Lossy read: cmake's stderr is not guaranteed UTF-8 and a mangled
    // byte must never fail ingestion.
    if let Some(stderr_path) = &input.stderr_path {
        if let Ok(bytes) = std::fs::read(stderr_path) {
            let mut ins = tx.prepare(
                "INSERT INTO configure_diagnostics(severity, kind, file, line, message)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for d in warnings::parse_stderr(&String::from_utf8_lossy(&bytes)) {
                ins.execute(params![
                    d.severity,
                    d.kind,
                    crate::cmake_path_spelling(&d.file),
                    d.line,
                    d.message
                ])?;
            }
        }
    }

    // Store preset file text so passes can apply the "referenced by presets"
    // guard (§4.5) without filesystem access (§3.4: passes are pure).
    for (fname, key) in [
        ("CMakePresets.json", "presets_json"),
        ("CMakeUserPresets.json", "user_presets_json"),
    ] {
        if let Ok(text) = std::fs::read_to_string(input.source_dir.join(fname)) {
            tx.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = ?2",
                params![key, text],
            )?;
        }
    }

    for (k, v) in &input.meta {
        tx.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![k, v],
        )?;
    }
    tx.execute(
        "INSERT INTO meta(key, value) VALUES ('source_dir', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![crate::cmake_path_spelling(
            &input.source_dir.to_string_lossy()
        )],
    )?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES ('scope_snapshots_validated', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![stats.snapshots_validated.to_string()],
    )?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES ('build_dir', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![crate::cmake_path_spelling(
            &input.build_dir.to_string_lossy()
        )],
    )?;

    tx.commit()?;
    db.conn.execute_batch(crate::schema::INDEXES)?;
    NODE_ID_BASE.with(|m| m.borrow_mut().clear());
    Ok(stats)
}

thread_local! {
    /// file_id -> (AST node index -> ast_nodes.id). Populated during file
    /// insertion, consumed by the dataflow pass, cleared after ingest.
    pub(crate) static NODE_ID_BASE: std::cell::RefCell<HashMap<i64, Vec<i64>>> =
        std::cell::RefCell::new(HashMap::new());
}

fn find_cmake_sources(source_dir: &Path, build_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![source_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir == build_dir {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if p.is_dir() {
                if name == ".git" || name == ".cmakedb" || p == build_dir {
                    continue;
                }
                stack.push(p);
            } else if name == "CMakeLists.txt"
                || (name.ends_with(".cmake")
                    // configure_file templates (config.h.cmake etc.) are not
                    // CMake code despite the extension.
                    && !std::path::Path::new(name.trim_end_matches(".cmake"))
                        .extension()
                        .is_some())
            {
                // Keep the source_dir-rooted spelling — trace paths use
                // cmake's spelling of -S, not resolved symlinks.
                out.push(p);
            }
        }
    }
    out.sort();
    out
}
