//! File API (`codemodel-v2`) ingestion: the final resolved build graph,
//! joined back to trace events via backtraces (design §3.2 channel 3, §3.3).

use anyhow::{Context, Result};
use rusqlite::{params, Transaction};
use serde_json::Value;
use std::path::Path;

use super::dataflow::CollectedGraph;
use super::{FileTable, IngestStats};

pub fn ingest(
    tx: &Transaction,
    reply_dir: &Path,
    files: &FileTable,
    collected: &CollectedGraph,
    stats: &mut IngestStats,
) -> Result<()> {
    // Find the newest index file.
    let mut index_files: Vec<_> = std::fs::read_dir(reply_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("index-") && n.ends_with(".json")
                })
                .unwrap_or(false)
        })
        .collect();
    index_files.sort();
    let Some(index_path) = index_files.pop() else {
        return Ok(()); // no reply; File API data simply absent
    };
    let index: Value = read_json(&index_path)?;

    let mut codemodel_file = None;
    if let Some(objects) = index.get("objects").and_then(|o| o.as_array()) {
        for o in objects {
            if o.get("kind").and_then(|k| k.as_str()) == Some("codemodel") {
                codemodel_file = o.get("jsonFile").and_then(|f| f.as_str()).map(String::from);
            }
        }
    }
    let Some(codemodel_file) = codemodel_file else {
        return Ok(());
    };
    let codemodel: Value = read_json(&reply_dir.join(&codemodel_file))?;

    let Some(config) = codemodel
        .get("configurations")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
    else {
        return Ok(());
    };
    if let Some(name) = config.get("name").and_then(|n| n.as_str()) {
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('config', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            params![name],
        )?;
    }

    let mut upsert_target = tx.prepare(
        "INSERT INTO targets(name, type, defined_event, in_file_api)
         VALUES (?1, ?2, ?3, 1)
         ON CONFLICT(name) DO UPDATE SET
           type = COALESCE(excluded.type, targets.type),
           defined_event = COALESCE(targets.defined_event, excluded.defined_event),
           in_file_api = 1",
    )?;
    let mut ins_req = tx.prepare(
        "INSERT INTO usage_reqs(target_id, kind, value, visibility, source, origin_event)
         VALUES (?1, ?2, ?3, NULL, 'fileapi', ?4)",
    )?;
    let mut ins_tu = tx.prepare("INSERT INTO tus(target_id, source_path) VALUES (?1, ?2)")?;

    for tref in config
        .get("targets")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
    {
        let Some(json_file) = tref.get("jsonFile").and_then(|f| f.as_str()) else {
            continue;
        };
        let t: Value = read_json(&reply_dir.join(json_file))
            .with_context(|| format!("reading target reply {json_file}"))?;
        let Some(name) = t.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let ttype = t.get("type").and_then(|x| x.as_str());

        // Backtrace of the defining command -> event.
        let bt = Backtraces::new(&t);
        let defined_event = t
            .get("backtrace")
            .and_then(|b| b.as_u64())
            .and_then(|i| bt.resolve(i as usize))
            .and_then(|(file, line)| lookup_event(files, collected, &file, line));

        upsert_target.execute(params![name, ttype, defined_event])?;
        let target_id: i64 =
            tx.query_row("SELECT id FROM targets WHERE name = ?1", [name], |r| {
                r.get(0)
            })?;

        // Final resolved includes/defines per compile group.
        let mut seen = std::collections::HashSet::new();
        for cg in t
            .get("compileGroups")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
        {
            for inc in cg
                .get("includes")
                .and_then(|i| i.as_array())
                .into_iter()
                .flatten()
            {
                let Some(path) = inc.get("path").and_then(|p| p.as_str()) else {
                    continue;
                };
                let origin = inc
                    .get("backtrace")
                    .and_then(|b| b.as_u64())
                    .and_then(|i| bt.resolve(i as usize))
                    .and_then(|(f, l)| lookup_event(files, collected, &f, l));
                if seen.insert(("include", path.to_string(), origin)) {
                    ins_req.execute(params![target_id, "include", path, origin])?;
                }
            }
            // Compile-option fragments (no backtraces in the File API);
            // needed as the cross-check for the add_compile_options codemod.
            for frag in cg
                .get("compileCommandFragments")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
            {
                let Some(f) = frag.get("fragment").and_then(|p| p.as_str()) else {
                    continue;
                };
                let f = f.trim();
                if !f.is_empty() && seen.insert(("option", f.to_string(), None)) {
                    ins_req.execute(params![target_id, "option", f, Option::<i64>::None])?;
                }
            }
            for def in cg
                .get("defines")
                .and_then(|d| d.as_array())
                .into_iter()
                .flatten()
            {
                let Some(d) = def.get("define").and_then(|p| p.as_str()) else {
                    continue;
                };
                let origin = def
                    .get("backtrace")
                    .and_then(|b| b.as_u64())
                    .and_then(|i| bt.resolve(i as usize))
                    .and_then(|(f, l)| lookup_event(files, collected, &f, l));
                if seen.insert(("define", d.to_string(), origin)) {
                    ins_req.execute(params![target_id, "define", d, origin])?;
                }
            }
        }

        // Final link closure (resolved by CMake, genex-evaluated).
        for frag in t
            .get("link")
            .and_then(|l| l.get("commandFragments"))
            .and_then(|f| f.as_array())
            .into_iter()
            .flatten()
        {
            let role = frag.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role != "libraries" {
                continue;
            }
            let Some(fragment) = frag.get("fragment").and_then(|f| f.as_str()) else {
                continue;
            };
            let fragment = fragment.trim();
            if fragment.is_empty() {
                continue;
            }
            let origin = frag
                .get("backtrace")
                .and_then(|b| b.as_u64())
                .and_then(|i| bt.resolve(i as usize))
                .and_then(|(f, l)| lookup_event(files, collected, &f, l));
            if seen.insert(("link", fragment.to_string(), origin)) {
                ins_req.execute(params![target_id, "link", fragment, origin])?;
            }
        }

        // Translation units.
        for src in t
            .get("sources")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
        {
            if src
                .get("compileGroupIndex")
                .map(|i| !i.is_null())
                .unwrap_or(false)
            {
                if let Some(path) = src.get("path").and_then(|p| p.as_str()) {
                    ins_tu.execute(params![target_id, path])?;
                }
            }
        }
    }
    stats.targets =
        tx.query_row("SELECT count(*) FROM targets", [], |r| r.get::<_, i64>(0))? as u64;
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("parsing {}", path.display()))
}

/// backtraceGraph resolver: node index -> (file path, line) of the nearest
/// node in the chain that has a line (macro-expanded calls chain upward).
struct Backtraces {
    nodes: Vec<(Option<usize>, Option<i64>, Option<usize>)>, // (file idx, line, parent)
    files: Vec<String>,
}

impl Backtraces {
    fn new(target: &Value) -> Backtraces {
        let g = target.get("backtraceGraph");
        let files = g
            .and_then(|g| g.get("files"))
            .and_then(|f| f.as_array())
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default();
        let nodes = g
            .and_then(|g| g.get("nodes"))
            .and_then(|n| n.as_array())
            .map(|a| {
                a.iter()
                    .map(|v| {
                        (
                            v.get("file").and_then(|f| f.as_u64()).map(|f| f as usize),
                            v.get("line").and_then(|l| l.as_i64()),
                            v.get("parent").and_then(|p| p.as_u64()).map(|p| p as usize),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Backtraces { nodes, files }
    }

    fn resolve(&self, mut idx: usize) -> Option<(String, i64)> {
        let mut hops = 0;
        while hops < 64 {
            let (file, line, parent) = *self.nodes.get(idx)?;
            if let (Some(f), Some(l)) = (file, line) {
                return Some((self.files.get(f)?.clone(), l));
            }
            idx = parent?;
            hops += 1;
        }
        None
    }
}

fn lookup_event(
    files: &FileTable,
    collected: &CollectedGraph,
    file: &str,
    line: i64,
) -> Option<i64> {
    let fid = files.file_id(file)?;
    collected.event_loc.get(&(fid, line)).copied()
}
