//! Streaming scope replay and variable dataflow (design §4.2), plus
//! collection of the trace-side build graph (targets, link edges,
//! property writes, usage requirements).
//!
//! Scope model, validated against CMake 4.2 traces:
//!  - `global_frame` is a depth counter: events directly inside a scope
//!    share one depth; a call that opens a frame is traced at depth d and
//!    its body at d+1, so the *previous* event is always the opener.
//!  - `frame` resets per directory and is not used.
//!  - Macro bodies DO open a frame (traced at the definition site) but do
//!    NOT create a variable scope -> transparent scope entries.
//!  - `block()`/`endblock()` do NOT open frames and `end*` commands are
//!    never traced, so block scopes close when an event's AST node falls
//!    outside the `block_def` byte range.

use anyhow::Result;
use rusqlite::{params, Transaction};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::trace::{for_each_event, TraceEvent};
use super::{FileTable, IngestStats, NODE_ID_BASE};
use cmakedb_syntax::{ArgKind, ParsedFile};

#[derive(Debug, Clone)]
pub struct TraceTarget {
    pub name: String,
    pub ttype: Option<String>,
    pub imported: bool,
    pub alias_of: Option<String>,
    pub event_id: i64,
}

#[derive(Debug, Clone)]
pub struct TraceEdge {
    pub src: String,
    pub dst: String,
    pub visibility: String,
    pub event_id: i64,
}

#[derive(Default)]
pub struct CollectedGraph {
    pub targets: Vec<TraceTarget>,
    pub edges: Vec<TraceEdge>,
    /// (target, prop, value, appended, event_id)
    pub props: Vec<(String, String, Option<String>, bool, i64)>,
    /// (target, kind, value, visibility, event_id)
    pub reqs: Vec<(String, &'static str, String, String, i64)>,
    /// (file_id, line) -> first event id at that location (File API
    /// backtrace join).
    pub event_loc: HashMap<(i64, i64), i64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ScopeKind {
    Root,
    Directory,
    Function,
    Macro,
    Include,
    Block,
    /// try_compile/try_run scratch configures run nested inside the trace
    /// but are an isolated variable world in real CMake — opaque so their
    /// writes never leak into the caller's replayed scopes.
    TryCompile,
    Unknown,
}

impl ScopeKind {
    fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Root => "root",
            ScopeKind::Directory => "directory",
            ScopeKind::Function => "function",
            ScopeKind::Macro => "macro",
            ScopeKind::Include => "include",
            ScopeKind::Block => "block",
            ScopeKind::TryCompile => "try_compile",
            ScopeKind::Unknown => "unknown",
        }
    }
}

struct OpenScope {
    id: i64,
    /// global_frame depth of events directly inside this scope.
    body_depth: i64,
    kind: ScopeKind,
    /// Transparent scopes have no variable table (macro, include, unknown).
    transparent: bool,
    /// Variable table: name -> dominating var_writes.id (opaque scopes).
    vars: HashMap<String, i64>,
    /// Macro parameter pseudo-variables (`${arg}` is textual substitution
    /// but reads must still resolve to the invocation).
    params: HashMap<String, i64>,
    /// For block scopes: (file_id, byte_start, byte_end) of the block_def.
    block_range: Option<(i64, usize, usize)>,
}

struct Pending {
    body_depth: i64,
    kind: ScopeKind,
    name: String,
    /// function: declared params + call args for synthesis
    params: Vec<String>,
    call_args: Vec<String>,
    opened_by: i64,
}

#[derive(Clone)]
struct FuncDef {
    kind: ScopeKind, // Function | Macro
    params: Vec<String>,
}

/// Per-file command lookup: line -> indices into ParsedFile::commands.
struct CmdIndex<'a> {
    parsed: &'a ParsedFile,
    node_ids: &'a [i64],
    by_line: HashMap<i64, Vec<usize>>,
}

/// One scope snapshot dumped by the injected hook (§3.2 channel 2).
#[derive(Debug)]
pub struct Snapshot {
    pub tag: String,
    pub vars: HashMap<String, String>,
}

/// Parse the hex-encoded TSV sidecar written by the snapshot hook.
pub fn parse_snapshots(path: &Path) -> Result<Vec<Snapshot>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading snapshots {}: {e}", path.display()))?;
    let mut out: Vec<Snapshot> = Vec::new();
    for line in text.lines() {
        if let Some(tag) = line.strip_prefix("SNAPSHOT\t") {
            out.push(Snapshot {
                tag: tag.to_string(),
                vars: HashMap::new(),
            });
        } else if let Some(snap) = out.last_mut() {
            if let Some((name, hex)) = line.split_once('\t') {
                snap.vars.insert(name.to_string(), decode_hex(hex));
            }
        }
    }
    Ok(out)
}

fn decode_hex(hex: &str) -> String {
    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks_exact(2)
        .filter_map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok())
        .collect();
    String::from_utf8_lossy(&bytes).to_string()
}

pub fn run(
    tx: &Transaction,
    trace_path: &Path,
    files: &FileTable,
    snapshots: Option<(String, Vec<Snapshot>)>,
    preseeded_cache: &[(String, String)],
    stats: &mut IngestStats,
) -> Result<CollectedGraph> {
    // Build command indexes. NODE_ID_BASE maps file_id -> node idx -> db id.
    let node_bases = NODE_ID_BASE.with(|m| m.borrow().clone());
    let mut index: HashMap<i64, CmdIndex> = HashMap::new();
    for (fid, parsed) in files.by_path.values() {
        if let (Some(p), Some(ids)) = (parsed.as_ref(), node_bases.get(fid)) {
            let mut by_line: HashMap<i64, Vec<usize>> = HashMap::new();
            for (i, c) in p.commands.iter().enumerate() {
                by_line.entry(c.line as i64).or_default().push(i);
            }
            index.insert(
                *fid,
                CmdIndex {
                    parsed: p,
                    node_ids: ids,
                    by_line,
                },
            );
        }
    }

    let mut ins_event = tx.prepare(
        "INSERT INTO events(node_id, file_id, line, cmd, cmd_lower, args_json,
                            scope_id, frame, global_frame, time_abs, elapsed_us)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0)",
    )?;
    let mut ins_scope = tx.prepare(
        "INSERT INTO scopes(parent_id, kind, name, transparent, opened_by_event)
         VALUES (?1,?2,?3,?4,?5)",
    )?;
    let mut close_scope = tx.prepare("UPDATE scopes SET closed_by_event = ?2 WHERE id = ?1")?;
    let mut ins_write = tx.prepare(
        "INSERT INTO var_writes(event_id, scope_id, name, value, write_kind)
         VALUES (?1,?2,?3,?4,?5)",
    )?;
    let mut ins_read = tx.prepare(
        "INSERT INTO var_reads(event_id, scope_id, name, resolved_write_id, read_kind)
         VALUES (?1,?2,?3,?4,?5)",
    )?;
    // Separate statement for read_kind='env' rows: env reads are recorded
    // outside the record_read closure (which holds ins_read mutably) and
    // resolve against env_vars only.
    let mut ins_env_read = tx.prepare(
        "INSERT INTO var_reads(event_id, scope_id, name, resolved_write_id, read_kind)
         VALUES (?1,?2,?3,?4,'env')",
    )?;
    let mut ins_def = tx.prepare(
        "INSERT INTO func_defs(event_id, name, name_lower, kind, file_id, line, params_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
    )?;

    let mut stack: Vec<OpenScope> = Vec::new();
    let mut cache_vars: HashMap<String, i64> = HashMap::new();
    // name -> dominating var_writes.id of a `set(ENV{X} ...)` in *this*
    // configure. $ENV{X} reads resolve ONLY against this map — a resolving
    // read is project-internal (reproducible); an unresolved one reads the
    // ambient environment (the env-dependence signal). Environment reads
    // never consult the variable scope chain or the cache.
    let mut env_vars: HashMap<String, i64> = HashMap::new();
    let mut defs: HashMap<String, FuncDef> = HashMap::new();
    let mut pending: Option<Pending> = None;
    let mut graph = CollectedGraph::default();
    let mut last_event_id = 0i64;
    // Snapshot validation state: write-id -> concrete value for comparison,
    // and the queue of dumped snapshots paired to file(APPEND) events.
    let mut write_values: HashMap<i64, String> = HashMap::new();
    let mut snap_idx = 0usize;
    let mut snap_mismatches: Vec<String> = Vec::new();

    for_each_event(trace_path, |e| {
        let fid = files.file_id(&e.file).expect("file collected in pass 1");
        // Scope reconstruction is built on `global_frame`; a cmake too old
        // to emit it (the field is 1-based, so 0 means absent) would
        // silently collapse the replay into one flat scope. Fail loudly
        // instead — the floor is on the *recording binary*, not on the
        // project's cmake_minimum_required.
        if e.global_frame == 0 {
            anyhow::bail!(
                "trace events carry no `global_frame` field — the cmake binary that \
                 produced this recording is too old for cmakedb's scope replay. \
                 Record with cmake >= 3.25 (the project's cmake_minimum_required \
                 may be lower; only the binary version matters)."
            );
        }
        let depth = e.global_frame;

        // --- Node join -----------------------------------------------------
        let (node_db, cmd_idx): (Option<i64>, Option<usize>) = match index.get(&fid) {
            Some(ci) => join_node(ci, e),
            None => (None, None),
        };

        // --- Root scope ----------------------------------------------------
        if stack.is_empty() {
            ins_scope.execute(params![
                Option::<i64>::None,
                "root",
                e.file,
                0i64,
                Option::<i64>::None
            ])?;
            stack.push(OpenScope {
                id: tx.last_insert_rowid(),
                body_depth: depth,
                kind: ScopeKind::Root,
                transparent: false,
                vars: HashMap::new(),
                params: HashMap::new(),
                block_range: None,
            });
        }

        // --- Close scopes we have returned from ----------------------------
        while stack.len() > 1 {
            let top = stack.last().unwrap();
            let close = if top.kind == ScopeKind::Block {
                // Block scopes share their surroundings' depth; close when
                // execution moves outside the block_def node.
                top.body_depth > depth
                    || (top.body_depth == depth
                        && match (top.block_range, node_db) {
                            (Some((bfid, bs, be)), Some(_)) => {
                                let ci = index.get(&fid);
                                match (ci, cmd_idx) {
                                    (Some(ci), Some(i)) => {
                                        let n = &ci.parsed.nodes[ci.parsed.commands[i].node];
                                        bfid != fid || n.byte_start < bs || n.byte_start >= be
                                    }
                                    _ => false,
                                }
                            }
                            (Some((bfid, _, _)), None) => bfid != fid,
                            (None, _) => false,
                        })
            } else {
                top.body_depth > depth
            };
            if !close {
                break;
            }
            let s = stack.pop().unwrap();
            close_scope.execute(params![s.id, last_event_id])?;
        }

        // --- Open a scope if the previous event was a frame opener ---------
        if let Some(p) = pending.take() {
            if p.body_depth == depth {
                let parent_id = stack.last().map(|s| s.id);
                let transparent = matches!(
                    p.kind,
                    ScopeKind::Macro | ScopeKind::Include | ScopeKind::Unknown
                );
                ins_scope.execute(params![
                    parent_id,
                    p.kind.as_str(),
                    p.name,
                    transparent as i64,
                    p.opened_by
                ])?;
                let sid = tx.last_insert_rowid();
                let mut scope = OpenScope {
                    id: sid,
                    body_depth: depth,
                    kind: p.kind,
                    transparent,
                    vars: HashMap::new(),
                    params: HashMap::new(),
                    block_range: None,
                };
                // Synthesize parameter variables for function/macro calls
                // (design §4.2: reads of `${arg}` must resolve).
                if matches!(p.kind, ScopeKind::Function | ScopeKind::Macro) {
                    let write_values = &mut write_values;
                    let mut synth = |scope: &mut OpenScope,
                                     name: &str,
                                     value: String|
                     -> Result<()> {
                        ins_write.execute(params![p.opened_by, sid, name, &value, "synthetic"])?;
                        let wid = tx.last_insert_rowid();
                        write_values.insert(wid, value);
                        if p.kind == ScopeKind::Macro {
                            scope.params.insert(name.to_string(), wid);
                        } else {
                            scope.vars.insert(name.to_string(), wid);
                        }
                        Ok(())
                    };
                    for (i, param) in p.params.iter().enumerate() {
                        let v = p.call_args.get(i).cloned().unwrap_or_default();
                        synth(&mut scope, param, v)?;
                    }
                    synth(&mut scope, "ARGC", p.call_args.len().to_string())?;
                    synth(&mut scope, "ARGV", p.call_args.join(";"))?;
                    let argn = if p.call_args.len() > p.params.len() {
                        p.call_args[p.params.len()..].join(";")
                    } else {
                        String::new()
                    };
                    synth(&mut scope, "ARGN", argn)?;
                    for (i, a) in p.call_args.iter().enumerate() {
                        synth(&mut scope, &format!("ARGV{i}"), a.clone())?;
                    }
                }
                stack.push(scope);
            }
        }

        // --- Insert the event ----------------------------------------------
        let scope_id = stack.last().unwrap().id;
        let cmd_lower = e.cmd.to_ascii_lowercase();
        ins_event.execute(params![
            node_db,
            fid,
            e.line,
            e.cmd,
            cmd_lower,
            serde_json::to_string(&e.args)?,
            scope_id,
            e.frame,
            e.global_frame,
            e.time,
        ])?;
        let eid = tx.last_insert_rowid();
        let first_event = stats.events == 0;
        last_event_id = eid;
        stats.events += 1;

        // Command-line -D cache entries (cache-v2) logically precede the
        // whole configure but emit no trace events; seed them as cache
        // writes anchored to the first event so early reads resolve.
        if first_event {
            for (name, value) in preseeded_cache {
                ins_write.execute(params![eid, Option::<i64>::None, name, value, "cache"])?;
                let wid = tx.last_insert_rowid();
                cache_vars.insert(name.clone(), wid);
                write_values.insert(wid, value.clone());
                stats.var_writes += 1;
            }
        }
        if node_db.is_some() {
            stats.joined_events += 1;
        }
        graph.event_loc.entry((fid, e.line)).or_insert(eid);

        // --- Function/macro definitions ------------------------------------
        if cmd_lower == "function" || cmd_lower == "macro" {
            if let Some(name) = e.args.first() {
                let kind = if cmd_lower == "function" {
                    ScopeKind::Function
                } else {
                    ScopeKind::Macro
                };
                let params: Vec<String> = e.args[1..].to_vec();
                defs.insert(
                    name.to_ascii_lowercase(),
                    FuncDef {
                        kind,
                        params: params.clone(),
                    },
                );
                ins_def.execute(params![
                    eid,
                    name,
                    name.to_ascii_lowercase(),
                    if kind == ScopeKind::Function {
                        "function"
                    } else {
                        "macro"
                    },
                    fid,
                    e.line,
                    serde_json::to_string(&params)?,
                ])?;
            }
        }

        // --- Reads ---------------------------------------------------------
        // 0. `$ENV{X}` references (read_kind='env') from the joined AST
        //    node's raw text, plus bare `ENV{X}` condition tokens
        //    (`if(DEFINED ENV{X})`). Recorded for every joined event —
        //    unresolved rows are the env-dependence signal — but resolved
        //    ONLY against prior `set(ENV{X} ...)` writes (env_vars), and
        //    *before* this event's own writes are processed so
        //    `set(ENV{X} "$ENV{X};more")` reads the prior state.
        if let (Some(ci), Some(i)) = (index.get(&fid), cmd_idx) {
            let cmd = &ci.parsed.commands[i];
            let mut env_names: Vec<&str> = cmd.env_refs.iter().map(|s| s.as_str()).collect();
            if matches!(cmd_lower.as_str(), "if" | "elseif" | "while") {
                for a in &cmd.args {
                    if a.kind == ArgKind::Unquoted {
                        if let Some(name) = a
                            .value
                            .strip_prefix("ENV{")
                            .and_then(|s| s.strip_suffix('}'))
                        {
                            if !name.is_empty() && !name.contains('$') {
                                env_names.push(name);
                            }
                        }
                    }
                }
            }
            env_names.sort_unstable();
            env_names.dedup();
            for name in env_names {
                let found = env_vars.get(name).copied();
                ins_env_read.execute(params![eid, scope_id, name, found])?;
                stats.var_reads += 1;
            }
        }

        // Resolve a name through the dynamic scope chain (macro params,
        // then opaque scope tables top-down), falling back to the cache.
        let mut record_read = |name: &str,
                               kind: &str,
                               force: bool,
                               cache_vars: &HashMap<String, i64>|
         -> Result<()> {
            let mut found: Option<i64> = None;
            for s in stack.iter().rev() {
                if let Some(w) = s.params.get(name) {
                    found = Some(*w);
                    break;
                }
                if !s.transparent {
                    if let Some(w) = s.vars.get(name) {
                        found = Some(*w);
                        break;
                    }
                }
            }
            let found = found.or_else(|| cache_vars.get(name).copied());
            if found.is_some() || force {
                ins_read.execute(params![eid, scope_id, name, found, kind])?;
                stats.var_reads += 1;
            }
            Ok(())
        };

        // 1. `${X}` references from the joined AST node's raw argument text.
        if let (Some(ci), Some(i)) = (index.get(&fid), cmd_idx) {
            let cmd = &ci.parsed.commands[i];
            for name in &cmd.var_refs {
                record_read(name, "expand", true, &cache_vars)?;
            }
            // 2. Condition reads: if/elseif/while auto-dereference bare
            //    identifiers.
            if matches!(cmd_lower.as_str(), "if" | "elseif" | "while") {
                for name in condition_reads(cmd.args.iter().map(|a| (a.kind, a.value.as_str()))) {
                    let force = name.1;
                    record_read(&name.0, "condition", force, &cache_vars)?;
                }
            }
        }
        // 3. foreach(v IN LISTS a b): list variables are reads.
        if cmd_lower == "foreach" {
            let mut in_lists = false;
            for a in &e.args {
                match a.as_str() {
                    "IN" => {}
                    "LISTS" => in_lists = true,
                    "ITEMS" | "RANGE" | "ZIP_LISTS" => in_lists = a == "ZIP_LISTS",
                    _ if in_lists => record_read(a, "listarg", false, &cache_vars)?,
                    _ => {}
                }
            }
        }

        // --- Writes --------------------------------------------------------
        // Raw argument kinds (quoted vs unquoted) from the joined node,
        // usable only when source and trace argument counts line up 1:1.
        let arg_kinds: Option<Vec<ArgKind>> = match (index.get(&fid), cmd_idx) {
            (Some(ci), Some(i)) => {
                let cargs = &ci.parsed.commands[i].args;
                (cargs.len() == e.args.len()).then(|| cargs.iter().map(|a| a.kind).collect())
            }
            _ => None,
        };
        let mut writes = extract_writes(&cmd_lower, &e.args, arg_kinds.as_deref());
        // Inside a try_compile scratch configure, cache writes belong to the
        // nested instance's cache, not ours — contain them like locals.
        let in_try_compile = stack.iter().any(|s| s.kind == ScopeKind::TryCompile);
        if in_try_compile {
            for w in &mut writes.writes {
                if matches!(w.kind, WriteKind::Cache | WriteKind::CacheSoft) {
                    w.kind = WriteKind::Set;
                }
            }
        }
        for w in &writes.reads {
            record_read(w, "listarg", false, &cache_vars)?;
        }
        for w in writes.writes {
            // Effective scope: nearest opaque, one more up for PARENT_SCOPE.
            let mut opaque: Vec<usize> = Vec::new();
            for (i, s) in stack.iter().enumerate().rev() {
                if !s.transparent {
                    opaque.push(i);
                }
            }
            let (target_idx, record_scope): (Option<usize>, Option<i64>) = match w.kind {
                WriteKind::Set | WriteKind::Unset => (
                    opaque.first().copied(),
                    opaque.first().map(|i| stack[*i].id),
                ),
                WriteKind::ParentScope => {
                    let idx = opaque.get(1).copied().or_else(|| opaque.first().copied());
                    (idx, idx.map(|i| stack[i].id))
                }
                WriteKind::Cache | WriteKind::CacheSoft | WriteKind::Env => (None, None),
            };
            let kind_str = match w.kind {
                WriteKind::Set => "set",
                WriteKind::Cache | WriteKind::CacheSoft => "cache",
                WriteKind::Env => "env",
                WriteKind::ParentScope => "parent_scope",
                WriteKind::Unset => "unset",
            };
            ins_write.execute(params![eid, record_scope, w.name, w.value, kind_str])?;
            let wid = tx.last_insert_rowid();
            stats.var_writes += 1;
            if let Some(v) = &w.value {
                write_values.insert(wid, v.clone());
            }
            match w.kind {
                WriteKind::Cache => {
                    cache_vars.insert(w.name.clone(), wid);
                }
                WriteKind::CacheSoft => {
                    cache_vars.entry(w.name.clone()).or_insert(wid);
                }
                WriteKind::Env => {
                    // Track for $ENV{X} read resolution. A valueless
                    // set(ENV{X}) / unset(ENV{X}) removes the entry. Env
                    // writes inside try_compile scratch configures happen
                    // in a child cmake process and never reach us.
                    if !in_try_compile {
                        if w.value.is_some() {
                            env_vars.insert(w.name.clone(), wid);
                        } else {
                            env_vars.remove(&w.name);
                        }
                    }
                }
                WriteKind::Unset => {
                    if let Some(i) = target_idx {
                        stack[i].vars.remove(&w.name);
                    }
                }
                _ => {
                    if let Some(i) = target_idx {
                        stack[i].vars.insert(w.name.clone(), wid);
                    }
                }
            }
        }
        // --- Scope-snapshot validation (§6.2) -------------------------------
        // Each `file(APPEND <sidecar> ...)` event from the injected hook
        // marks one dumped snapshot; the replayed table visible at exactly
        // this point must agree on every dumped name we hold a concrete
        // value for. Cache is NOT consulted: get_cmake_property(VARIABLES)
        // lists normal variables only.
        if let Some((snapfile, snaps)) = &snapshots {
            if cmd_lower == "file"
                && e.args.first().map(|a| a == "APPEND").unwrap_or(false)
                && e.args.get(1).map(|a| a == snapfile).unwrap_or(false)
            {
                if let Some(snap) = snaps.get(snap_idx) {
                    snap_idx += 1;
                    stats.snapshots_validated += 1;
                    for (name, dumped) in &snap.vars {
                        let mut found: Option<i64> = None;
                        for s in stack.iter().rev() {
                            if let Some(w) = s.params.get(name) {
                                found = Some(*w);
                                break;
                            }
                            if !s.transparent {
                                if let Some(w) = s.vars.get(name) {
                                    found = Some(*w);
                                    break;
                                }
                            }
                        }
                        let Some(wid) = found else { continue };
                        let Some(replayed) = write_values.get(&wid) else {
                            continue;
                        };
                        stats.snapshot_vars_checked += 1;
                        if replayed != dumped {
                            snap_mismatches.push(format!(
                                "snapshot '{}': {name}: replayed '{}' but CMake dumped '{}'",
                                snap.tag,
                                truncate_for_error(replayed),
                                truncate_for_error(dumped),
                            ));
                        }
                    }
                }
            }
        }

        // --- Build-graph collection ----------------------------------------
        // try_compile/try_run scratch projects define throwaway targets
        // (cmTC_*) in generated files; keep them out of the build graph.
        if !e.file.contains("/CMakeScratch/") && !e.file.contains("/CMakeTmp/") {
            collect_graph(&cmd_lower, &e.args, eid, &mut graph);
        }

        // --- Block scopes ---------------------------------------------------
        if cmd_lower == "block" {
            // block(SCOPE_FOR POLICIES) does not scope variables.
            let scopes_vars =
                !e.args.iter().any(|a| a == "SCOPE_FOR") || e.args.iter().any(|a| a == "VARIABLES");
            let range = match (index.get(&fid), cmd_idx) {
                (Some(ci), Some(i)) => {
                    let node = ci.parsed.commands[i].node;
                    ci.parsed.nodes[node].parent.map(|p| {
                        let pn = &ci.parsed.nodes[p];
                        (fid, pn.byte_start, pn.byte_end)
                    })
                }
                _ => None,
            };
            ins_scope.execute(params![
                Some(scope_id),
                "block",
                Option::<String>::None,
                (!scopes_vars) as i64,
                eid
            ])?;
            stack.push(OpenScope {
                id: tx.last_insert_rowid(),
                body_depth: depth,
                kind: ScopeKind::Block,
                transparent: !scopes_vars,
                vars: HashMap::new(),
                params: HashMap::new(),
                block_range: range,
            });
        }

        // --- Pending scope for the next event -------------------------------
        pending = Some(compute_pending(&cmd_lower, e, eid, depth, &defs));
        Ok(())
    })?;

    // Close all remaining scopes.
    while let Some(s) = stack.pop() {
        close_scope.execute(params![s.id, last_event_id])?;
    }

    // §6.2: divergence between the replay and CMake's own dumped tables is
    // a hard error — fail loud, never produce silently wrong provenance.
    // (The enclosing transaction rolls back: no partial database.)
    if !snap_mismatches.is_empty() {
        let shown: Vec<_> = snap_mismatches.iter().take(10).cloned().collect();
        anyhow::bail!(
            "scope replay diverged from CMake's dumped variable tables \
             ({} mismatch(es)); this is a cmakedb bug — please report it:\n  {}",
            snap_mismatches.len(),
            shown.join("\n  ")
        );
    }

    // elapsed_us ≈ time to the next event (approximation: includes child
    // command time for frame openers). One window-function pass instead of
    // a per-event UPDATE — this matters at LLVM scale.
    tx.execute_batch(
        "WITH d AS (
           SELECT id,
                  CAST(max((lead(time_abs) OVER (ORDER BY id) - time_abs) * 1e6, 0)
                       AS INTEGER) AS e
           FROM events)
         UPDATE events SET elapsed_us = coalesce(d.e, 0)
         FROM d WHERE d.id = events.id;",
    )?;

    // Insert trace-observed targets (File API upserts on top of these).
    let mut ins_tgt = tx.prepare(
        "INSERT INTO targets(name, type, imported, alias_of, defined_event, in_file_api)
         VALUES (?1,?2,?3,?4,?5,0)
         ON CONFLICT(name) DO NOTHING",
    )?;
    for t in &graph.targets {
        ins_tgt.execute(params![
            t.name,
            t.ttype,
            t.imported as i64,
            t.alias_of,
            t.event_id
        ])?;
    }

    Ok(graph)
}

fn join_node(ci: &CmdIndex, e: &TraceEvent) -> (Option<i64>, Option<usize>) {
    let cmd_lower = e.cmd.to_ascii_lowercase();
    let Some(cands) = ci.by_line.get(&e.line) else {
        return (None, None);
    };
    let matching: Vec<usize> = cands
        .iter()
        .copied()
        .filter(|&i| ci.parsed.commands[i].name_lower == cmd_lower)
        .collect();
    let chosen = match matching.len() {
        0 => None,
        1 => Some(matching[0]),
        _ => {
            // Multiple same-named commands on one line: prefer the candidate
            // whose literal (non-${}) arguments all appear in the expanded
            // event args (design §3.1 argument-text disambiguation).
            matching
                .iter()
                .copied()
                .find(|&i| {
                    ci.parsed.commands[i]
                        .args
                        .iter()
                        .filter(|a| !a.text.contains('$'))
                        .all(|a| e.args.iter().any(|ea| ea == &a.value))
                })
                .or(Some(matching[0]))
        }
    };
    match chosen {
        Some(i) => (Some(ci.node_ids[ci.parsed.commands[i].node]), Some(i)),
        None => (None, None),
    }
}

fn compute_pending(
    cmd_lower: &str,
    e: &TraceEvent,
    eid: i64,
    depth: i64,
    defs: &HashMap<String, FuncDef>,
) -> Pending {
    let mk = |kind, name: String, params: Vec<String>, call_args: Vec<String>| Pending {
        body_depth: depth + 1,
        kind,
        name,
        params,
        call_args,
        opened_by: eid,
    };
    if cmd_lower == "add_subdirectory" {
        return mk(
            ScopeKind::Directory,
            e.args.first().cloned().unwrap_or_default(),
            vec![],
            vec![],
        );
    }
    if cmd_lower == "cmake_language" {
        // cmake_language(CALL fn args...) / (EVAL CODE ...) / (DEFER CALL ..)
        if e.args.first().map(|a| a == "CALL").unwrap_or(false) {
            if let Some(name) = e.args.get(1) {
                if let Some(d) = defs.get(&name.to_ascii_lowercase()) {
                    return mk(d.kind, name.clone(), d.params.clone(), e.args[2..].to_vec());
                }
            }
        }
        return mk(ScopeKind::Unknown, e.args.join(" "), vec![], vec![]);
    }
    if let Some(d) = defs.get(cmd_lower) {
        return mk(
            d.kind,
            cmd_lower.to_string(),
            d.params.clone(),
            e.args.clone(),
        );
    }
    if matches!(cmd_lower, "try_compile" | "try_run") {
        return mk(ScopeKind::TryCompile, cmd_lower.to_string(), vec![], vec![]);
    }
    if matches!(
        cmd_lower,
        "include"
            | "find_package"
            | "project"
            | "include_guard"
            | "enable_language"
            | "cmake_minimum_required"
    ) {
        return mk(
            ScopeKind::Include,
            e.args.first().cloned().unwrap_or_default(),
            vec![],
            vec![],
        );
    }
    mk(ScopeKind::Unknown, cmd_lower.to_string(), vec![], vec![])
}

// --- Writes ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum WriteKind {
    Set,
    Cache,
    /// Cache write that does NOT overwrite an existing entry: non-FORCE
    /// `set(... CACHE)`, `option()`, `find_*`. When the entry already
    /// exists (-D/preset preseed or an earlier cache write), the declared
    /// default is recorded but the old write keeps winning resolution.
    CacheSoft,
    Env,
    ParentScope,
    Unset,
}

struct VarWrite {
    name: String,
    value: Option<String>,
    kind: WriteKind,
}

#[derive(Default)]
struct ExtractedWrites {
    writes: Vec<VarWrite>,
    /// Variables the command reads as part of writing (list(APPEND V ..)).
    reads: Vec<String>,
}

fn extract_writes(cmd: &str, args: &[String], arg_kinds: Option<&[ArgKind]>) -> ExtractedWrites {
    let mut out = ExtractedWrites::default();
    let mut write = |name: &str, value: Option<String>, kind: WriteKind| {
        if !name.is_empty() {
            out.writes.push(VarWrite {
                name: name.to_string(),
                value,
                kind,
            });
        }
    };
    let arg = |i: usize| args.get(i).map(|s| s.as_str()).unwrap_or("");
    // Trace-format fact (validated by §6.2 snapshot cross-checks): json-v1
    // args are *per-source-argument* expansions. An unquoted argument that
    // expanded to nothing appears as '' but the real command never received
    // it; a quoted "" IS a real (empty) argument. The joined AST tells the
    // two apart; without a join, dropping empties is right far more often.
    let join_values = |range: std::ops::Range<usize>| -> String {
        args[range.clone()]
            .iter()
            .enumerate()
            .filter(|(off, v)| {
                !v.is_empty()
                    || matches!(
                        arg_kinds.and_then(|k| k.get(range.start + off)),
                        Some(ArgKind::Quoted | ArgKind::Bracket)
                    )
            })
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join(";")
    };
    match cmd {
        "set" => {
            let Some(name) = args.first() else { return out };
            if let Some(inner) = name.strip_prefix("ENV{").and_then(|s| s.strip_suffix('}')) {
                write(inner, args.get(1).cloned(), WriteKind::Env);
            } else if let Some(pos) = args.iter().position(|a| a == "CACHE") {
                let kind = if args[pos..].iter().any(|a| a == "FORCE") {
                    WriteKind::Cache
                } else {
                    WriteKind::CacheSoft
                };
                write(name, Some(join_values(1..pos)), kind);
            } else if args.last().map(|a| a == "PARENT_SCOPE").unwrap_or(false) {
                write(
                    name,
                    Some(join_values(1..args.len() - 1)),
                    WriteKind::ParentScope,
                );
            } else {
                write(name, Some(join_values(1..args.len())), WriteKind::Set);
            }
        }
        "unset" => {
            let Some(name) = args.first() else { return out };
            if let Some(inner) = name.strip_prefix("ENV{").and_then(|s| s.strip_suffix('}')) {
                write(inner, None, WriteKind::Env);
            } else if args.iter().any(|a| a == "CACHE") {
                write(name, None, WriteKind::Cache);
            } else if args.iter().any(|a| a == "PARENT_SCOPE") {
                write(name, None, WriteKind::ParentScope);
            } else {
                write(name, None, WriteKind::Unset);
            }
        }
        "option" => {
            write(
                arg(0),
                Some(args.get(2).cloned().unwrap_or_else(|| "OFF".into())),
                WriteKind::CacheSoft,
            );
        }
        "find_program" | "find_library" | "find_path" | "find_file" => {
            write(arg(0), None, WriteKind::CacheSoft);
        }
        "list" => match arg(0) {
            "APPEND" | "PREPEND" | "INSERT" | "REMOVE_ITEM" | "REMOVE_AT" | "REMOVE_DUPLICATES"
            | "REVERSE" | "SORT" | "FILTER" => {
                out.reads.push(arg(1).to_string());
                write(arg(1), None, WriteKind::Set);
            }
            "POP_BACK" | "POP_FRONT" => {
                out.reads.push(arg(1).to_string());
                write(arg(1), None, WriteKind::Set);
                for v in &args[2..] {
                    write(v, None, WriteKind::Set);
                }
            }
            "LENGTH" | "GET" | "FIND" | "JOIN" | "SUBLIST" => {
                out.reads.push(arg(1).to_string());
                if let Some(last) = args.last() {
                    if args.len() >= 3 {
                        write(last, None, WriteKind::Set);
                    }
                }
            }
            "TRANSFORM" => {
                out.reads.push(arg(1).to_string());
                if let Some(pos) = args.iter().position(|a| a == "OUTPUT_VARIABLE") {
                    write(arg(pos + 1), None, WriteKind::Set);
                } else {
                    write(arg(1), None, WriteKind::Set);
                }
            }
            _ => {}
        },
        "string" => match arg(0) {
            "APPEND" | "PREPEND" => {
                out.reads.push(arg(1).to_string());
                write(arg(1), None, WriteKind::Set);
            }
            "REPLACE" => write(arg(3), None, WriteKind::Set),
            "REGEX" => match arg(1) {
                "MATCH" | "MATCHALL" => write(arg(3), None, WriteKind::Set),
                "REPLACE" => write(arg(4), None, WriteKind::Set),
                _ => {}
            },
            "CONCAT" | "JOIN" | "TIMESTAMP" | "UUID" | "MD5" | "SHA1" | "SHA224" | "SHA256"
            | "SHA384" | "SHA512" | "SHA3_224" | "SHA3_256" | "SHA3_384" | "SHA3_512"
            | "MAKE_C_IDENTIFIER" | "GENEX_STRIP" | "HEX" | "ASCII" => {
                // string(<op> <out> ...) except JOIN which is (JOIN glue out..)
                let out_idx = if arg(0) == "JOIN" { 2 } else { 1 };
                write(arg(out_idx), None, WriteKind::Set);
            }
            "TOUPPER" | "TOLOWER" | "STRIP" | "LENGTH" => write(arg(2), None, WriteKind::Set),
            "SUBSTRING" => write(arg(4), None, WriteKind::Set),
            "RANDOM" => {
                if let Some(last) = args.last() {
                    write(last, None, WriteKind::Set);
                }
            }
            "FIND" => write(arg(3), None, WriteKind::Set),
            "COMPARE" => write(arg(4), None, WriteKind::Set),
            "REPEAT" => write(arg(3), None, WriteKind::Set),
            "JSON" => write(arg(1), None, WriteKind::Set),
            _ => {}
        },
        "math" => {
            if arg(0) == "EXPR" {
                write(arg(1), None, WriteKind::Set);
            }
        }
        "include" => {
            // include(<file> OPTIONAL RESULT_VARIABLE <var>): the full path
            // (or NOTFOUND) lands in <var>; the value isn't in the trace.
            if let Some(pos) = args.iter().position(|a| a == "RESULT_VARIABLE") {
                write(arg(pos + 1), None, WriteKind::Set);
            }
        }
        "foreach" => {
            // Loop variables: everything before IN/RANGE, else the first arg.
            if let Some(pos) = args.iter().position(|a| a == "IN" || a == "RANGE") {
                for v in &args[..pos.max(1).min(args.len())] {
                    if v != "IN" && v != "RANGE" {
                        write(v, None, WriteKind::Set);
                    }
                }
            } else if !args.is_empty() {
                write(arg(0), None, WriteKind::Set);
            }
        }
        "get_filename_component"
        | "get_property"
        | "get_target_property"
        | "get_cmake_property"
        | "get_directory_property"
        | "get_source_file_property"
        | "get_test_property"
        | "separate_arguments"
        | "site_name" => {
            write(arg(0), None, WriteKind::Set);
        }
        "execute_process" | "exec_program" | "try_compile" | "try_run" => {
            for kw in [
                "OUTPUT_VARIABLE",
                "ERROR_VARIABLE",
                "RESULT_VARIABLE",
                "RESULTS_VARIABLE",
                "COMPILE_RESULT_VAR",
                "RUN_RESULT_VAR",
            ] {
                let mut it = args.iter();
                while let Some(a) = it.next() {
                    if a == kw {
                        if let Some(v) = it.next() {
                            write(v, None, WriteKind::Set);
                        }
                    }
                }
            }
        }
        "file" => match arg(0) {
            "READ" | "STRINGS" | "SIZE" | "TIMESTAMP" | "MD5" | "SHA1" | "SHA224" | "SHA256"
            | "SHA384" | "SHA512" | "TO_CMAKE_PATH" | "TO_NATIVE_PATH" | "REAL_PATH" => {
                write(arg(2), None, WriteKind::Set)
            }
            "GLOB" | "GLOB_RECURSE" | "RELATIVE_PATH" => write(arg(1), None, WriteKind::Set),
            _ => {}
        },
        "cmake_parse_arguments" => {
            // (prefix opts one multi args...) or (PARSE_ARGV n prefix opts one multi)
            let (prefix, lists) = if arg(0) == "PARSE_ARGV" {
                (arg(2).to_string(), [arg(3), arg(4), arg(5)])
            } else {
                (arg(0).to_string(), [arg(1), arg(2), arg(3)])
            };
            for list in lists {
                for kw in list.split(';').filter(|s| !s.is_empty()) {
                    write(&format!("{prefix}_{kw}"), None, WriteKind::Set);
                }
            }
            write(
                &format!("{prefix}_UNPARSED_ARGUMENTS"),
                None,
                WriteKind::Set,
            );
            write(
                &format!("{prefix}_KEYWORDS_MISSING_VALUES"),
                None,
                WriteKind::Set,
            );
        }
        "project" => {
            // project() defines PROJECT_NAME & friends; record the main ones
            // so reads of them resolve.
            let name = arg(0);
            write("PROJECT_NAME", Some(name.to_string()), WriteKind::Set);
            write(
                "CMAKE_PROJECT_NAME",
                Some(name.to_string()),
                WriteKind::Cache,
            );
            for suffix in ["SOURCE_DIR", "BINARY_DIR", "VERSION"] {
                write(&format!("PROJECT_{suffix}"), None, WriteKind::Set);
                write(&format!("{name}_{suffix}"), None, WriteKind::Set);
            }
        }
        _ => {}
    }
    out
}

// --- Condition reads -------------------------------------------------------

/// Extract variable reads from a raw if/elseif/while argument list.
/// Returns (name, force) — forced reads are recorded even when the variable
/// is undefined (`DEFINED X`, `x IN_LIST L`). Also used at file-indexing
/// time to record condition identifiers as static references.
pub(crate) fn condition_reads<'a>(
    args: impl Iterator<Item = (ArgKind, &'a str)>,
) -> Vec<(String, bool)> {
    const OPERATORS: &[&str] = &[
        "AND",
        "OR",
        "NOT",
        "EQUAL",
        "LESS",
        "GREATER",
        "LESS_EQUAL",
        "GREATER_EQUAL",
        "STREQUAL",
        "STRLESS",
        "STRGREATER",
        "STRLESS_EQUAL",
        "STRGREATER_EQUAL",
        "VERSION_EQUAL",
        "VERSION_LESS",
        "VERSION_GREATER",
        "VERSION_LESS_EQUAL",
        "VERSION_GREATER_EQUAL",
        "PATH_EQUAL",
        "MATCHES",
        "IN_LIST",
        "DEFINED",
        "COMMAND",
        "POLICY",
        "TARGET",
        "TEST",
        "EXISTS",
        "IS_NEWER_THAN",
        "IS_DIRECTORY",
        "IS_SYMLINK",
        "IS_ABSOLUTE",
        "IS_READABLE",
        "IS_WRITABLE",
        "IS_EXECUTABLE",
    ];
    const CONSTANTS: &[&str] = &[
        "ON", "OFF", "YES", "NO", "TRUE", "FALSE", "Y", "N", "IGNORE", "NOTFOUND",
    ];
    // Operands of these keywords are not variable names.
    const SKIP_NEXT: &[&str] = &[
        "COMMAND",
        "POLICY",
        "TARGET",
        "TEST",
        "EXISTS",
        "IS_DIRECTORY",
        "IS_SYMLINK",
        "IS_ABSOLUTE",
        "IS_READABLE",
        "IS_WRITABLE",
        "IS_EXECUTABLE",
        "MATCHES",
    ];
    let mut out = Vec::new();
    let mut skip = false;
    let mut force_next = false;
    for (kind, value) in args {
        let is_kw = OPERATORS.contains(&value);
        if skip && !is_kw {
            skip = false;
            continue;
        }
        skip = false;
        if is_kw {
            skip = SKIP_NEXT.contains(&value);
            force_next = value == "DEFINED" || value == "IN_LIST";
            continue;
        }
        if kind != ArgKind::Unquoted {
            force_next = false;
            continue;
        }
        if CONSTANTS.contains(&value)
            || value.contains('$')
            || value.contains('"')
            || value.ends_with("-NOTFOUND")
        {
            force_next = false;
            continue;
        }
        // Identifier-like?
        let mut chars = value.chars();
        let first_ok = chars
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false);
        let rest_ok = value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c));
        if first_ok && rest_ok {
            if value.starts_with("ENV{") || value.starts_with("CACHE{") {
                force_next = false;
                continue;
            }
            out.push((value.to_string(), force_next));
        }
        force_next = false;
    }
    out
}

// --- Build-graph collection ------------------------------------------------

fn collect_graph(cmd: &str, args: &[String], eid: i64, graph: &mut CollectedGraph) {
    let arg = |i: usize| args.get(i).map(|s| s.as_str()).unwrap_or("");
    match cmd {
        "add_library" | "add_executable" => {
            let name = arg(0).to_string();
            if name.is_empty() {
                return;
            }
            if arg(1) == "ALIAS" {
                graph.targets.push(TraceTarget {
                    name,
                    ttype: Some("ALIAS".into()),
                    imported: false,
                    alias_of: Some(arg(2).to_string()),
                    event_id: eid,
                });
                return;
            }
            let imported = args.iter().any(|a| a == "IMPORTED");
            let ttype = if cmd == "add_executable" {
                Some("EXECUTABLE".to_string())
            } else {
                [
                    "STATIC",
                    "SHARED",
                    "MODULE",
                    "OBJECT",
                    "INTERFACE",
                    "UNKNOWN",
                ]
                .iter()
                .find(|k| args.iter().any(|a| a == *k))
                .map(|k| format!("{k}_LIBRARY"))
            };
            graph.targets.push(TraceTarget {
                name,
                ttype,
                imported,
                alias_of: None,
                event_id: eid,
            });
        }
        "add_custom_target" => {
            if !arg(0).is_empty() {
                graph.targets.push(TraceTarget {
                    name: arg(0).into(),
                    ttype: Some("UTILITY".into()),
                    imported: false,
                    alias_of: None,
                    event_id: eid,
                });
            }
        }
        "target_link_libraries" => {
            let src = arg(0).to_string();
            let mut vis = "PUBLIC".to_string(); // legacy plain signature
                                                // Expanded list variables arrive as single ';'-joined arguments
                                                // (e.g. LLVM's `target_link_libraries(t PRIVATE "A;B;C")`).
                                                // Known gap: a ';' inside a $<...> genex splits incorrectly.
            for a in args[1.min(args.len())..].iter().flat_map(|a| a.split(';')) {
                match a {
                    "PUBLIC" | "LINK_PUBLIC" => vis = "PUBLIC".into(),
                    "PRIVATE" | "LINK_PRIVATE" => vis = "PRIVATE".into(),
                    "INTERFACE" | "LINK_INTERFACE_LIBRARIES" => vis = "INTERFACE".into(),
                    "debug" | "optimized" | "general" => {}
                    lib if !lib.is_empty() => graph.edges.push(TraceEdge {
                        src: src.clone(),
                        dst: lib.to_string(),
                        visibility: vis.clone(),
                        event_id: eid,
                    }),
                    _ => {}
                }
            }
        }
        "target_include_directories"
        | "target_compile_definitions"
        | "target_compile_options"
        | "target_compile_features"
        | "target_link_options" => {
            let kind: &'static str = match cmd {
                "target_include_directories" => "include",
                "target_compile_definitions" => "define",
                "target_compile_options" => "option",
                "target_compile_features" => "feature",
                _ => "link_option",
            };
            let tgt = arg(0).to_string();
            let mut vis: Option<String> = None;
            for a in args[1.min(args.len())..].iter().flat_map(|a| a.split(';')) {
                match a {
                    "PUBLIC" | "PRIVATE" | "INTERFACE" => vis = Some(a.to_string()),
                    "SYSTEM" | "BEFORE" | "AFTER" => {}
                    v if !v.is_empty() => {
                        let value = if kind == "define" {
                            v.trim_start_matches("-D").to_string()
                        } else {
                            v.to_string()
                        };
                        graph.reqs.push((
                            tgt.clone(),
                            kind,
                            value,
                            vis.clone().unwrap_or_else(|| "PRIVATE".into()),
                            eid,
                        ));
                    }
                    _ => {}
                }
            }
        }
        "set_target_properties" => {
            let Some(pos) = args.iter().position(|a| a == "PROPERTIES") else {
                return;
            };
            let tgts = &args[..pos];
            let mut i = pos + 1;
            while i + 1 < args.len() {
                let (prop, value) = (&args[i], &args[i + 1]);
                for t in tgts {
                    record_prop(graph, t, prop, value, false, eid);
                }
                i += 2;
            }
        }
        "set_property" => {
            if arg(0) != "TARGET" {
                return;
            }
            let Some(ppos) = args.iter().position(|a| a == "PROPERTY") else {
                return;
            };
            let appended = args[..ppos]
                .iter()
                .any(|a| a == "APPEND" || a == "APPEND_STRING");
            let tgts: Vec<&String> = args[1..ppos]
                .iter()
                .filter(|a| *a != "APPEND" && *a != "APPEND_STRING")
                .collect();
            let Some(prop) = args.get(ppos + 1) else {
                return;
            };
            let value = args[ppos + 2..].join(";");
            for t in tgts {
                record_prop(graph, t, prop, &value, appended, eid);
            }
        }
        _ => {}
    }
}

fn record_prop(
    graph: &mut CollectedGraph,
    target: &str,
    prop: &str,
    value: &str,
    appended: bool,
    eid: i64,
) {
    graph.props.push((
        target.into(),
        prop.into(),
        Some(value.into()),
        appended,
        eid,
    ));
    match prop {
        "INTERFACE_LINK_LIBRARIES" => {
            for lib in value.split(';').filter(|s| !s.is_empty()) {
                graph.edges.push(TraceEdge {
                    src: target.into(),
                    dst: lib.into(),
                    visibility: "INTERFACE".into(),
                    event_id: eid,
                });
            }
        }
        "INTERFACE_INCLUDE_DIRECTORIES" => {
            for d in value.split(';').filter(|s| !s.is_empty()) {
                graph
                    .reqs
                    .push((target.into(), "include", d.into(), "INTERFACE".into(), eid));
            }
        }
        "INTERFACE_COMPILE_DEFINITIONS" => {
            for d in value.split(';').filter(|s| !s.is_empty()) {
                graph
                    .reqs
                    .push((target.into(), "define", d.into(), "INTERFACE".into(), eid));
            }
        }
        "INTERFACE_COMPILE_OPTIONS" => {
            for d in value.split(';').filter(|s| !s.is_empty()) {
                graph
                    .reqs
                    .push((target.into(), "option", d.into(), "INTERFACE".into(), eid));
            }
        }
        _ => {}
    }
}

/// Insert edges/props/reqs collected from the trace, resolving target names
/// (through aliases) to target ids. Runs after File API ingestion so all
/// targets exist.
pub fn finalize_graph(
    tx: &Transaction,
    graph: &CollectedGraph,
    stats: &mut IngestStats,
) -> Result<()> {
    // name -> (id, alias_of)
    let mut by_name: HashMap<String, (i64, Option<String>)> = HashMap::new();
    {
        let mut stmt = tx.prepare("SELECT name, id, alias_of FROM targets")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?),
            ))
        })?;
        for r in rows {
            let (name, v) = r?;
            by_name.insert(name, v);
        }
    }
    let resolve = |name: &str| -> Option<i64> {
        let mut cur = name.to_string();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(cur.clone()) {
                return None;
            }
            match by_name.get(&cur) {
                Some((_, Some(alias))) => cur = alias.clone(),
                Some((id, None)) => return Some(*id),
                None => return None,
            }
        }
    };

    let mut ins_edge = tx.prepare(
        "INSERT INTO tgt_edges(src_target, dst, dst_target, visibility, origin_event)
         VALUES (?1,?2,?3,?4,?5)",
    )?;
    for e in &graph.edges {
        let Some(src_id) = resolve(&e.src) else {
            continue;
        };
        let dst_id = resolve(&e.dst);
        ins_edge.execute(params![src_id, e.dst, dst_id, e.visibility, e.event_id])?;
        stats.edges += 1;
    }

    let mut ins_prop = tx.prepare(
        "INSERT INTO tgt_props(target_id, prop, value, appended, origin_event)
         VALUES (?1,?2,?3,?4,?5)",
    )?;
    for (t, prop, value, appended, eid) in &graph.props {
        if let Some(tid) = resolve(t) {
            ins_prop.execute(params![tid, prop, value, *appended as i64, eid])?;
        }
    }

    let mut ins_req = tx.prepare(
        "INSERT INTO usage_reqs(target_id, kind, value, visibility, source, origin_event)
         VALUES (?1,?2,?3,?4,'trace',?5)",
    )?;
    for (t, kind, value, vis, eid) in &graph.reqs {
        if let Some(tid) = resolve(t) {
            ins_req.execute(params![tid, kind, value, vis, eid])?;
        }
    }

    stats.targets =
        tx.query_row("SELECT count(*) FROM targets", [], |r| r.get::<_, i64>(0))? as u64;
    Ok(())
}

fn truncate_for_error(s: &str) -> String {
    match s.char_indices().nth(80) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}
