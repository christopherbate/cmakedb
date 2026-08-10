//! Link-graph passes (lint roadmap Tier 1): `duplicate-links` and
//! `cyclic-links`, both pure queries over `tgt_edges`.
//!
//! `duplicate-links`: the same destination linked to the same target more
//! than once. Ingestion drops the `debug`/`optimized`/`general` keywords
//! when collecting edges (see `collect_graph` in `ingest/dataflow.rs`), so
//! a legitimate per-config pair (`debug X optimized X`) arrives as two
//! identical rows from one event — this pass re-walks the originating
//! event's expanded args with the same split to assign every occurrence a
//! config slot and only counts repeats *within* a slot. Conflicting
//! visibilities are the warning class (the declarations merge to their
//! union — the item is linked and/or propagated per the loosest one, so
//! the stricter call is silently subsumed); same-visibility repeats are
//! redundancy notes.
//!
//! `cyclic-links`: strongly-connected components of size >= 2 (or
//! self-loops) over edges whose both endpoints are real targets. CMake
//! permits cycles between static libraries (it repeats them on the link
//! line), so this is note-severity architecture feedback, not an error.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use cmakedb_db::Db;
use rusqlite::params;

use crate::{event_in_source, event_span, Finding, Pass, PassConfig, Severity};

// --- duplicate-links -------------------------------------------------------

pub struct DuplicateLinks;

/// Per-config link slot: `debug`/`optimized` keyword items only apply to
/// one configuration; `general` (and no keyword) means all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    All,
    Debug,
    Optimized,
}

impl Slot {
    fn label(self) -> &'static str {
        match self {
            Slot::All => "all configurations",
            Slot::Debug => "debug-only",
            Slot::Optimized => "optimized-only",
        }
    }
}

/// Re-walk a `target_link_libraries` event's expanded args and return the
/// (visibility, config slot) of every occurrence of `dst`, replicating the
/// exact split `collect_graph` used at ingest time (args after the target
/// name, each split on `;`) plus the keyword tracking ingestion drops.
fn occurrences_in_call(args: &[String], dst: &str) -> Vec<(String, Slot)> {
    let mut vis = "PUBLIC".to_string(); // legacy plain signature
    let mut slot = Slot::All;
    let mut out = Vec::new();
    for a in args.iter().skip(1).flat_map(|a| a.split(';')) {
        match a {
            "PUBLIC" | "LINK_PUBLIC" => vis = "PUBLIC".into(),
            "PRIVATE" | "LINK_PRIVATE" => vis = "PRIVATE".into(),
            "INTERFACE" | "LINK_INTERFACE_LIBRARIES" => vis = "INTERFACE".into(),
            "debug" => slot = Slot::Debug,
            "optimized" => slot = Slot::Optimized,
            "general" => slot = Slot::All,
            lib if !lib.is_empty() => {
                if lib == dst {
                    out.push((vis.clone(), slot));
                }
                slot = Slot::All; // keywords apply to the next item only
            }
            _ => {}
        }
    }
    out
}

impl Pass for DuplicateLinks {
    fn id(&self) -> &'static str {
        "duplicate-links"
    }
    fn description(&self) -> &'static str {
        "the same dependency linked to a target more than once"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut groups_stmt = db.conn.prepare(
            "SELECT e.src_target, t.name, e.dst FROM tgt_edges e
             JOIN targets t ON t.id = e.src_target
             GROUP BY e.src_target, e.dst HAVING count(*) > 1
             ORDER BY min(e.id)",
        )?;
        let groups: Vec<(i64, String, String)> = groups_stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut rows_stmt = db.conn.prepare(
            "SELECT visibility, origin_event FROM tgt_edges
             WHERE src_target = ?1 AND dst = ?2 ORDER BY id",
        )?;
        let mut event_stmt = db
            .conn
            .prepare("SELECT cmd_lower, args_json FROM events WHERE id = ?1")?;

        let mut findings = Vec::new();
        for (src_id, src_name, dst) in groups {
            // A ';' inside $<...> splits incorrectly at ingest (known gap)
            // — genex-y destinations are not trustworthy duplicate keys.
            if dst.contains("$<") || cfg.ignored(&src_name) || cfg.ignored(&dst) {
                continue;
            }
            let rows: Vec<(String, Option<i64>)> = rows_stmt
                .query_map(params![src_id, dst], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;

            // One logical occurrence list, in execution order, with config
            // slots recovered from the originating calls.
            let mut occurrences: Vec<(String, Slot, Option<i64>)> = Vec::new();
            let mut i = 0usize;
            while i < rows.len() {
                let event = rows[i].1;
                let mut j = i;
                while j < rows.len() && rows[j].1 == event {
                    j += 1;
                }
                let event_rows = &rows[i..j];
                let parsed: Option<Vec<(String, Slot)>> = event.and_then(|eid| {
                    let (cmd, args_json): (String, String) = event_stmt
                        .query_row([eid], |r| Ok((r.get(0)?, r.get(1)?)))
                        .ok()?;
                    if cmd != "target_link_libraries" {
                        return None;
                    }
                    let args: Vec<String> = serde_json::from_str(&args_json).ok()?;
                    let occ = occurrences_in_call(&args, &dst);
                    // Only trust the re-walk when it reproduces ingestion.
                    (occ.len() == event_rows.len()).then_some(occ)
                });
                match parsed {
                    Some(occ) => {
                        occurrences.extend(occ.into_iter().map(|(v, s)| (v, s, event)));
                    }
                    None => {
                        occurrences
                            .extend(event_rows.iter().map(|(v, e)| (v.clone(), Slot::All, *e)));
                    }
                }
                i = j;
            }

            // Repeats only count within a config slot: `debug X` +
            // `optimized X` is the legitimate per-config idiom, folded here.
            let mut by_slot: HashMap<Slot, usize> = HashMap::new();
            for (_, slot, _) in &occurrences {
                *by_slot.entry(*slot).or_insert(0) += 1;
            }
            let dup: Vec<&(String, Slot, Option<i64>)> = occurrences
                .iter()
                .filter(|(_, slot, _)| by_slot[slot] >= 2)
                .collect();
            if dup.is_empty() {
                continue;
            }
            // Only report duplicates the project's own code participates in.
            if !dup
                .iter()
                .any(|(_, _, e)| e.map(|e| event_in_source(db, e)).unwrap_or(false))
            {
                continue;
            }

            let visibilities: Vec<String> = {
                let mut seen = HashSet::new();
                dup.iter()
                    .filter(|(v, _, _)| seen.insert(v.clone()))
                    .map(|(v, _, _)| v.clone())
                    .collect()
            };
            let distinct_events: HashSet<Option<i64>> = dup.iter().map(|(_, _, e)| *e).collect();
            let slot_note = match dup.first().map(|(_, s, _)| *s) {
                Some(Slot::All) | None => String::new(),
                Some(s) => format!(" ({})", s.label()),
            };
            let (severity, message) = if visibilities.len() > 1 {
                (
                    Severity::Warning,
                    format!(
                        "`{src_name}` links `{dst}` {} times with conflicting visibilities \
                         ({}){slot_note} — the declarations merge to their union (the \
                         dependency is linked and propagated per the loosest one), so the \
                         stricter call is silently subsumed; keep exactly one",
                        dup.len(),
                        visibilities.join(", "),
                    ),
                )
            } else if distinct_events.len() == 1 {
                (
                    Severity::Note,
                    format!(
                        "`{src_name}` links `{dst}` {} times in the same call \
                         (duplicate items in the expanded argument list, {}){slot_note} \
                         — redundant",
                        dup.len(),
                        visibilities[0],
                    ),
                )
            } else {
                (
                    Severity::Note,
                    format!(
                        "`{src_name}` links `{dst}` {} times with the same visibility \
                         ({}){slot_note} — redundant duplicate link",
                        dup.len(),
                        visibilities[0],
                    ),
                )
            };

            // Primary = first in-source occurrence; every other occurrence
            // is evidence.
            let primary_idx = dup
                .iter()
                .position(|(_, _, e)| e.map(|e| event_in_source(db, e)).unwrap_or(false))
                .unwrap_or(0);
            let span_of = |e: &Option<i64>| match e {
                Some(e) => event_span(db, *e),
                None => crate::SourceSpan::new("<unknown>", 0),
            };
            let primary = span_of(&dup[primary_idx].2);
            let related = dup
                .iter()
                .enumerate()
                .filter(|(k, _)| *k != primary_idx)
                .map(|(_, (v, _, e))| {
                    let note = if *e == dup[primary_idx].2 {
                        format!("duplicate item in the same call ({v})")
                    } else {
                        format!("also linked {v} here")
                    };
                    (span_of(e), note)
                })
                .collect();
            findings.push(Finding {
                rule: self.id().into(),
                severity,
                message,
                primary,
                related,
                fix: None,
            });
        }
        Ok(findings)
    }
}

// --- cyclic-links ----------------------------------------------------------

pub struct CyclicLinks;

impl Pass for CyclicLinks {
    fn id(&self) -> &'static str {
        "cyclic-links"
    }
    fn description(&self) -> &'static str {
        "circular link dependencies between targets (architecture smell)"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        // Edges where both endpoints are real targets; first (execution
        // order) row per (src, dst) pair carries the origin evidence.
        let mut stmt = db.conn.prepare(
            "SELECT src_target, dst_target, visibility, origin_event
             FROM tgt_edges WHERE dst_target IS NOT NULL ORDER BY id",
        )?;
        let raw: Vec<(i64, i64, String, Option<i64>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut edge_info: HashMap<(i64, i64), (String, Option<i64>)> = HashMap::new();
        for (src, dst, vis, origin) in raw {
            edge_info.entry((src, dst)).or_insert((vis, origin));
        }
        if edge_info.is_empty() {
            return Ok(vec![]);
        }

        let mut meta_stmt = db.conn.prepare("SELECT id, name, imported FROM targets")?;
        let meta: HashMap<i64, (String, bool)> = meta_stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, (r.get(1)?, r.get::<_, i64>(2)? != 0)))
            })?
            .collect::<rusqlite::Result<_>>()?;

        // Dense node numbering over targets that participate in edges.
        let mut ids: Vec<i64> = edge_info
            .keys()
            .flat_map(|(s, d)| [*s, *d])
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        ids.sort_unstable();
        let index: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, t)| (*t, i)).collect();
        let mut adj = vec![Vec::new(); ids.len()];
        for (src, dst) in edge_info.keys() {
            adj[index[src]].push(index[dst]);
        }
        for a in &mut adj {
            a.sort_unstable();
        }

        let mut findings = Vec::new();
        for comp in strongly_connected_components(&adj) {
            let cyclic = comp.len() >= 2
                || (comp.len() == 1 && adj[comp[0]].binary_search(&comp[0]).is_ok());
            if !cyclic {
                continue;
            }
            let mut names: Vec<&str> = comp
                .iter()
                .map(|n| meta.get(&ids[*n]).map(|(name, _)| name.as_str()))
                .collect::<Option<_>>()
                .unwrap_or_default();
            if names.is_empty()
                || names.iter().any(|n| cfg.ignored(n))
                || comp
                    .iter()
                    .any(|n| meta.get(&ids[*n]).map(|(_, imp)| *imp).unwrap_or(true))
            {
                continue; // imported/ignored targets: not project architecture
            }
            names.sort_unstable();

            // One concrete cycle path, rendered like a why-links chain:
            // shortest cycle through the lexicographically first member.
            let start = comp
                .iter()
                .copied()
                .min_by_key(|n| &meta[&ids[*n]].0)
                .unwrap();
            let in_comp: HashSet<usize> = comp.iter().copied().collect();
            let Some(path) = shortest_cycle(&adj, start, &in_comp) else {
                continue;
            };
            let hops: Vec<((i64, i64), (String, String))> = path
                .windows(2)
                .map(|w| {
                    let (u, v) = (ids[w[0]], ids[w[1]]);
                    let (vis, _) = &edge_info[&(u, v)];
                    ((u, v), (meta[&u].0.clone(), vis.clone()))
                })
                .collect();
            let origin_of = |edge: &(i64, i64)| edge_info[edge].1;
            if !hops.iter().any(|(e, _)| {
                origin_of(e)
                    .map(|o| event_in_source(db, o))
                    .unwrap_or(false)
            }) {
                continue; // cycle entirely outside the project's own code
            }

            let rendered: String = {
                let mut s = String::new();
                for (i, w) in path.iter().enumerate() {
                    if i > 0 {
                        s.push_str(" -> ");
                    }
                    s.push_str(&meta[&ids[*w]].0);
                }
                s
            };
            let extra = if names.len() > path.len() - 1 {
                format!(
                    " (strongly-connected group of {}: {})",
                    names.len(),
                    names.join(", ")
                )
            } else {
                String::new()
            };
            let span_of = |o: Option<i64>| match o {
                Some(o) => event_span(db, o),
                None => crate::SourceSpan::new("<unknown>", 0),
            };
            let primary = span_of(origin_of(&hops[0].0));
            let related = hops
                .iter()
                .skip(1)
                .map(|(e, (src_name, vis))| {
                    let dst_name = &meta[&e.1].0;
                    (
                        span_of(origin_of(e)),
                        format!("`{src_name}` links `{dst_name}` ({vis}) here"),
                    )
                })
                .collect();
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Note,
                message: format!(
                    "circular link dependency: {rendered}{extra}. CMake permits cycles \
                     between static libraries (it repeats them on the link line), so this \
                     is an architecture smell rather than an error — consider breaking \
                     the cycle"
                ),
                primary,
                related,
                fix: None,
            });
        }
        // Stable reporting order regardless of SCC discovery order.
        findings.sort_by(|a, b| a.message.cmp(&b.message));
        Ok(findings)
    }
}

/// Iterative Tarjan SCC over a dense adjacency list.
fn strongly_connected_components(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    const UNSET: usize = usize::MAX;
    let mut index = vec![UNSET; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut components = Vec::new();

    // Explicit DFS: (node, next child position).
    let mut work: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if index[root] != UNSET {
            continue;
        }
        work.push((root, 0));
        while let Some(&mut (v, ref mut child)) = work.last_mut() {
            if *child == 0 {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if let Some(&w) = adj[v].get(*child) {
                *child += 1;
                if index[w] == UNSET {
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                if low[v] == index[v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    components.push(comp);
                }
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    low[parent] = low[parent].min(low[v]);
                }
            }
        }
    }
    components
}

/// Shortest cycle through `start` using only nodes in `allowed`
/// (BFS; `start -> ... -> start`, or `[start, start]` for a self-loop).
fn shortest_cycle(
    adj: &[Vec<usize>],
    start: usize,
    allowed: &HashSet<usize>,
) -> Option<Vec<usize>> {
    if adj[start].binary_search(&start).is_ok() {
        return Some(vec![start, start]);
    }
    let mut parent: HashMap<usize, usize> = HashMap::new();
    let mut queue = std::collections::VecDeque::from([start]);
    while let Some(u) = queue.pop_front() {
        for &w in &adj[u] {
            if w == start {
                // Reconstruct start -> ... -> u -> start.
                let mut rev = vec![u];
                let mut cur = u;
                while cur != start {
                    cur = parent[&cur];
                    rev.push(cur);
                }
                rev.reverse();
                rev.push(start);
                return Some(rev);
            }
            if allowed.contains(&w) && w != start && !parent.contains_key(&w) && u != w {
                parent.insert(w, u);
                queue.push_back(w);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn occurrence_parsing_folds_config_keywords() {
        // debug/optimized pair: two occurrences, distinct slots — no dup.
        let occ = occurrences_in_call(
            &args(&["app", "PRIVATE", "optimized", "core", "debug", "core"]),
            "core",
        );
        assert_eq!(
            occ,
            vec![
                ("PRIVATE".to_string(), Slot::Optimized),
                ("PRIVATE".to_string(), Slot::Debug)
            ]
        );
        // Keyword applies to the next item only.
        let occ = occurrences_in_call(&args(&["app", "debug", "core", "core"]), "core");
        assert_eq!(
            occ,
            vec![
                ("PUBLIC".to_string(), Slot::Debug),
                ("PUBLIC".to_string(), Slot::All)
            ]
        );
        // ';'-joined expanded lists split like ingestion does.
        let occ = occurrences_in_call(&args(&["t", "PUBLIC", "a;b;a"]), "a");
        assert_eq!(occ.len(), 2);
        // Visibility switches mid-call.
        let occ = occurrences_in_call(&args(&["t", "PUBLIC", "x", "PRIVATE", "x"]), "x");
        assert_eq!(
            occ,
            vec![
                ("PUBLIC".to_string(), Slot::All),
                ("PRIVATE".to_string(), Slot::All)
            ]
        );
    }

    #[test]
    fn scc_and_cycle_path() {
        // 0 -> 1 -> 2 -> 0 (cycle), 3 -> 0 (feeder), 4 self-loop.
        let adj = vec![vec![1], vec![2], vec![0], vec![0], vec![4]];
        let mut comps = strongly_connected_components(&adj);
        for c in &mut comps {
            c.sort_unstable();
        }
        comps.sort();
        assert!(comps.contains(&vec![0, 1, 2]));
        assert!(comps.contains(&vec![3]));
        assert!(comps.contains(&vec![4]));

        let allowed: HashSet<usize> = [0, 1, 2].into_iter().collect();
        assert_eq!(shortest_cycle(&adj, 0, &allowed), Some(vec![0, 1, 2, 0]));
        let allowed4: HashSet<usize> = [4].into_iter().collect();
        assert_eq!(shortest_cycle(&adj, 4, &allowed4), Some(vec![4, 4]));

        // Two-node cycle.
        let adj2 = vec![vec![1], vec![0]];
        let allowed2: HashSet<usize> = [0, 1].into_iter().collect();
        assert_eq!(shortest_cycle(&adj2, 0, &allowed2), Some(vec![0, 1, 0]));
    }
}
