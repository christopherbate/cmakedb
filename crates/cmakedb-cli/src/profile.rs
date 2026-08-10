//! `cmakedb profile`: configure-time hotspot report over the recorded
//! per-event timings (ROADMAP "slow-configure"; a report, not a lint
//! pass).
//!
//! Accuracy model, stated wherever the numbers appear: the trace gives
//! `elapsed_us` as time-to-next-event. For a leaf command that is the
//! command's own cost; for a frame-opening command (a function call,
//! `include`, `add_subdirectory`) it covers only the latency until the
//! first body event, NOT the body. So this report computes both:
//!
//! - **self time** per event = its `elapsed_us` (good for leaf outliers:
//!   `execute_process`, `file(GLOB_RECURSE)`, `find_path` ...), and
//! - **inclusive time** per scope = closing event `time_abs` minus
//!   opening event `time_abs` — a true wall-clock span, giving real
//!   function/macro/include/directory costs, aggregated by scope name.

use anyhow::{bail, Result};
use cmakedb_db::Db;
use serde::Serialize;
use std::collections::BTreeMap;

/// One-line caveat carried in both text and JSON output.
pub const NOTE: &str = "self time is time-to-next-event: for frame openers (function calls, \
     include, add_subdirectory) it excludes the body; scope totals are true \
     wall-clock inclusive spans";

#[derive(Debug, Serialize)]
pub struct Profile {
    pub schema_version: u32,
    /// Wall span of the whole configure: last event time - first event time.
    pub total_span_us: i64,
    pub event_count: i64,
    pub note: String,
    /// Slowest individual events by self time (`elapsed_us`).
    pub slowest_events: Vec<SlowEvent>,
    /// Hottest scopes by inclusive wall time, aggregated by (kind, name).
    pub scopes: Vec<ScopeHotspot>,
    /// Total self time per command name.
    pub commands: Vec<CommandHotspot>,
}

#[derive(Debug, Serialize)]
pub struct SlowEvent {
    pub cmd: String,
    pub location: String,
    pub self_us: i64,
}

#[derive(Debug, Serialize)]
pub struct ScopeHotspot {
    pub kind: String,
    /// Function/macro name, included file, or directory path.
    pub name: String,
    /// Call site of the first time this scope was opened.
    pub location: String,
    pub calls: i64,
    pub total_us: i64,
    pub mean_us: i64,
}

#[derive(Debug, Serialize)]
pub struct CommandHotspot {
    pub cmd: String,
    pub calls: i64,
    pub total_self_us: i64,
}

/// Total configure span (last minus first timestamped event, integer
/// microseconds) plus the timestamped event count. Errors when the
/// recording carries no timings at all.
fn configure_span_us(db: &Db) -> Result<(i64, i64)> {
    let (event_count, span_s): (i64, Option<f64>) = db.conn.query_row(
        "SELECT count(*), max(time_abs) - min(time_abs)
         FROM events WHERE time_abs IS NOT NULL",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if event_count == 0 {
        bail!("recording has no event timings — re-record with a cmake that emits time/elapsed");
    }
    Ok((event_count, (span_s.unwrap_or(0.0) * 1e6).round() as i64))
}

pub fn profile(db: &Db, top: usize) -> Result<Profile> {
    let top = top as i64;

    // Header: total configure span from the first/last timestamped event.
    let (event_count, total_span_us) = configure_span_us(db)?;

    // 1. Slowest individual events by self time. One ordered scan of
    //    events; the file join happens per emitted row only.
    let mut stmt = db.conn.prepare(
        "SELECT e.cmd, f.path, e.line, e.elapsed_us
         FROM events e JOIN files f ON f.id = e.file_id
         WHERE e.elapsed_us IS NOT NULL
         ORDER BY e.elapsed_us DESC
         LIMIT ?1",
    )?;
    let slowest_events = stmt
        .query_map([top], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(cmd, path, line, self_us)| SlowEvent {
            cmd,
            location: format!("{}:{}", db.display_path(&path), line),
            self_us,
        })
        .collect();

    // 2. Hottest scopes by inclusive wall time, aggregated by name so a
    //    function called N times reports one row (total, count, mean).
    //    Both event joins are primary-key lookups; scopes lacking either
    //    endpoint (e.g. the root scope, unclosed try_compile shells) are
    //    skipped — the root's span is the header's total anyway.
    let mut stmt = db.conn.prepare(
        "SELECT s.kind, coalesce(s.name, ''), count(*),
                CAST(round(sum((ec.time_abs - eo.time_abs) * 1e6)) AS INTEGER),
                min(s.opened_by_event)
         FROM scopes s
         JOIN events eo ON eo.id = s.opened_by_event
         JOIN events ec ON ec.id = s.closed_by_event
         WHERE s.kind <> 'root'
           AND eo.time_abs IS NOT NULL AND ec.time_abs IS NOT NULL
         GROUP BY s.kind, s.name
         ORDER BY 4 DESC
         LIMIT ?1",
    )?;
    let scope_rows = stmt
        .query_map([top], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut scopes = Vec::with_capacity(scope_rows.len());
    for (kind, name, calls, total_us, first_open) in scope_rows {
        scopes.push(ScopeHotspot {
            kind,
            name: db.display_path(&name),
            location: db.event_location(first_open).unwrap_or_default(),
            calls,
            total_us,
            mean_us: total_us / calls.max(1),
        });
    }

    // 3. Total self time per command name ("4000 string() calls cost
    //    800ms" patterns).
    let mut stmt = db.conn.prepare(
        "SELECT e.cmd_lower, count(*), sum(e.elapsed_us)
         FROM events e
         WHERE e.elapsed_us IS NOT NULL
         GROUP BY e.cmd_lower
         ORDER BY 3 DESC
         LIMIT ?1",
    )?;
    let commands = stmt
        .query_map([top], |r| {
            Ok(CommandHotspot {
                cmd: r.get(0)?,
                calls: r.get(1)?,
                total_self_us: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Profile {
        schema_version: 1,
        total_span_us,
        event_count,
        note: NOTE.to_string(),
        slowest_events,
        scopes,
        commands,
    })
}

/// Human time units: `421us`, `12.34ms`, `1.23s`.
fn fmt_us(us: i64) -> String {
    if us < 1_000 {
        format!("{us}us")
    } else if us < 1_000_000 {
        format!("{:.2}ms", us as f64 / 1e3)
    } else {
        format!("{:.2}s", us as f64 / 1e6)
    }
}

/// Left-ellipsize to `max` chars keeping the tail (paths stay
/// recognizable by their most specific components).
fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

pub fn render_text(p: &Profile) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "configure profile: {} total span, {} events",
        fmt_us(p.total_span_us),
        p.event_count
    );
    let _ = writeln!(out, "note: {}", NOTE);

    let _ = writeln!(out, "\nslowest events (self time):");
    for e in &p.slowest_events {
        let _ = writeln!(
            out,
            "  {:>9}  {:<26}  {}",
            fmt_us(e.self_us),
            ellipsize(&e.cmd, 26),
            ellipsize(&e.location, 72)
        );
    }

    let _ = writeln!(
        out,
        "\nhottest scopes (inclusive wall time, grouped by name):"
    );
    for s in &p.scopes {
        let what = ellipsize(&format!("{} {}", s.kind, s.name), 44);
        let calls = if s.calls == 1 {
            "1 call".to_string()
        } else {
            format!("{} calls, mean {}", s.calls, fmt_us(s.mean_us))
        };
        let _ = writeln!(
            out,
            "  {:>9}  {:<44}  {:<24}  {}",
            fmt_us(s.total_us),
            what,
            calls,
            ellipsize(&s.location, 72)
        );
    }

    let _ = writeln!(out, "\nhottest commands (total self time):");
    for c in &p.commands {
        let _ = writeln!(
            out,
            "  {:>9}  {:<26}  {} calls",
            fmt_us(c.total_self_us),
            ellipsize(&c.cmd, 26),
            c.calls
        );
    }
    out
}

// --- Flamegraph export (`profile --flamegraph`) --------------------------
//
// The scope tree IS a call tree with wall-clock spans, so folded-stack
// output (the `frame;frame;frame N` format consumed by flamegraph.pl,
// inferno, speedscope) is a tree walk plus formatting. Folded-stack
// semantics want SELF time per stack — the tools sum inclusive time up
// the stack themselves — so each scope's value is its EXCLUSIVE span:
// its inclusive wall span minus its children's inclusive spans, clamped
// at zero. The root gets the residue (total span minus its children),
// which covers all top-level leaf commands. Scopes with a missing
// endpoint are skipped with their subtrees (consistent with the report's
// scope section); their time stays in the parent's exclusive value, so
// the emitted values still sum to the total configure span.

/// `<build-dir>/` prefix (trailing slash included) for name
/// normalization, when the recording carries one.
fn build_prefix(db: &Db) -> Option<String> {
    db.get_meta("build_dir")
        .ok()
        .flatten()
        .map(|b| format!("{}/", b.trim_end_matches('/')))
}

/// Display-normalized scope name: source-relative via `display_path`,
/// with paths under the recording's build dir rewritten to `<build>/...`
/// — function/include names and these normalized paths are stable across
/// recordings of the same tree, so `--compare` aligns even when the
/// build (or checkout) directories differ.
fn normalized_scope_name(db: &Db, build_prefix: &Option<String>, name: &str) -> String {
    if let Some(prefix) = build_prefix {
        let spelled = cmakedb_db::cmake_path_spelling(name);
        if cmakedb_db::cmake_path_starts_with(&spelled, prefix) {
            return format!("<build>/{}", &spelled[prefix.len()..]);
        }
    }
    db.display_path(name)
}

/// Sanitized frame name: `kind:name` (name display-path relativized),
/// bare `kind` when the scope is unnamed, `root` for the root. `;` and
/// whitespace would corrupt the folded format, so they are replaced.
fn frame_name(db: &Db, bp: &Option<String>, kind: &str, name: &str) -> String {
    let raw = if kind == "root" {
        "root".to_string()
    } else if name.is_empty() {
        kind.to_string()
    } else {
        format!("{kind}:{}", normalized_scope_name(db, bp, name))
    };
    raw.chars()
        .map(|c| match c {
            ';' => ',',
            c if c.is_whitespace() => '_',
            c => c,
        })
        .collect()
}

/// Folded-stack lines, aggregated by identical stacks and sorted (byte
/// order) for deterministic output.
pub fn flamegraph(db: &Db) -> Result<String> {
    let (_, total_span_us) = configure_span_us(db)?;
    let bp = build_prefix(db);

    struct Node {
        kind: String,
        name: String,
        /// Inclusive wall span; None when either endpoint is missing.
        span_us: Option<i64>,
        children: Vec<i64>,
    }

    // One scan of scopes with primary-key event joins for the endpoints.
    let mut stmt = db.conn.prepare(
        "SELECT s.id, s.parent_id, s.kind, coalesce(s.name, ''),
                CASE WHEN eo.time_abs IS NOT NULL AND ec.time_abs IS NOT NULL
                     THEN CAST(round((ec.time_abs - eo.time_abs) * 1e6) AS INTEGER)
                END
         FROM scopes s
         LEFT JOIN events eo ON eo.id = s.opened_by_event
         LEFT JOIN events ec ON ec.id = s.closed_by_event
         ORDER BY s.id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut nodes: BTreeMap<i64, Node> = BTreeMap::new();
    let mut root_id = None;
    for (id, parent, kind, name, span_us) in rows {
        if kind == "root" {
            root_id = Some(id);
        }
        nodes.insert(
            id,
            Node {
                kind,
                name,
                span_us,
                children: Vec::new(),
            },
        );
        if let Some(p) = parent {
            // Parents precede children in scope-id order.
            if let Some(pn) = nodes.get_mut(&p) {
                pn.children.push(id);
            }
        }
    }
    let root_id = root_id.ok_or_else(|| anyhow::anyhow!("recording has no root scope"))?;
    // The root scope has no closing event; its span is the configure span.
    nodes.get_mut(&root_id).unwrap().span_us = Some(total_span_us);

    // DFS carrying the folded stack prefix; aggregate identical stacks.
    let mut folded: BTreeMap<String, i64> = BTreeMap::new();
    let mut work = vec![(root_id, String::new())];
    while let Some((id, prefix)) = work.pop() {
        let n = &nodes[&id];
        let Some(span) = n.span_us else {
            continue; // missing endpoint: skip the subtree
        };
        let stack = if prefix.is_empty() {
            frame_name(db, &bp, &n.kind, &n.name)
        } else {
            format!("{prefix};{}", frame_name(db, &bp, &n.kind, &n.name))
        };
        let child_total: i64 = n
            .children
            .iter()
            .filter_map(|c| nodes[c].span_us)
            .sum::<i64>();
        let exclusive = (span - child_total).max(0);
        if exclusive > 0 {
            *folded.entry(stack.clone()).or_insert(0) += exclusive;
        }
        for &c in &n.children {
            work.push((c, stack.clone()));
        }
    }

    let mut out = String::new();
    for (stack, us) in &folded {
        use std::fmt::Write;
        let _ = writeln!(out, "{stack} {us}");
    }
    Ok(out)
}

// --- Recording comparison (`profile --compare <other.db>`) ---------------

/// Caveat + delta convention carried in both text and JSON output.
pub const COMPARE_NOTE: &str = "inclusive wall time aggregated by scope (kind, name), names \
     display-path normalized so build-dir differences align; delta = primary - other \
     (positive: primary is slower)";

#[derive(Debug, Serialize)]
pub struct CompareReport {
    pub schema_version: u32,
    pub primary_span_us: i64,
    pub other_span_us: i64,
    /// Total-span regression: primary minus other.
    pub span_delta_us: i64,
    pub note: String,
    /// Top rows by absolute inclusive-time delta.
    pub scopes: Vec<CompareRow>,
}

#[derive(Debug, Serialize)]
pub struct CompareRow {
    pub kind: String,
    pub name: String,
    /// `common` | `appeared` (primary only) | `disappeared` (other only).
    pub status: String,
    pub primary_total_us: i64,
    pub other_total_us: i64,
    pub delta_us: i64,
    pub primary_calls: i64,
    pub other_calls: i64,
}

/// Inclusive wall time + call count per (kind, display-normalized name).
/// Scopes lacking either endpoint are skipped, as in the report.
fn scope_totals(db: &Db) -> Result<BTreeMap<(String, String), (i64, i64)>> {
    let bp = build_prefix(db);
    let mut stmt = db.conn.prepare(
        "SELECT s.kind, coalesce(s.name, ''), count(*),
                CAST(round(sum((ec.time_abs - eo.time_abs) * 1e6)) AS INTEGER)
         FROM scopes s
         JOIN events eo ON eo.id = s.opened_by_event
         JOIN events ec ON ec.id = s.closed_by_event
         WHERE s.kind <> 'root'
           AND eo.time_abs IS NOT NULL AND ec.time_abs IS NOT NULL
         GROUP BY s.kind, s.name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out: BTreeMap<(String, String), (i64, i64)> = BTreeMap::new();
    for (kind, name, calls, total_us) in rows {
        // Normalization may fold distinct raw spellings together — accumulate.
        let e = out
            .entry((kind, normalized_scope_name(db, &bp, &name)))
            .or_insert((0, 0));
        e.0 += calls;
        e.1 += total_us;
    }
    Ok(out)
}

/// Per-scope-name regression report between two recordings. Alignment is
/// by (kind, display-path-normalized name): function/macro/include names
/// and source-relative directory paths are stable across recordings of
/// the same tree even when build/source dirs differ.
pub fn compare(primary: &Db, other: &Db, top: usize) -> Result<CompareReport> {
    let (_, primary_span_us) = configure_span_us(primary)?;
    let (_, other_span_us) = configure_span_us(other)?;
    let pa = scope_totals(primary)?;
    let ob = scope_totals(other)?;

    let mut rows: Vec<CompareRow> = pa
        .keys()
        .chain(ob.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|key| {
            let p = pa.get(key);
            let o = ob.get(key);
            let (p_calls, p_total) = p.copied().unwrap_or((0, 0));
            let (o_calls, o_total) = o.copied().unwrap_or((0, 0));
            CompareRow {
                kind: key.0.clone(),
                name: key.1.clone(),
                status: match (p.is_some(), o.is_some()) {
                    (true, false) => "appeared",
                    (false, true) => "disappeared",
                    _ => "common",
                }
                .to_string(),
                primary_total_us: p_total,
                other_total_us: o_total,
                delta_us: p_total - o_total,
                primary_calls: p_calls,
                other_calls: o_calls,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.delta_us
            .abs()
            .cmp(&a.delta_us.abs())
            .then_with(|| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)))
    });
    rows.truncate(top);

    Ok(CompareReport {
        schema_version: 1,
        primary_span_us,
        other_span_us,
        span_delta_us: primary_span_us - other_span_us,
        note: COMPARE_NOTE.to_string(),
        scopes: rows,
    })
}

/// `+12.34ms` / `-421us` / `+0us`.
fn fmt_us_signed(us: i64) -> String {
    if us < 0 {
        format!("-{}", fmt_us(-us))
    } else {
        format!("+{}", fmt_us(us))
    }
}

pub fn render_compare_text(c: &CompareReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "profile compare: primary {} vs other {} (span delta {})",
        fmt_us(c.primary_span_us),
        fmt_us(c.other_span_us),
        fmt_us_signed(c.span_delta_us)
    );
    let _ = writeln!(out, "note: {}", COMPARE_NOTE);
    let _ = writeln!(out, "\nlargest inclusive-time deltas by scope:");
    for r in &c.scopes {
        let what = ellipsize(&format!("{} {}", r.kind, r.name), 44);
        let detail = match r.status.as_str() {
            "appeared" => format!(
                "appeared: {} ({} calls)",
                fmt_us(r.primary_total_us),
                r.primary_calls
            ),
            "disappeared" => format!(
                "disappeared: was {} ({} calls)",
                fmt_us(r.other_total_us),
                r.other_calls
            ),
            _ => {
                let calls = if r.primary_calls != r.other_calls {
                    format!("{} -> {} calls", r.other_calls, r.primary_calls)
                } else if r.primary_calls == 1 {
                    "1 call".to_string()
                } else {
                    format!("{} calls", r.primary_calls)
                };
                format!(
                    "{} -> {}  {}",
                    fmt_us(r.other_total_us),
                    fmt_us(r.primary_total_us),
                    calls
                )
            }
        };
        let _ = writeln!(
            out,
            "  {:>10}  {:<44}  {}",
            fmt_us_signed(r.delta_us),
            what,
            detail
        );
    }
    out
}
