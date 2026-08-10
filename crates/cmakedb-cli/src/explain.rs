//! `cmakedb explain <file:line>`: what did this line do? (ROADMAP
//! Track A.) One command for the most common debugging motion: every
//! recorded evaluation of the command at a source location with its
//! EXPANDED argument values, the call chains it ran under, the effects
//! it produced (variable writes, link edges, usage requirements, target
//! definitions, property writes), and one hop of influence (which reads
//! resolved to its writes, File API evidence for its edges).
//!
//! If the location has AST commands but zero events, the line never
//! executed — the report then shows static context only (the raw
//! command and its enclosing conditions/loops/definitions), clearly
//! labeled as such.

use anyhow::{bail, Result};
use cmakedb_db::{cmake_path_spelling, Db};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

/// Rows shown per list in text output (JSON carries the same truncated
/// lists; the `distinct_*`/count fields are always exact totals).
const MAX_ARG_SETS: usize = 5;
const MAX_CONTEXTS: usize = 3;
const MAX_VALUES: usize = 3;
const MAX_READ_SITES: usize = 3;
const MAX_EVIDENCE: usize = 3;
/// Effect groups rendered in text before eliding (JSON keeps all).
const MAX_TEXT_GROUPS: usize = 20;

#[derive(Debug, Serialize)]
pub struct Explain {
    pub schema_version: u32,
    /// Resolved display location (`file:line`, source-relative when possible).
    pub location: String,
    /// Set when the requested line fell inside a multi-line command and
    /// was remapped to the command's starting line.
    pub note: Option<String>,
    /// Distinct command names evaluated at the line (normally one).
    pub commands: Vec<String>,
    pub evaluations: i64,
    pub distinct_arg_sets: i64,
    /// Most frequent expanded argument sets (loop iterations collapse).
    pub arg_sets: Vec<ArgSet>,
    pub distinct_contexts: i64,
    /// Most frequent call chains (innermost first) the line ran under.
    pub contexts: Vec<EvalContext>,
    pub writes: Vec<WriteEffect>,
    pub edges: Vec<EdgeEffect>,
    pub requirements: Vec<ReqEffect>,
    pub targets_defined: Vec<TargetDef>,
    pub properties: Vec<PropEffect>,
    /// Present iff the line has AST commands but zero recorded events.
    pub never_executed: Option<NeverExecuted>,
}

#[derive(Debug, Serialize)]
pub struct ArgSet {
    pub count: i64,
    /// Expanded argument values as the command actually received them.
    pub args: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EvalContext {
    pub count: i64,
    /// `kind name (call site)` hops, innermost first; empty = top level.
    pub call_chain: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ValueCount {
    pub value: Option<String>,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct WriteEffect {
    pub name: String,
    pub write_kind: String,
    /// Scope kinds the writes landed in (usually one).
    pub scope_kinds: Vec<String>,
    pub writes: i64,
    pub distinct_values: i64,
    /// Most frequent written values.
    pub values: Vec<ValueCount>,
    /// Reads (anywhere) whose dominating write is one produced here.
    pub resolved_reads: i64,
    /// Locations of the first few such reads.
    pub first_reads: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EdgeEffect {
    pub src: String,
    pub dst: String,
    pub visibility: String,
    pub count: i64,
    /// Final resolved link fragments from the File API mentioning `dst`.
    pub final_link_evidence: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ReqEffect {
    pub target: String,
    pub kind: String,
    pub visibility: Option<String>,
    pub count: i64,
    pub distinct_values: i64,
    pub values: Vec<ValueCount>,
}

#[derive(Debug, Serialize)]
pub struct TargetDef {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PropEffect {
    pub target: String,
    pub prop: String,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct NeverExecuted {
    pub commands: Vec<StaticCommand>,
    /// Whether ANY command in the file executed (false = the file itself
    /// was never included/added).
    pub file_ever_executed: bool,
}

#[derive(Debug, Serialize)]
pub struct StaticCommand {
    pub name: String,
    /// Raw (unexpanded) argument text from the AST.
    pub args_text: String,
    /// Enclosing conditions/loops/definitions, innermost first, e.g.
    /// `if(BUILD_LEGACY_DRIVER) at CMakeLists.txt:12`.
    pub enclosing: Vec<String>,
}

pub fn explain(db: &Db, file: &str, line: i64) -> Result<Explain> {
    let (file_id, path) = resolve_file(db, file)?;
    let mut line = line;
    let mut note = None;

    // Multi-line commands: the trace (and the AST command node) sit at
    // the command's first line. If the requested line is inside a
    // command's span, remap to its start.
    let mut ast_cmds = ast_commands_at(db, file_id, line)?;
    if ast_cmds.is_empty() {
        if let Some(start) = command_line_containing(db, file_id, line)? {
            if start != line {
                note = Some(format!(
                    "line {line} is inside the command starting at line {start}; \
                     explaining line {start}"
                ));
                line = start;
                ast_cmds = ast_commands_at(db, file_id, line)?;
            }
        }
    }

    let display_loc = format!("{}:{}", db.display_path(&path), line);

    // Every event at the location, in execution order.
    let mut stmt = db.conn.prepare(
        "SELECT id, cmd, args_json, scope_id FROM events
         WHERE file_id = ?1 AND line = ?2 ORDER BY id",
    )?;
    let events: Vec<(i64, String, String, Option<i64>)> = stmt
        .query_map([file_id, line], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    if events.is_empty() {
        if ast_cmds.is_empty() {
            bail!("no CMake command at {display_loc} (in the AST or the recording)");
        }
        return never_executed(db, file_id, display_loc, note, ast_cmds);
    }

    // Distinct command names, in first-seen order.
    let mut commands: Vec<String> = Vec::new();
    for (_, cmd, _, _) in &events {
        if !commands.contains(cmd) {
            commands.push(cmd.clone());
        }
    }

    // Distinct expanded argument sets with counts (loop iterations
    // collapse into one row per distinct set).
    let mut set_counts: Vec<(Vec<String>, i64)> = Vec::new();
    let mut set_index: HashMap<String, usize> = HashMap::new();
    for (_, _, args_json, _) in &events {
        match set_index.get(args_json.as_str()) {
            Some(&i) => set_counts[i].1 += 1,
            None => {
                let args: Vec<String> = serde_json::from_str(args_json).unwrap_or_default();
                set_index.insert(args_json.clone(), set_counts.len());
                set_counts.push((args, 1));
            }
        }
    }
    let distinct_arg_sets = set_counts.len() as i64;
    set_counts.sort_by_key(|x| std::cmp::Reverse(x.1));
    let arg_sets = set_counts
        .into_iter()
        .take(MAX_ARG_SETS)
        .map(|(args, count)| ArgSet { count, args })
        .collect();

    // Call chains: group events by scope, render each scope's chain
    // (memoized — a hot line can run under thousands of scopes), then
    // aggregate by identical chain.
    let mut scope_counts: BTreeMap<Option<i64>, i64> = BTreeMap::new();
    for (_, _, _, sid) in &events {
        *scope_counts.entry(*sid).or_insert(0) += 1;
    }
    let mut memo: HashMap<i64, Vec<String>> = HashMap::new();
    let mut chain_counts: BTreeMap<Vec<String>, i64> = BTreeMap::new();
    for (sid, n) in scope_counts {
        let chain = match sid {
            Some(s) => call_chain(db, s, &mut memo),
            None => Vec::new(),
        };
        *chain_counts.entry(chain).or_insert(0) += n;
    }
    let distinct_contexts = chain_counts.len() as i64;
    let mut chains: Vec<(Vec<String>, i64)> = chain_counts.into_iter().collect();
    chains.sort_by_key(|x| std::cmp::Reverse(x.1));
    let contexts = chains
        .into_iter()
        .take(MAX_CONTEXTS)
        .map(|(call_chain, count)| EvalContext { count, call_chain })
        .collect();

    Ok(Explain {
        schema_version: 1,
        location: display_loc,
        note,
        commands,
        evaluations: events.len() as i64,
        distinct_arg_sets,
        arg_sets,
        distinct_contexts,
        contexts,
        writes: write_effects(db, file_id, line)?,
        edges: edge_effects(db, file_id, line)?,
        requirements: req_effects(db, file_id, line)?,
        targets_defined: target_defs(db, file_id, line)?,
        properties: prop_effects(db, file_id, line)?,
        never_executed: None,
    })
}

// --- file resolution ------------------------------------------------------

/// Resolve user input (absolute or source-relative) to a `files` row,
/// with the recording's own path spelling (see AGENTS.md: never
/// canonicalize paths that join against trace data; LSP `db_path_for`
/// precedent).
fn resolve_file(db: &Db, input: &str) -> Result<(i64, String)> {
    let spelled = cmake_path_spelling(input);
    let lookup = |p: &str| -> Option<(i64, String)> {
        db.conn
            .query_row("SELECT id, path FROM files WHERE path = ?1", [p], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .ok()
    };
    if let Some(hit) = lookup(&spelled) {
        return Ok(hit);
    }
    // Source-relative: join against the recorded source dir.
    let rel = spelled.trim_start_matches("./");
    if let Ok(Some(src)) = db.get_meta("source_dir") {
        if let Some(hit) = lookup(&format!("{}/{}", src.trim_end_matches('/'), rel)) {
            return Ok(hit);
        }
    }
    // Suffix match anywhere in the recording (unique or bail).
    let mut stmt = db
        .conn
        .prepare("SELECT id, path FROM files WHERE path LIKE '%/' || ?1")?;
    let hits: Vec<(i64, String)> = stmt
        .query_map([rel], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if hits.len() == 1 {
        return Ok(hits.into_iter().next().unwrap());
    }
    if hits.len() > 1 {
        bail!(
            "'{input}' is ambiguous in this recording — candidates:\n  {}",
            hits.iter()
                .take(5)
                .map(|(_, p)| db.display_path(p))
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
    // Last resort: the editor path may spell a symlink differently than
    // cmake did (/var vs /private/var on macOS) — compare canonicalized.
    if let Ok(canon) = std::path::Path::new(input).canonicalize() {
        let mut stmt = db.conn.prepare("SELECT id, path FROM files")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, p) in rows {
            if std::path::Path::new(&p)
                .canonicalize()
                .map(|c| c == canon)
                .unwrap_or(false)
            {
                return Ok((id, p));
            }
        }
    }
    bail!("no file matching '{input}' in this recording (try a source-relative or absolute path)");
}

// --- AST helpers ----------------------------------------------------------

struct AstCmd {
    node_id: i64,
    byte_start: i64,
    name: String,
    args_text: String,
}

fn ast_commands_at(db: &Db, file_id: i64, line: i64) -> Result<Vec<AstCmd>> {
    let mut stmt = db.conn.prepare(
        "SELECT n.id, n.byte_start, c.name, c.args_text
         FROM ast_nodes n JOIN commands c ON c.node_id = n.id
         WHERE n.file_id = ?1 AND n.line = ?2 ORDER BY n.byte_start",
    )?;
    let rows = stmt
        .query_map([file_id, line], |r| {
            Ok(AstCmd {
                node_id: r.get(0)?,
                byte_start: r.get(1)?,
                name: r.get(2)?,
                args_text: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// If `line` falls inside a multi-line command's span, return the
/// command's starting line (needs the recorded file content to convert
/// the line to a byte offset).
fn command_line_containing(db: &Db, file_id: i64, line: i64) -> Result<Option<i64>> {
    let content: Option<String> = db
        .conn
        .query_row("SELECT content FROM files WHERE id = ?1", [file_id], |r| {
            r.get(0)
        })
        .ok()
        .flatten();
    let Some(content) = content else {
        return Ok(None);
    };
    let mut off = 0usize;
    let mut cur = 1i64;
    for l in content.split_inclusive('\n') {
        if cur == line {
            break;
        }
        off += l.len();
        cur += 1;
    }
    if cur != line {
        return Ok(None);
    }
    Ok(db
        .conn
        .query_row(
            "SELECT n.line FROM ast_nodes n JOIN commands c ON c.node_id = n.id
             WHERE n.file_id = ?1 AND n.byte_start <= ?2 AND n.byte_end > ?2
             ORDER BY n.byte_start DESC LIMIT 1",
            [file_id, off as i64],
            |r| r.get(0),
        )
        .ok())
}

// --- never-executed static context ---------------------------------------

fn never_executed(
    db: &Db,
    file_id: i64,
    location: String,
    note: Option<String>,
    ast_cmds: Vec<AstCmd>,
) -> Result<Explain> {
    let file_ever_executed: i64 = db.conn.query_row(
        "SELECT count(*) FROM events WHERE file_id = ?1",
        [file_id],
        |r| r.get(0),
    )?;
    let mut commands = Vec::new();
    let mut static_cmds = Vec::new();
    for c in ast_cmds {
        if !commands.contains(&c.name) {
            commands.push(c.name.clone());
        }
        static_cmds.push(StaticCommand {
            enclosing: enclosing_context(db, c.node_id, c.byte_start),
            name: c.name,
            args_text: c.args_text,
        });
    }
    Ok(Explain {
        schema_version: 1,
        location,
        note,
        commands,
        evaluations: 0,
        distinct_arg_sets: 0,
        arg_sets: vec![],
        distinct_contexts: 0,
        contexts: vec![],
        writes: vec![],
        edges: vec![],
        requirements: vec![],
        targets_defined: vec![],
        properties: vec![],
        never_executed: Some(NeverExecuted {
            commands: static_cmds,
            file_ever_executed: file_ever_executed > 0,
        }),
    })
}

/// Walk AST ancestors of a command node collecting the headers of every
/// enclosing condition/loop/definition, innermost first: a static
/// "why-not" hint for never-executed lines.
fn enclosing_context(db: &Db, node_id: i64, byte_start: i64) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Some(node_id);
    for _ in 0..64 {
        let Some(id) = cur else { break };
        let row: Option<(String, Option<i64>)> = db
            .conn
            .query_row(
                "SELECT kind, parent_id FROM ast_nodes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((kind, parent)) = row else { break };
        if matches!(
            kind.as_str(),
            "if_condition"
                | "foreach_loop"
                | "while_loop"
                | "function_def"
                | "macro_def"
                | "block_def"
        ) {
            // The branch header governing our node: the last
            // if/elseif/else (or loop/def opener) starting before it.
            let mut stmt = match db.conn.prepare(
                "SELECT c.name, c.args_text, f.path, n.line, n.byte_start
                 FROM ast_nodes n
                 JOIN commands c ON c.node_id = n.id
                 JOIN files f ON f.id = n.file_id
                 WHERE n.parent_id = ?1
                   AND c.name_lower IN ('if','elseif','else','foreach','while',
                                        'function','macro','block')
                 ORDER BY n.byte_start",
            ) {
                Ok(s) => s,
                Err(_) => break,
            };
            // (command name, args text, file path, line, byte_start)
            type HeaderRow = (String, String, String, i64, i64);
            let headers: Vec<HeaderRow> = stmt
                .query_map([id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .and_then(|m| m.collect())
                .unwrap_or_default();
            // Strictly-before: an if/foreach/... command must not report
            // its own header as "enclosing" context.
            if let Some((name, args, path, line, _)) =
                headers.into_iter().take_while(|h| h.4 < byte_start).last()
            {
                out.push(format!(
                    "inside {name}({}) at {}:{line}",
                    truncate(args.trim(), 100),
                    db.display_path(&path)
                ));
            }
        }
        cur = parent;
    }
    out
}

// --- call chains ----------------------------------------------------------

/// `kind name (call site)` labels from a scope to the root, innermost
/// first (same shape as history.rs why_value). Memoized per scope id.
fn call_chain(db: &Db, scope_id: i64, memo: &mut HashMap<i64, Vec<String>>) -> Vec<String> {
    if let Some(c) = memo.get(&scope_id) {
        return c.clone();
    }
    // (kind, name, opened_by_event, parent_id)
    type ScopeRow = (String, Option<String>, Option<i64>, Option<i64>);
    let row: Option<ScopeRow> = db
        .conn
        .query_row(
            "SELECT kind, name, opened_by_event, parent_id FROM scopes WHERE id = ?1",
            [scope_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok();
    let chain = match row {
        None => Vec::new(),
        Some((kind, _, _, _)) if kind == "root" => Vec::new(),
        Some((kind, name, opened_by, parent)) => {
            let site = opened_by
                .and_then(|e| db.event_location(e).ok())
                .unwrap_or_default();
            let label = match name {
                Some(n) if !n.is_empty() => format!("{kind} {} ({site})", db.display_path(&n)),
                _ => format!("{kind} ({site})"),
            };
            let mut chain = vec![label];
            if memo.len() < 100_000 {
                if let Some(p) = parent {
                    if p != scope_id {
                        chain.extend(call_chain(db, p, memo));
                    }
                }
            }
            chain
        }
    };
    memo.insert(scope_id, chain.clone());
    chain
}

// --- effects --------------------------------------------------------------

const EVENTS_AT: &str = "SELECT id FROM events WHERE file_id = ?1 AND line = ?2";

fn write_effects(db: &Db, file_id: i64, line: i64) -> Result<Vec<WriteEffect>> {
    let mut stmt = db.conn.prepare(&format!(
        "SELECT w.name, w.write_kind, coalesce(s.kind, 'cache'), w.value
         FROM var_writes w LEFT JOIN scopes s ON s.id = w.scope_id
         WHERE w.event_id IN ({EVENTS_AT}) ORDER BY w.id"
    ))?;
    let rows: Vec<(String, String, String, Option<String>)> = stmt
        .query_map([file_id, line], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    // Group by (name, write_kind), preserving first-write order.
    struct Group {
        scope_kinds: Vec<String>,
        writes: i64,
        values: Vec<(Option<String>, i64)>,
    }
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), Group> = HashMap::new();
    for (name, wkind, skind, value) in rows {
        let key = (name, wkind);
        let g = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Group {
                scope_kinds: vec![],
                writes: 0,
                values: vec![],
            }
        });
        g.writes += 1;
        if !g.scope_kinds.contains(&skind) {
            g.scope_kinds.push(skind);
        }
        match g.values.iter_mut().find(|(v, _)| *v == value) {
            Some((_, c)) => *c += 1,
            None => g.values.push((value, 1)),
        }
    }

    let mut out = Vec::with_capacity(order.len());
    for (name, write_kind) in order {
        let g = groups.remove(&(name.clone(), write_kind.clone())).unwrap();
        // Influence: reads whose dominating write is one produced here.
        let resolved_reads: i64 = db.conn.query_row(
            &format!(
                "SELECT count(*) FROM var_reads r
                 JOIN var_writes w ON w.id = r.resolved_write_id
                 WHERE w.name = ?3 AND w.write_kind = ?4
                   AND w.event_id IN ({EVENTS_AT})"
            ),
            rusqlite::params![file_id, line, name, write_kind],
            |r| r.get(0),
        )?;
        let mut stmt = db.conn.prepare(&format!(
            "SELECT r.event_id FROM var_reads r
             JOIN var_writes w ON w.id = r.resolved_write_id
             WHERE w.name = ?3 AND w.write_kind = ?4
               AND w.event_id IN ({EVENTS_AT})
             ORDER BY r.id LIMIT {MAX_READ_SITES}"
        ))?;
        let read_events: Vec<i64> = stmt
            .query_map(rusqlite::params![file_id, line, name, write_kind], |r| {
                r.get(0)
            })?
            .collect::<rusqlite::Result<_>>()?;
        let first_reads = read_events
            .into_iter()
            .filter_map(|e| db.event_location(e).ok())
            .collect();

        let distinct_values = g.values.len() as i64;
        let mut values = g.values;
        values.sort_by_key(|x| std::cmp::Reverse(x.1));
        out.push(WriteEffect {
            name,
            write_kind,
            scope_kinds: g.scope_kinds,
            writes: g.writes,
            distinct_values,
            values: values
                .into_iter()
                .take(MAX_VALUES)
                .map(|(value, count)| ValueCount { value, count })
                .collect(),
            resolved_reads,
            first_reads,
        });
    }
    // Most influential first; ties keep first-write order (stable sort).
    out.sort_by_key(|x| std::cmp::Reverse(x.resolved_reads));
    Ok(out)
}

fn edge_effects(db: &Db, file_id: i64, line: i64) -> Result<Vec<EdgeEffect>> {
    let mut stmt = db.conn.prepare(&format!(
        "SELECT e.src_target, t.name, e.dst, e.visibility, count(*)
         FROM tgt_edges e JOIN targets t ON t.id = e.src_target
         WHERE e.origin_event IN ({EVENTS_AT})
         GROUP BY e.src_target, e.dst, e.visibility ORDER BY min(e.id)"
    ))?;
    let rows: Vec<(i64, String, String, String, i64)> = stmt
        .query_map([file_id, line], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (src_id, src, dst, visibility, count) in rows {
        let mut stmt = db.conn.prepare(&format!(
            "SELECT value FROM usage_reqs
             WHERE target_id = ?1 AND kind = 'link' AND source = 'fileapi'
               AND value LIKE '%' || ?2 || '%'
             LIMIT {MAX_EVIDENCE}"
        ))?;
        let final_link_evidence: Vec<String> = stmt
            .query_map(rusqlite::params![src_id, dst], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        out.push(EdgeEffect {
            src,
            dst,
            visibility,
            count,
            final_link_evidence,
        });
    }
    Ok(out)
}

fn req_effects(db: &Db, file_id: i64, line: i64) -> Result<Vec<ReqEffect>> {
    let mut stmt = db.conn.prepare(&format!(
        "SELECT t.name, r.kind, r.visibility, r.value
         FROM usage_reqs r JOIN targets t ON t.id = r.target_id
         WHERE r.source = 'trace' AND r.origin_event IN ({EVENTS_AT})
         ORDER BY r.id"
    ))?;
    let rows: Vec<(String, String, Option<String>, String)> = stmt
        .query_map([file_id, line], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    // (target, kind, visibility) -> distinct values with counts.
    type ReqKey = (String, String, Option<String>);
    let mut order: Vec<ReqKey> = Vec::new();
    let mut groups: HashMap<ReqKey, Vec<(Option<String>, i64)>> = HashMap::new();
    for (target, kind, vis, value) in rows {
        let key = (target, kind, vis);
        let g = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            Vec::new()
        });
        let value = Some(value);
        match g.iter_mut().find(|(v, _)| *v == value) {
            Some((_, c)) => *c += 1,
            None => g.push((value, 1)),
        }
    }
    Ok(order
        .into_iter()
        .map(|key| {
            let mut values = groups.remove(&key).unwrap();
            let count: i64 = values.iter().map(|(_, c)| c).sum();
            let distinct_values = values.len() as i64;
            values.sort_by_key(|x| std::cmp::Reverse(x.1));
            ReqEffect {
                target: key.0,
                kind: key.1,
                visibility: key.2,
                count,
                distinct_values,
                values: values
                    .into_iter()
                    .take(MAX_VALUES)
                    .map(|(value, count)| ValueCount { value, count })
                    .collect(),
            }
        })
        .collect())
}

fn target_defs(db: &Db, file_id: i64, line: i64) -> Result<Vec<TargetDef>> {
    let mut stmt = db.conn.prepare(&format!(
        "SELECT name, type FROM targets WHERE defined_event IN ({EVENTS_AT}) ORDER BY id"
    ))?;
    let rows = stmt
        .query_map([file_id, line], |r| {
            Ok(TargetDef {
                name: r.get(0)?,
                type_: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn prop_effects(db: &Db, file_id: i64, line: i64) -> Result<Vec<PropEffect>> {
    let mut stmt = db.conn.prepare(&format!(
        "SELECT t.name, p.prop, count(*)
         FROM tgt_props p JOIN targets t ON t.id = p.target_id
         WHERE p.origin_event IN ({EVENTS_AT})
         GROUP BY t.name, p.prop ORDER BY min(p.id)"
    ))?;
    let rows = stmt
        .query_map([file_id, line], |r| {
            Ok(PropEffect {
                target: r.get(0)?,
                prop: r.get(1)?,
                count: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

// --- text rendering -------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Render an expanded call: args quoted only when needed, long args and
/// long lines elided (JSON carries full values).
fn fmt_call(cmd: &str, args: &[String]) -> String {
    let rendered: Vec<String> = args
        .iter()
        .map(|a| {
            let t = truncate(a, 80);
            if t.is_empty() || t.contains(|c: char| c.is_whitespace() || c == ';' || c == '"') {
                format!("\"{}\"", t.replace('"', "\\\""))
            } else {
                t
            }
        })
        .collect();
    truncate(&format!("{cmd}({})", rendered.join(" ")), 220)
}

fn fmt_value(v: &Option<String>) -> String {
    match v {
        Some(s) => format!("\"{}\"", truncate(s, 120)),
        None => "<not captured>".into(),
    }
}

fn plural(n: i64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

pub fn render_text(x: &Explain) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let cmds = x.commands.join(", ");
    let _ = writeln!(out, "explain {} — {}", x.location, cmds);
    if let Some(n) = &x.note {
        let _ = writeln!(out, "note: {n}");
    }

    if let Some(ne) = &x.never_executed {
        let _ = writeln!(
            out,
            "\nthis line NEVER EXECUTED during the recorded configure \
             (commands exist in the AST, but no evaluation was recorded)."
        );
        let _ = writeln!(
            out,
            "\nstatic context (from the AST, not runtime evidence):"
        );
        for c in &ne.commands {
            let _ = writeln!(out, "  {}({})", c.name, truncate(c.args_text.trim(), 160));
            for e in &c.enclosing {
                let _ = writeln!(out, "    {e}");
            }
        }
        if !ne.file_ever_executed {
            let _ = writeln!(
                out,
                "  no command in this file executed at all — the file was never \
                 included or added in this configuration."
            );
        }
        return out;
    }

    let _ = writeln!(
        out,
        "evaluated {} ({})",
        plural(x.evaluations, "time", "times"),
        plural(
            x.distinct_arg_sets,
            "distinct argument set",
            "distinct argument sets"
        ),
    );

    let _ = writeln!(out, "\nexecuted as:");
    for s in &x.arg_sets {
        let _ = writeln!(
            out,
            "  {:>4}x  {}",
            s.count,
            fmt_call(&x.commands[0], &s.args)
        );
    }
    if x.distinct_arg_sets > x.arg_sets.len() as i64 {
        let _ = writeln!(
            out,
            "        … and {} more distinct argument sets",
            x.distinct_arg_sets - x.arg_sets.len() as i64
        );
    }

    let _ = writeln!(out, "\ncontext (call chain, innermost first):");
    for c in &x.contexts {
        if c.call_chain.is_empty() {
            let _ = writeln!(out, "  {:>4}x  at top level", c.count);
        } else {
            let _ = writeln!(out, "  {:>4}x  via {}", c.count, c.call_chain[0]);
            for hop in &c.call_chain[1..] {
                let _ = writeln!(out, "         via {hop}");
            }
        }
    }
    if x.distinct_contexts > x.contexts.len() as i64 {
        let _ = writeln!(
            out,
            "        … and {} more distinct call chains",
            x.distinct_contexts - x.contexts.len() as i64
        );
    }

    let has_effects = !(x.writes.is_empty()
        && x.edges.is_empty()
        && x.requirements.is_empty()
        && x.targets_defined.is_empty()
        && x.properties.is_empty());
    let _ = writeln!(out, "\neffects:");
    if !has_effects {
        let _ = writeln!(
            out,
            "  none recorded (no variable writes, link edges, usage requirements, \
             target definitions, or property writes originate here)"
        );
        return out;
    }

    for t in &x.targets_defined {
        let _ = writeln!(
            out,
            "  defined target {} ({})",
            t.name,
            t.type_.as_deref().unwrap_or("?")
        );
    }
    let shown_edges = x.edges.len().min(MAX_TEXT_GROUPS);
    for e in &x.edges[..shown_edges] {
        let times = if e.count == 1 {
            String::new()
        } else {
            format!("  ({}x)", e.count)
        };
        let _ = writeln!(
            out,
            "  created link edge {} -{}-> {}{times}",
            e.src, e.visibility, e.dst
        );
        for ev in &e.final_link_evidence {
            let _ = writeln!(
                out,
                "    final link evidence (File API): {}",
                truncate(ev, 160)
            );
        }
    }
    if x.edges.len() > shown_edges {
        let _ = writeln!(
            out,
            "  … and {} more link edges (see --format json)",
            x.edges.len() - shown_edges
        );
    }
    let shown_reqs = x.requirements.len().min(MAX_TEXT_GROUPS);
    for r in &x.requirements[..shown_reqs] {
        let vis = r.visibility.as_deref().unwrap_or("");
        let vals: Vec<String> = r
            .values
            .iter()
            .map(|v| {
                if v.count == 1 {
                    fmt_value(&v.value)
                } else {
                    format!("{} ({}x)", fmt_value(&v.value), v.count)
                }
            })
            .collect();
        let more = if r.distinct_values > r.values.len() as i64 {
            format!(", … {} more", r.distinct_values - r.values.len() as i64)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  added {} {} {} to target {}{more}",
            vis,
            r.kind,
            vals.join(", "),
            r.target
        );
    }
    if x.requirements.len() > shown_reqs {
        let _ = writeln!(
            out,
            "  … and {} more requirement groups (see --format json)",
            x.requirements.len() - shown_reqs
        );
    }
    let shown_props = x.properties.len().min(MAX_TEXT_GROUPS);
    for p in &x.properties[..shown_props] {
        let times = if p.count == 1 {
            String::new()
        } else {
            format!("  ({}x)", p.count)
        };
        let _ = writeln!(out, "  set property {} on {}{times}", p.prop, p.target);
    }
    if x.properties.len() > shown_props {
        let _ = writeln!(
            out,
            "  … and {} more property writes (see --format json)",
            x.properties.len() - shown_props
        );
    }

    let shown = x.writes.len().min(MAX_TEXT_GROUPS);
    for w in &x.writes[..shown] {
        let _ = writeln!(
            out,
            "  wrote {} ({} write in {} scope): {}",
            w.name,
            w.write_kind,
            w.scope_kinds.join("/"),
            plural(w.writes, "write", "writes")
        );
        for v in &w.values {
            if v.count == 1 && w.writes == 1 {
                let _ = writeln!(out, "    value {}", fmt_value(&v.value));
            } else {
                let _ = writeln!(out, "    {:>4}x  {}", v.count, fmt_value(&v.value));
            }
        }
        if w.distinct_values > w.values.len() as i64 {
            let _ = writeln!(
                out,
                "        … and {} more distinct values",
                w.distinct_values - w.values.len() as i64
            );
        }
        if w.resolved_reads == 0 {
            let _ = writeln!(out, "    never read (no read resolved to these writes)");
        } else {
            let _ = writeln!(
                out,
                "    value read {}: first at {}",
                plural(w.resolved_reads, "time", "times"),
                w.first_reads.join(", ")
            );
        }
    }
    if x.writes.len() > shown {
        let _ = writeln!(
            out,
            "  … and {} more written variables (see --format json)",
            x.writes.len() - shown
        );
    }
    out
}
