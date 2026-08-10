//! `cmakedb graph`: dependency-graph export with provenance (ROADMAP
//! Track A). Unlike `cmake --graphviz`, every edge carries its as-written
//! visibility and the `file:line` of the call that created it.
//!
//! Nodes are the recording's non-alias targets plus (by default) external
//! link destinations — names that never resolved to a target in this
//! recording (`z`, `-lfoo`, `/usr/lib/libssl.dylib`). Edges are `tgt_edges`
//! rows in execution order; parallel edges are kept because each carries
//! its own origin. `--target` limits output to that target's *forward*
//! closure, followed through every visibility (a PRIVATE hop still makes
//! the dependency exist; propagation semantics belong to why-links).

use anyhow::{bail, Result};
use clap::ValueEnum;
use cmakedb_db::Db;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Clone, Copy, ValueEnum)]
pub enum GraphFormat {
    Dot,
    Mermaid,
    Json,
}

#[derive(Debug, Serialize)]
pub struct Graph {
    pub cmakedb_graph_version: u32,
    pub nodes: Vec<Node>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Serialize)]
pub struct Node {
    pub name: String,
    /// EXECUTABLE|STATIC_LIBRARY|...; null when unknown or external.
    #[serde(rename = "type")]
    pub ttype: Option<String>,
    pub imported: bool,
    /// True for link destinations that are not targets in this recording.
    pub external: bool,
    /// `file:line` of the defining call, when recorded.
    pub location: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GraphEdge {
    pub src: String,
    pub dst: String,
    /// PUBLIC | PRIVATE | INTERFACE, as written.
    pub visibility: String,
    /// `file:line` of the call that created the edge, when joined.
    pub origin: Option<String>,
}

/// Build the (optionally filtered) graph. `paths` reuses the
/// `filter_findings` display-path prefix semantics from main.rs: matching
/// is by path component against source-relative locations, and it applies
/// to *targets* only (externals have no defining location — they are kept
/// while an edge into them survives).
pub fn build(
    db: &Db,
    target: Option<&str>,
    paths: &[String],
    include_external: bool,
) -> Result<Graph> {
    // Non-alias targets (aliases are folded into their real target by
    // ingestion; both edge endpoints are alias-resolved ids).
    struct Target {
        name: String,
        ttype: Option<String>,
        imported: bool,
        location: Option<String>,
    }
    type TargetRow = (i64, String, Option<String>, i64, Option<i64>);
    let mut stmt = db.conn.prepare(
        "SELECT id, name, type, imported, defined_event
         FROM targets WHERE alias_of IS NULL ORDER BY name",
    )?;
    let rows: Vec<TargetRow> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut targets: HashMap<i64, Target> = HashMap::new();
    let mut order: Vec<i64> = Vec::new(); // name order from the query
    for (id, name, ttype, imported, defined_event) in rows {
        order.push(id);
        targets.insert(
            id,
            Target {
                name,
                ttype,
                imported: imported != 0,
                location: defined_event.and_then(|e| db.event_location(e).ok()),
            },
        );
    }

    // Edges in execution order.
    type EdgeRow = (i64, String, Option<i64>, String, Option<i64>);
    let mut stmt = db.conn.prepare(
        "SELECT src_target, dst, dst_target, visibility, origin_event
         FROM tgt_edges ORDER BY id",
    )?;
    let edges: Vec<EdgeRow> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    // --target: forward closure by BFS over ALL edges regardless of
    // visibility (deliberately: this answers "what does X depend on",
    // not "what reaches X's link line" — that is why-links' job).
    let mut kept: HashSet<i64> = if let Some(name) = target {
        let Some(root) = db.target_id(name)? else {
            bail!("unknown target '{name}' (not defined in this recording)");
        };
        let mut adj: HashMap<i64, Vec<i64>> = HashMap::new();
        for (src, _, dst, _, _) in &edges {
            if let Some(d) = dst {
                adj.entry(*src).or_default().push(*d);
            }
        }
        let mut seen = HashSet::from([root]);
        let mut queue = VecDeque::from([root]);
        while let Some(t) = queue.pop_front() {
            for &d in adj.get(&t).map(|v| v.as_slice()).unwrap_or(&[]) {
                if seen.insert(d) {
                    queue.push_back(d);
                }
            }
        }
        seen
    } else {
        targets.keys().copied().collect()
    };

    // --path: keep targets whose defining location falls under a prefix
    // (same component-wise semantics as `filter_findings` in main.rs).
    // Targets with no recorded definition site cannot match a prefix.
    if !paths.is_empty() {
        let normalized: Vec<String> = paths
            .iter()
            .map(|p| {
                cmakedb_db::cmake_path_spelling(p)
                    .trim_start_matches("./")
                    .trim_end_matches('/')
                    .to_string()
            })
            .collect();
        kept.retain(|id| {
            let Some(loc) = targets[id].location.as_deref() else {
                return false;
            };
            let file = loc.rsplit_once(':').map(|(f, _)| f).unwrap_or(loc);
            let file = file.trim_start_matches("./");
            normalized
                .iter()
                .any(|p| file == p || file.starts_with(&format!("{p}/")))
        });
    }

    // Surviving edges; externals appear only via an edge that kept them.
    let mut out_edges = Vec::new();
    let mut externals: Vec<String> = Vec::new();
    let mut external_seen: HashSet<String> = HashSet::new();
    for (src, dst_text, dst_target, visibility, origin_event) in &edges {
        if !kept.contains(src) {
            continue;
        }
        let dst_name = match dst_target {
            Some(d) if kept.contains(d) => targets[d].name.clone(),
            Some(_) => continue, // resolved to a target filtered out
            None => {
                if !include_external {
                    continue;
                }
                if external_seen.insert(dst_text.clone()) {
                    externals.push(dst_text.clone());
                }
                dst_text.clone()
            }
        };
        out_edges.push(GraphEdge {
            src: targets[src].name.clone(),
            dst: dst_name,
            visibility: visibility.clone(),
            origin: origin_event.and_then(|e| db.event_location(e).ok()),
        });
    }

    let mut nodes: Vec<Node> = order
        .iter()
        .filter(|id| kept.contains(id))
        .map(|id| {
            let t = &targets[id];
            Node {
                name: t.name.clone(),
                ttype: t.ttype.clone(),
                imported: t.imported,
                external: false,
                location: t.location.clone(),
            }
        })
        .collect();
    externals.sort();
    nodes.extend(externals.into_iter().map(|name| Node {
        name,
        ttype: None,
        imported: false,
        external: true,
        location: None,
    }));

    Ok(Graph {
        cmakedb_graph_version: 1,
        nodes,
        edges: out_edges,
    })
}

pub fn render(g: &Graph, format: GraphFormat) -> Result<String> {
    Ok(match format {
        GraphFormat::Dot => render_dot(g),
        GraphFormat::Mermaid => render_mermaid(g),
        GraphFormat::Json => {
            let mut s = serde_json::to_string_pretty(g)?;
            s.push('\n');
            s
        }
    })
}

/// Escape for a DOT double-quoted string ("quoted ID"): backslash and
/// double quote. Namespaced names like `ZLIB::ZLIB` need no escaping once
/// quoted, but always quoting keeps every name (spaces, `-l` flags,
/// absolute paths) a valid ID.
fn esc_dot(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn render_dot(g: &Graph) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    out.push_str("// cmakedb dependency graph (as-written visibility + origins)\n");
    out.push_str("// edges: solid = PUBLIC, dashed = PRIVATE, dotted = INTERFACE;\n");
    out.push_str("//        the tooltip carries \"VISIBILITY origin-file:line\"\n");
    out.push_str("// nodes: box = executable, ellipse = library, dashed = interface,\n");
    out.push_str("//        filled = imported, octagon = external (not a target here)\n");
    out.push_str("digraph cmakedb {\n  rankdir=LR;\n");
    for n in &g.nodes {
        let mut attrs = Vec::new();
        let mut styles = Vec::new();
        if n.external {
            attrs.push("shape=octagon".to_string());
            styles.push("dashed");
        } else if n.ttype.as_deref() == Some("EXECUTABLE") {
            attrs.push("shape=box".to_string());
        } else {
            attrs.push("shape=ellipse".to_string());
            if n.ttype.as_deref() == Some("INTERFACE_LIBRARY") {
                styles.push("dashed");
            }
        }
        if n.imported {
            styles.push("filled");
            attrs.push("fillcolor=\"#dddddd\"".to_string());
        }
        if !styles.is_empty() {
            attrs.push(format!("style=\"{}\"", styles.join(",")));
        }
        let mut label = n.name.clone();
        if let Some(t) = &n.ttype {
            label.push('\n');
            label.push_str(t);
        } else if n.external {
            label.push_str("\n(external)");
        }
        // String-escape first, then turn the real newline into DOT's
        // literal `\n` line-break escape.
        let label = esc_dot(&label).replace('\n', "\\n");
        attrs.push(format!("label=\"{label}\""));
        if let Some(loc) = &n.location {
            attrs.push(format!("tooltip=\"{}\"", esc_dot(loc)));
        }
        let _ = writeln!(out, "  \"{}\" [{}];", esc_dot(&n.name), attrs.join(", "));
    }
    for e in &g.edges {
        let style = match e.visibility.as_str() {
            "PRIVATE" => "dashed",
            "INTERFACE" => "dotted",
            _ => "solid",
        };
        let tooltip = match &e.origin {
            Some(o) => format!("{} {}", e.visibility, o),
            None => e.visibility.clone(),
        };
        let _ = writeln!(
            out,
            "  \"{}\" -> \"{}\" [style={style}, tooltip=\"{}\"];",
            esc_dot(&e.src),
            esc_dot(&e.dst),
            esc_dot(&tooltip)
        );
    }
    out.push_str("}\n");
    out
}

/// Escape for a Mermaid quoted label: double quotes become `#quot;`
/// (Mermaid's HTML-entity escape); everything else is safe inside quotes.
fn esc_mermaid(s: &str) -> String {
    s.replace('"', "#quot;")
}

fn render_mermaid(g: &Graph) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    out.push_str("%% cmakedb dependency graph (as-written visibility + origins)\n");
    out.push_str("%% edges: --> PUBLIC (solid), -.-> PRIVATE (dashed), ==> INTERFACE\n");
    out.push_str("%%        (thick; Mermaid has no third dash pattern) — the link text\n");
    out.push_str("%%        carries \"VISIBILITY origin-file:line\" either way\n");
    out.push_str("%% nodes: [name] executable, (name) library, {{name}} external;\n");
    out.push_str("%%        classes mark imported / interface targets\n");
    out.push_str("graph LR\n");
    out.push_str("  classDef imported fill:#ddd,stroke:#666;\n");
    out.push_str("  classDef interface stroke-dasharray: 5 5;\n");
    out.push_str("  classDef external stroke-dasharray: 2 3;\n");

    // Names can contain characters Mermaid IDs cannot (`::`), so nodes get
    // synthetic ids n0, n1, ... and quoted display labels.
    let ids: HashMap<&str, String> = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.name.as_str(), format!("n{i}")))
        .collect();
    let mut by_class: HashMap<&str, Vec<String>> = HashMap::new();
    for n in &g.nodes {
        let id = &ids[n.name.as_str()];
        let label = esc_mermaid(&n.name);
        let line = if n.external {
            by_class.entry("external").or_default().push(id.clone());
            format!("  {id}{{{{\"{label}\"}}}}\n")
        } else if n.ttype.as_deref() == Some("EXECUTABLE") {
            format!("  {id}[\"{label}\"]\n")
        } else {
            if n.ttype.as_deref() == Some("INTERFACE_LIBRARY") {
                by_class.entry("interface").or_default().push(id.clone());
            }
            format!("  {id}(\"{label}\")\n")
        };
        if n.imported {
            by_class.entry("imported").or_default().push(id.clone());
        }
        out.push_str(&line);
    }
    for e in &g.edges {
        let (open, close) = match e.visibility.as_str() {
            "PRIVATE" => ("-.", ".->"),
            "INTERFACE" => ("==", "==>"),
            _ => ("--", "-->"),
        };
        let text = match &e.origin {
            Some(o) => format!("{} {}", e.visibility, o),
            None => e.visibility.clone(),
        };
        let _ = writeln!(
            out,
            "  {} {open} \"{}\" {close} {}",
            ids[e.src.as_str()],
            esc_mermaid(&text),
            ids[e.dst.as_str()]
        );
    }
    for class in ["imported", "interface", "external"] {
        if let Some(nodes) = by_class.get(class) {
            let _ = writeln!(out, "  class {} {class};", nodes.join(","));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, ttype: Option<&str>, imported: bool, external: bool) -> Node {
        Node {
            name: name.into(),
            ttype: ttype.map(|s| s.into()),
            imported,
            external,
            location: None,
        }
    }

    /// Names with quotes, backslashes, and `::` must stay inside one
    /// quoted DOT ID / Mermaid label.
    #[test]
    fn escaping_hostile_names() {
        let g = Graph {
            cmakedb_graph_version: 1,
            nodes: vec![
                node("evil\"name\\x", Some("EXECUTABLE"), false, false),
                node("ZLIB::ZLIB", None, false, true),
            ],
            edges: vec![GraphEdge {
                src: "evil\"name\\x".into(),
                dst: "ZLIB::ZLIB".into(),
                visibility: "PRIVATE".into(),
                origin: Some("a \"b\"/CMakeLists.txt:1".into()),
            }],
        };
        let dot = render_dot(&g);
        assert!(dot.contains(r#""evil\"name\\x""#), "{dot}");
        assert!(dot.contains(r#""ZLIB::ZLIB""#), "{dot}");
        assert!(
            dot.contains(r#"tooltip="PRIVATE a \"b\"/CMakeLists.txt:1""#),
            "{dot}"
        );
        let mmd = render_mermaid(&g);
        assert!(mmd.contains(r#"n0["evil#quot;name\x"]"#), "{mmd}");
        assert!(mmd.contains(r#"n1{{"ZLIB::ZLIB"}}"#), "{mmd}");
        // No unescaped double quote may survive inside a Mermaid label.
        assert!(
            mmd.contains(r#"-. "PRIVATE a #quot;b#quot;/CMakeLists.txt:1" .->"#),
            "{mmd}"
        );
    }

    #[test]
    fn dot_edge_styles_follow_visibility() {
        let mk = |vis: &str| GraphEdge {
            src: "a".into(),
            dst: "b".into(),
            visibility: vis.into(),
            origin: None,
        };
        let g = Graph {
            cmakedb_graph_version: 1,
            nodes: vec![node("a", None, false, false), node("b", None, false, false)],
            edges: vec![mk("PUBLIC"), mk("PRIVATE"), mk("INTERFACE")],
        };
        let dot = render_dot(&g);
        assert!(dot.contains("[style=solid, tooltip=\"PUBLIC\"]"), "{dot}");
        assert!(dot.contains("[style=dashed, tooltip=\"PRIVATE\"]"), "{dot}");
        assert!(
            dot.contains("[style=dotted, tooltip=\"INTERFACE\"]"),
            "{dot}"
        );
    }
}
