//! Provenance walks (design §4.3): why-links, why-includes, why-flag.
//!
//! The final closure comes from the File API; the explanation joins each
//! closure member back to origin events on `tgt_edges` / `usage_reqs`, so
//! every hop carries a real source location — including hops introduced by
//! third-party Find modules.

use anyhow::{bail, Result};
use cmakedb_db::Db;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct Hop {
    pub from: String,
    pub to: String,
    pub visibility: String,
    /// Source location of the originating call, if joined.
    pub origin: Option<String>,
}

/// A tree of propagation paths, shared prefixes collapsed (§4.3).
#[derive(Debug, Clone, Serialize)]
pub struct ExplainNode {
    pub label: String,
    pub visibility: Option<String>,
    pub origin: Option<String>,
    pub children: Vec<ExplainNode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Explanation {
    pub target: String,
    pub query: String,
    /// All distinct propagation paths, shortest first.
    pub paths: Vec<Vec<Hop>>,
    pub tree: ExplainNode,
    /// Final-link evidence from the File API (resolved fragments).
    pub final_evidence: Vec<String>,
}

struct Edge {
    src: i64,
    dst_text: String,
    dst_target: Option<i64>,
    visibility: String,
    origin: Option<String>,
}

fn load_edges(db: &Db) -> Result<Vec<Edge>> {
    let mut stmt = db.conn.prepare(
        "SELECT e.src_target, e.dst, e.dst_target, e.visibility, e.origin_event
         FROM tgt_edges e ORDER BY e.id",
    )?;
    let rows: Vec<(i64, String, Option<i64>, String, Option<i64>)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .map(|(src, dst, dst_target, visibility, origin_event)| Edge {
            src,
            dst_text: dst,
            dst_target,
            visibility,
            origin: origin_event.and_then(|e| db.event_location(e).ok()),
        })
        .collect())
}

fn target_names(db: &Db) -> Result<HashMap<i64, String>> {
    let mut stmt = db.conn.prepare("SELECT id, name FROM targets")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = HashMap::new();
    for r in rows {
        let (id, name) = r?;
        out.insert(id, name);
    }
    Ok(out)
}

/// Does an edge destination (as written) refer to the queried library?
/// Matches target names exactly and external names structurally
/// (`z` ~ `libz.so`, `-lz`, `/usr/lib/libz.dylib`, `ZLIB::ZLIB` suffix).
pub(crate) fn dst_matches(dst_text: &str, dst_name: Option<&str>, query: &str) -> bool {
    if dst_text == query || dst_name == Some(query) {
        return true;
    }
    let base = dst_text
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(dst_text)
        .trim_start_matches("-l");
    if base == query {
        return true;
    }
    for ext in [".a", ".so", ".dylib", ".lib", ".tbd"] {
        if let Some(stem) = base.strip_suffix(ext) {
            let stem = stem.split(".so.").next().unwrap_or(stem);
            if stem == query || stem.strip_prefix("lib") == Some(query) {
                return true;
            }
        }
    }
    // Namespaced target: match last component case-insensitively (z ~ ZLIB::ZLIB is
    // NOT matched here; but Foo::bar matches query "bar").
    if let Some((_, last)) = dst_text.rsplit_once("::") {
        if last.eq_ignore_ascii_case(query) {
            return true;
        }
    }
    false
}

/// why-links / generic dependency provenance (§4.3).
pub fn why_links(db: &Db, target: &str, dep: &str) -> Result<Explanation> {
    let Some(tid) = db.target_id(target)? else {
        bail!("unknown target '{target}' (not defined in this recording)");
    };
    let edges = load_edges(db)?;
    let names = target_names(db)?;

    // adjacency by src
    let mut adj: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        adj.entry(e.src).or_default().push(i);
    }

    // DFS over simple paths; after the first hop only PUBLIC/INTERFACE
    // edges propagate to the root target's link line.
    let mut paths: Vec<Vec<Hop>> = Vec::new();
    let mut stack_path: Vec<usize> = Vec::new();
    let mut visited: Vec<i64> = vec![tid];

    fn dfs(
        cur: i64,
        query: &str,
        edges: &[Edge],
        adj: &HashMap<i64, Vec<usize>>,
        names: &HashMap<i64, String>,
        stack_path: &mut Vec<usize>,
        visited: &mut Vec<i64>,
        paths: &mut Vec<Vec<Hop>>,
    ) {
        if paths.len() >= 64 || stack_path.len() >= 32 {
            return;
        }
        for &ei in adj.get(&cur).map(|v| v.as_slice()).unwrap_or(&[]) {
            let e = &edges[ei];
            // propagation rule: edges beyond the first hop must be
            // PUBLIC/INTERFACE on the *intermediate* target's side.
            if !stack_path.is_empty() && e.visibility == "PRIVATE" {
                continue;
            }
            let dst_name = e.dst_target.and_then(|t| names.get(&t).map(|s| s.as_str()));
            stack_path.push(ei);
            if dst_matches(&e.dst_text, dst_name, query) {
                paths.push(
                    stack_path
                        .iter()
                        .map(|&i| {
                            let e = &edges[i];
                            Hop {
                                from: names.get(&e.src).cloned().unwrap_or_default(),
                                to: e
                                    .dst_target
                                    .and_then(|t| names.get(&t).cloned())
                                    .unwrap_or_else(|| e.dst_text.clone()),
                                visibility: e.visibility.clone(),
                                origin: e.origin.clone(),
                            }
                        })
                        .collect(),
                );
            } else if let Some(dst) = e.dst_target {
                if !visited.contains(&dst) {
                    visited.push(dst);
                    dfs(dst, query, edges, adj, names, stack_path, visited, paths);
                    visited.pop();
                }
            }
            stack_path.pop();
        }
    }
    dfs(
        tid,
        dep,
        &edges,
        &adj,
        &names,
        &mut stack_path,
        &mut visited,
        &mut paths,
    );
    paths.sort_by_key(|p| p.len());

    // Final-link evidence from the resolved File API link line.
    let mut evidence = Vec::new();
    {
        let mut stmt = db.conn.prepare(
            "SELECT r.value FROM usage_reqs r JOIN targets t ON t.id = r.target_id
             WHERE t.id = ?1 AND r.kind = 'link' AND r.source = 'fileapi'",
        )?;
        let rows: Vec<String> = stmt
            .query_map([tid], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for v in rows {
            if dst_matches(&v, None, dep) {
                evidence.push(v);
            }
        }
    }

    if paths.is_empty() && evidence.is_empty() {
        bail!(
            "no link path from '{target}' to '{dep}' found in this recording \
             (checked {} edges)",
            edges.len()
        );
    }

    Ok(Explanation {
        tree: collapse(target, &paths),
        target: target.into(),
        query: dep.into(),
        paths,
        final_evidence: evidence,
    })
}

/// why-includes / why-flag (§4.3): explain how a usage requirement reached
/// a target. `kind` is include|define|option|feature.
pub fn why_requirement(db: &Db, target: &str, kind: &str, value: &str) -> Result<Explanation> {
    let Some(tid) = db.target_id(target)? else {
        bail!("unknown target '{target}'");
    };
    // Which targets declare this requirement (as written, with visibility)?
    let mut stmt = db.conn.prepare(
        "SELECT r.target_id, t.name, r.visibility, r.origin_event
         FROM usage_reqs r JOIN targets t ON t.id = r.target_id
         WHERE r.kind = ?1 AND r.source = 'trace'
           AND (r.value = ?2 OR r.value LIKE '%' || ?2 || '%')
         ORDER BY r.id",
    )?;
    let declarers: Vec<(i64, String, Option<String>, Option<i64>)> = stmt
        .query_map(rusqlite::params![kind, value], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    // Final resolution evidence from the File API.
    let mut evidence = Vec::new();
    {
        let mut stmt = db.conn.prepare(
            "SELECT r.value FROM usage_reqs r
             WHERE r.target_id = ?1 AND r.kind = ?2 AND r.source = 'fileapi'
               AND (r.value = ?3 OR r.value LIKE '%' || ?3 || '%')",
        )?;
        let rows: Vec<String> = stmt
            .query_map(rusqlite::params![tid, kind, value], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        evidence.extend(rows);
    }

    let mut paths: Vec<Vec<Hop>> = Vec::new();
    for (decl_tid, decl_name, vis, origin) in &declarers {
        let origin_loc = origin.and_then(|e| db.event_location(e).ok());
        let final_hop = Hop {
            from: decl_name.clone(),
            to: format!("{kind} '{value}'"),
            visibility: vis.clone().unwrap_or_default(),
            origin: origin_loc,
        };
        if *decl_tid == tid {
            paths.push(vec![final_hop]);
        } else {
            // Path from the queried target to the declaring target; the
            // requirement propagates only if declared PUBLIC/INTERFACE.
            if vis.as_deref() == Some("PRIVATE") {
                continue;
            }
            if let Ok(link) = why_links(db, target, decl_name) {
                for mut p in link.paths {
                    p.push(final_hop.clone());
                    paths.push(p);
                }
            }
        }
    }
    paths.sort_by_key(|p| p.len());
    if paths.is_empty() && evidence.is_empty() {
        bail!("no {kind} '{value}' found on '{target}' in this recording");
    }
    Ok(Explanation {
        tree: collapse(target, &paths),
        target: target.into(),
        query: value.into(),
        paths,
        final_evidence: evidence,
    })
}

/// Collapse shared path prefixes into a tree (§4.3).
fn collapse(root: &str, paths: &[Vec<Hop>]) -> ExplainNode {
    fn insert(node: &mut ExplainNode, path: &[Hop]) {
        let Some(hop) = path.first() else { return };
        let child = node.children.iter_mut().position(|c| {
            c.label == hop.to && c.visibility.as_deref() == Some(hop.visibility.as_str())
        });
        let idx = match child {
            Some(i) => i,
            None => {
                node.children.push(ExplainNode {
                    label: hop.to.clone(),
                    visibility: Some(hop.visibility.clone()),
                    origin: hop.origin.clone(),
                    children: vec![],
                });
                node.children.len() - 1
            }
        };
        insert(&mut node.children[idx], &path[1..]);
    }
    let mut root_node = ExplainNode {
        label: root.to_string(),
        visibility: None,
        origin: None,
        children: vec![],
    };
    for p in paths {
        insert(&mut root_node, p);
    }
    root_node
}

/// Render the explanation tree as indented text with box-drawing, matching
/// the §2.2 example session style.
pub fn render_tree(
    node: &ExplainNode,
    out: &mut String,
    prefix: &str,
    is_last: bool,
    depth: usize,
) {
    let connector = if depth == 0 {
        String::new()
    } else if is_last {
        format!("{prefix}└─ ")
    } else {
        format!("{prefix}├─ ")
    };
    let vis = node
        .visibility
        .as_ref()
        .map(|v| format!("{v} "))
        .unwrap_or_default();
    let origin = node
        .origin
        .as_ref()
        .map(|o| format!("    [{o}]"))
        .unwrap_or_default();
    out.push_str(&format!("{connector}{vis}{}{origin}\n", node.label));
    let child_prefix = if depth == 0 {
        String::new()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}│  ")
    };
    let n = node.children.len();
    for (i, c) in node.children.iter().enumerate() {
        render_tree(c, out, &child_prefix, i + 1 == n, depth + 1);
    }
}
