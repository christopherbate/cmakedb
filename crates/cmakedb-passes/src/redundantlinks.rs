//! `redundant-links` (roadmap Track F, requirement placement): a direct
//! project-file link edge `T -> B` that is transitively redundant because
//! another direct dependency `A` of `T` already provides `B` through an
//! unbroken PUBLIC/INTERFACE chain (`A -> ... -> B`, every hop
//! propagating and resolving to a real target, never passing back
//! through `T`). The chain search is a bounded BFS (depth cap
//! [`MAX_CHAIN_HOPS`], visited-set cycle guard — cyclic-links proves
//! real projects have link cycles).
//!
//! Report-only Note severity **by design**: with static archives the
//! linker resolves symbols in command-line order, and an explicit
//! direct edge can be load-bearing for link ordering or ODR-sensitive
//! symbol resolution even when the same library also arrives
//! transitively. The finding says so; nothing here is auto-fixed.
//!
//! Guards: both direct edges must originate in project files; `T` must
//! be a real non-imported, non-interface, non-alias (and non-object)
//! target; `B` must resolve to a real target (external/string libs are
//! never flagged — name matching over-matches short names like `m`);
//! object libraries are excluded anywhere in the chain
//! (`$<TARGET_OBJECTS>` semantics differ); genex-bearing as-written
//! destinations and try_compile-scope origins are skipped.
//!
//! Wrapper-machinery guard (LLVM-validated): a hand-written
//! `target_link_libraries` call names exactly one target, so an origin
//! call site observed linking **more than one distinct source target**
//! is wrapper/loop machinery (`llvm_add_library`-style helpers). The
//! finding would point at the shared helper line, where no edit is
//! possible — the real edit site is the helper's input data, invisible
//! to the trace — so such candidates are skipped (documented FN class).
//! On LLVM this removes 1003 of 1019 raw findings, all from three
//! component-closure machinery lines that link deliberate full
//! static-archive closures.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use cmakedb_db::Db;

use crate::{
    event_in_source, event_in_try_compile, event_span, Finding, Pass, PassConfig, Severity,
};

pub struct RedundantLinks;

/// Depth cap for the interface-chain BFS (hops from `A` to `B`).
const MAX_CHAIN_HOPS: usize = 8;

struct TargetMeta {
    name: String,
    ty: Option<String>,
    imported: bool,
    alias_of: Option<String>,
}

struct Edge {
    src: i64,
    dst_text: String,
    dst: Option<i64>,
    visibility: String,
    origin: Option<i64>,
}

impl Pass for RedundantLinks {
    fn id(&self) -> &'static str {
        "redundant-links"
    }
    fn description(&self) -> &'static str {
        "direct link edges already provided transitively via a PUBLIC/INTERFACE chain"
    }

    fn run(&self, db: &Db, cfg: &PassConfig) -> Result<Vec<Finding>> {
        let mut meta_stmt = db
            .conn
            .prepare("SELECT id, name, type, imported, alias_of FROM targets")?;
        let meta: HashMap<i64, TargetMeta> = meta_stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    TargetMeta {
                        name: r.get(1)?,
                        ty: r.get(2)?,
                        imported: r.get::<_, i64>(3)? != 0,
                        alias_of: r.get(4)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let is_object = |t: i64| {
            meta.get(&t)
                .and_then(|m| m.ty.as_deref())
                .map(|ty| ty == "OBJECT_LIBRARY")
                .unwrap_or(false)
        };

        let mut edge_stmt = db.conn.prepare(
            "SELECT src_target, dst, dst_target, visibility, origin_event
             FROM tgt_edges ORDER BY id",
        )?;
        let edges: Vec<Edge> = edge_stmt
            .query_map([], |r| {
                Ok(Edge {
                    src: r.get(0)?,
                    dst_text: r.get(1)?,
                    dst: r.get(2)?,
                    visibility: r.get(3)?,
                    origin: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        // Interface (propagating) adjacency over resolved edges; the first
        // row per (src, dst) pair carries the evidence, like cyclic-links.
        let mut iface_adj: HashMap<i64, Vec<usize>> = HashMap::new();
        let mut iface_seen: HashSet<(i64, i64)> = HashSet::new();
        for (i, e) in edges.iter().enumerate() {
            let Some(dst) = e.dst else { continue };
            if (e.visibility == "PUBLIC" || e.visibility == "INTERFACE")
                && dst != e.src
                && iface_seen.insert((e.src, dst))
            {
                iface_adj.entry(e.src).or_default().push(i);
            }
        }

        // Origin call sites that link more than one distinct source
        // target are wrapper/loop machinery (see module docs) — findings
        // there are not actionable at the flagged line.
        let mut site_stmt = db.conn.prepare(
            "SELECT e.file_id, e.line, count(DISTINCT g.src_target)
             FROM tgt_edges g JOIN events e ON e.id = g.origin_event
             GROUP BY e.file_id, e.line",
        )?;
        let multi_target_sites: HashSet<(i64, i64)> = site_stmt
            .query_map([], |r| {
                Ok((
                    (r.get::<_, i64>(0)?, r.get::<_, i64>(1)?),
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?
            .into_iter()
            .filter(|&(_, n)| n > 1)
            .map(|(k, _)| k)
            .collect();
        let mut origin_site_stmt = db
            .conn
            .prepare("SELECT file_id, line FROM events WHERE id = ?1")?;

        // Direct candidate edges: resolved destination + project-file
        // origin, first such row per (src, dst) pair, in execution order.
        let mut in_source_cache: HashMap<i64, bool> = HashMap::new();
        let mut in_source = |db: &Db, ev: i64| {
            *in_source_cache
                .entry(ev)
                .or_insert_with(|| event_in_source(db, ev))
        };
        let mut direct: Vec<usize> = Vec::new();
        let mut direct_seen: HashSet<(i64, i64)> = HashSet::new();
        for (i, e) in edges.iter().enumerate() {
            let (Some(dst), Some(origin)) = (e.dst, e.origin) else {
                continue;
            };
            if dst != e.src && in_source(db, origin) && direct_seen.insert((e.src, dst)) {
                direct.push(i);
            }
        }
        let direct_by_src: HashMap<i64, Vec<usize>> = {
            let mut m: HashMap<i64, Vec<usize>> = HashMap::new();
            for &i in &direct {
                m.entry(edges[i].src).or_default().push(i);
            }
            m
        };

        let mut findings = Vec::new();
        for &bi in &direct {
            let edge_b = &edges[bi];
            let t = edge_b.src;
            let b = edge_b.dst.expect("direct edges are resolved");
            let Some(t_meta) = meta.get(&t) else { continue };
            // T must be a real linking target the project owns.
            if t_meta.imported
                || t_meta.alias_of.is_some()
                || matches!(
                    t_meta.ty.as_deref(),
                    Some("INTERFACE_LIBRARY") | Some("OBJECT_LIBRARY")
                )
                || cfg.ignored(&t_meta.name)
            {
                continue;
            }
            let Some(b_meta) = meta.get(&b) else { continue };
            if b_meta.imported || is_object(b) || cfg.ignored(&b_meta.name) {
                continue;
            }
            if edge_b.dst_text.contains("$<") {
                continue; // genex-bearing destination: not a trustworthy edge key
            }
            let origin_b = edge_b.origin.expect("direct edges have origins");
            if event_in_try_compile(db, origin_b) {
                continue;
            }
            let site: Option<(i64, i64)> = origin_site_stmt
                .query_row([origin_b], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok();
            if site
                .map(|s| multi_target_sites.contains(&s))
                .unwrap_or(true)
            {
                continue; // wrapper/loop machinery site (see module docs)
            }

            // Best providing chain: shortest, tie-broken by A's name.
            let mut best: Option<(usize, Vec<usize>)> = None; // (head edge, chain)
            for &ai in direct_by_src.get(&t).into_iter().flatten() {
                if ai == bi {
                    continue;
                }
                let edge_a = &edges[ai];
                let a = edge_a.dst.expect("direct edges are resolved");
                if a == b || is_object(a) || edge_a.dst_text.contains("$<") {
                    continue;
                }
                let Some(a_meta) = meta.get(&a) else { continue };
                if cfg.ignored(&a_meta.name) {
                    continue;
                }
                let Some(chain) = interface_path(&iface_adj, &edges, a, b, t, &is_object) else {
                    continue;
                };
                let better = match &best {
                    None => true,
                    Some((prev_ai, prev)) => {
                        let prev_a = &meta[&edges[*prev_ai].dst.unwrap()].name;
                        chain.len() < prev.len()
                            || (chain.len() == prev.len() && a_meta.name < *prev_a)
                    }
                };
                if better {
                    best = Some((ai, chain));
                }
            }
            let Some((ai, chain)) = best else { continue };

            let edge_a = &edges[ai];
            let a = edge_a.dst.unwrap();
            let a_name = &meta[&a].name;
            // "PUBLIC interface" reads naturally; "INTERFACE interface"
            // does not — phrase the INTERFACE-visibility head as propagation.
            let head_vis = match edges[chain[0]].visibility.as_str() {
                "PUBLIC" => "PUBLIC interface".to_string(),
                other => format!("{other} propagation"),
            };
            let rendered = {
                let mut s = a_name.clone();
                for &hop in &chain {
                    s.push_str(" -> ");
                    s.push_str(&meta[&edges[hop].dst.unwrap()].name);
                }
                s
            };
            let span_of = |o: Option<i64>| match o {
                Some(o) => event_span(db, o),
                None => crate::SourceSpan::new("<unknown>", 0),
            };
            let primary = span_of(Some(origin_b));
            let mut related = vec![(
                span_of(edge_a.origin),
                format!(
                    "`{}` links `{}` ({}) here — the providing dependency",
                    t_meta.name, a_name, edge_a.visibility
                ),
            )];
            for &hop in &chain {
                let e = &edges[hop];
                related.push((
                    span_of(e.origin),
                    format!(
                        "`{}` links `{}` ({}) here",
                        meta[&e.src].name,
                        meta[&e.dst.unwrap()].name,
                        e.visibility
                    ),
                ));
            }
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Note, // report-only by design (see module docs)
                message: format!(
                    "`{}` links `{}` directly ({}), but `{}` already arrives via `{}`'s \
                     {} ({}) — the direct edge is redundant for linking. \
                     Static-archive link ordering / ODR resolution can make the explicit \
                     edge intentional, so verify before removing",
                    t_meta.name, b_meta.name, primary, b_meta.name, a_name, head_vis, rendered
                ),
                primary,
                related,
                fix: None,
            });
        }
        Ok(findings)
    }
}

/// Shortest propagating chain `start -> ... -> goal` over PUBLIC/INTERFACE
/// edges, never through `forbidden` (the linking target) or an object
/// library, capped at [`MAX_CHAIN_HOPS`] hops. Returns edge indexes.
fn interface_path(
    iface_adj: &HashMap<i64, Vec<usize>>,
    edges: &[Edge],
    start: i64,
    goal: i64,
    forbidden: i64,
    is_object: &impl Fn(i64) -> bool,
) -> Option<Vec<usize>> {
    if start == forbidden || is_object(start) {
        return None;
    }
    // parent: node -> incoming edge index; also the visited set.
    let mut parent: HashMap<i64, usize> = HashMap::new();
    let mut queue: VecDeque<(i64, usize)> = VecDeque::from([(start, 0)]);
    while let Some((u, depth)) = queue.pop_front() {
        if depth >= MAX_CHAIN_HOPS {
            continue;
        }
        for &ei in iface_adj.get(&u).into_iter().flatten() {
            let v = edges[ei].dst.expect("interface edges are resolved");
            if v == forbidden || v == start || is_object(v) || parent.contains_key(&v) {
                continue;
            }
            parent.insert(v, ei);
            if v == goal {
                let mut rev = Vec::new();
                let mut cur = v;
                while cur != start {
                    let e = parent[&cur];
                    rev.push(e);
                    cur = edges[e].src;
                }
                rev.reverse();
                return Some(rev);
            }
            queue.push_back((v, depth + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(src: i64, dst: i64, vis: &str) -> Edge {
        Edge {
            src,
            dst_text: format!("t{dst}"),
            dst: Some(dst),
            visibility: vis.into(),
            origin: None,
        }
    }

    fn adj(edges: &[Edge]) -> HashMap<i64, Vec<usize>> {
        let mut m: HashMap<i64, Vec<usize>> = HashMap::new();
        for (i, e) in edges.iter().enumerate() {
            m.entry(e.src).or_default().push(i);
        }
        m
    }

    #[test]
    fn finds_shortest_chain_and_respects_guards() {
        // 1 -> 2 -> 3, and 1 -> 3 directly (shorter).
        let edges = vec![
            edge(1, 2, "PUBLIC"),
            edge(2, 3, "PUBLIC"),
            edge(1, 3, "INTERFACE"),
        ];
        let a = adj(&edges);
        let no_obj = |_: i64| false;
        let path = interface_path(&a, &edges, 1, 3, 99, &no_obj).unwrap();
        assert_eq!(path, vec![2]); // the direct 1 -> 3 edge

        // Forbidden node blocks the only route.
        let edges2 = vec![edge(1, 5, "PUBLIC"), edge(5, 3, "PUBLIC")];
        let a2 = adj(&edges2);
        assert!(interface_path(&a2, &edges2, 1, 3, 5, &no_obj).is_none());

        // Object library blocks the route.
        let obj5 = |t: i64| t == 5;
        assert!(interface_path(&a2, &edges2, 1, 3, 99, &obj5).is_none());
    }

    #[test]
    fn cycles_terminate_and_depth_is_capped() {
        // 1 <-> 2 cycle with no route to 9.
        let edges = vec![edge(1, 2, "PUBLIC"), edge(2, 1, "PUBLIC")];
        let a = adj(&edges);
        let no_obj = |_: i64| false;
        assert!(interface_path(&a, &edges, 1, 9, 99, &no_obj).is_none());

        // A 10-hop chain exceeds the cap; a 3-hop chain does not.
        let long: Vec<Edge> = (0..10).map(|i| edge(i, i + 1, "PUBLIC")).collect();
        let al = adj(&long);
        assert!(interface_path(&al, &long, 0, 10, 99, &no_obj).is_none());
        assert_eq!(
            interface_path(&al, &long, 0, 3, 99, &no_obj).map(|p| p.len()),
            Some(3)
        );
    }
}
