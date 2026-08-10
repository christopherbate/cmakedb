//! `cmakedb diff` (design §4.7): compare two recordings.

use crate::Db;
use anyhow::Result;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

pub struct DiffReport {
    pub added_targets: Vec<String>,
    pub removed_targets: Vec<String>,
    pub added_edges: Vec<String>,
    pub removed_edges: Vec<String>,
    /// target -> (kind, added values, removed values)
    pub changed_reqs: Vec<(String, String, Vec<String>, Vec<String>)>,
}

fn targets(db: &Db) -> Result<BTreeSet<String>> {
    let mut stmt = db
        .conn
        .prepare("SELECT name FROM targets WHERE alias_of IS NULL")?;
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows.into_iter().collect())
}

fn edges(db: &Db) -> Result<BTreeSet<String>> {
    let mut stmt = db.conn.prepare(
        "SELECT s.name || ' -[' || e.visibility || ']-> ' || e.dst
         FROM tgt_edges e JOIN targets s ON s.id = e.src_target",
    )?;
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows.into_iter().collect())
}

/// Final resolved requirement sets per target (File API rows), aligned by
/// target name (§4.7).
fn final_reqs(db: &Db) -> Result<BTreeMap<(String, String), BTreeSet<String>>> {
    let mut stmt = db.conn.prepare(
        "SELECT t.name, r.kind, r.value FROM usage_reqs r
         JOIN targets t ON t.id = r.target_id WHERE r.source = 'fileapi'",
    )?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (t, k, v) in rows {
        out.entry((t, k)).or_default().insert(v);
    }
    Ok(out)
}

pub fn diff(a: &Db, b: &Db) -> Result<DiffReport> {
    let (ta, tb) = (targets(a)?, targets(b)?);
    let (ea, eb) = (edges(a)?, edges(b)?);
    let (ra, rb) = (final_reqs(a)?, final_reqs(b)?);

    let mut changed_reqs = Vec::new();
    let keys: BTreeSet<_> = ra.keys().chain(rb.keys()).cloned().collect();
    for key in keys {
        let empty = BTreeSet::new();
        let va = ra.get(&key).unwrap_or(&empty);
        let vb = rb.get(&key).unwrap_or(&empty);
        // Only report for targets present in both (add/remove covers the rest).
        if !ta.contains(&key.0) || !tb.contains(&key.0) {
            continue;
        }
        let added: Vec<String> = vb.difference(va).cloned().collect();
        let removed: Vec<String> = va.difference(vb).cloned().collect();
        if !added.is_empty() || !removed.is_empty() {
            changed_reqs.push((key.0.clone(), key.1.clone(), added, removed));
        }
    }

    Ok(DiffReport {
        added_targets: tb.difference(&ta).cloned().collect(),
        removed_targets: ta.difference(&tb).cloned().collect(),
        added_edges: eb.difference(&ea).cloned().collect(),
        removed_edges: ea.difference(&eb).cloned().collect(),
        changed_reqs,
    })
}

impl DiffReport {
    pub fn is_empty(&self) -> bool {
        self.added_targets.is_empty()
            && self.removed_targets.is_empty()
            && self.added_edges.is_empty()
            && self.removed_edges.is_empty()
            && self.changed_reqs.is_empty()
    }

    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let section = |out: &mut String, title: &str, items: &[String], sign: char| {
            if !items.is_empty() {
                out.push_str(&format!("{title}:\n"));
                for i in items {
                    out.push_str(&format!("  {sign} {i}\n"));
                }
            }
        };
        section(&mut out, "targets added", &self.added_targets, '+');
        section(&mut out, "targets removed", &self.removed_targets, '-');
        section(&mut out, "link edges added", &self.added_edges, '+');
        section(&mut out, "link edges removed", &self.removed_edges, '-');
        for (t, kind, added, removed) in &self.changed_reqs {
            out.push_str(&format!("{t}: final {kind} set changed:\n"));
            for v in added {
                out.push_str(&format!("  + {v}\n"));
            }
            for v in removed {
                out.push_str(&format!("  - {v}\n"));
            }
        }
        if self.is_empty() {
            out.push_str("recordings are equivalent (targets, edges, resolved requirements)\n");
        }
        out
    }

    pub fn render_json(&self) -> String {
        serde_json::to_string_pretty(&json!({
            "added_targets": self.added_targets,
            "removed_targets": self.removed_targets,
            "added_edges": self.added_edges,
            "removed_edges": self.removed_edges,
            "changed_requirements": self.changed_reqs.iter().map(|(t, k, a, r)| json!({
                "target": t, "kind": k, "added": a, "removed": r
            })).collect::<Vec<_>>(),
        }))
        .expect("serialize")
    }
}

/// Build-graph isomorphism check for the codemod verification loop
/// (design §4.6 step 6): same target set and same *resolved* requirement
/// sets (File API rows). Trace-side rows (edges, as-written requirements)
/// are intentionally excluded — a codemod legitimately rewrites how a
/// requirement is expressed, not what it resolves to.
pub fn graph_isomorphism_diff(a: &Db, b: &Db) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let (ta, tb) = (targets(a)?, targets(b)?);
    for t in ta.difference(&tb) {
        problems.push(format!("target disappeared: {t}"));
    }
    for t in tb.difference(&ta) {
        problems.push(format!("target appeared: {t}"));
    }
    let (ra, rb) = (final_reqs(a)?, final_reqs(b)?);
    let keys: BTreeSet<_> = ra.keys().chain(rb.keys()).cloned().collect();
    for key in keys {
        if !ta.contains(&key.0) || !tb.contains(&key.0) {
            continue; // already reported above
        }
        let empty = BTreeSet::new();
        let va = ra.get(&key).unwrap_or(&empty);
        let vb = rb.get(&key).unwrap_or(&empty);
        for v in va.difference(vb) {
            problems.push(format!("{}: lost {} '{}'", key.0, key.1, v));
        }
        for v in vb.difference(va) {
            problems.push(format!("{}: gained {} '{}'", key.0, key.1, v));
        }
    }
    Ok(problems)
}
