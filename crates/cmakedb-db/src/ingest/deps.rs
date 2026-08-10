//! Compiler dependency ingestion (`ninja -t deps` output), design §3.2
//! channel 4. Populates `tu_headers` so `overshare` can check which headers
//! consumer TUs actually included.

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::Db;

/// Parse `ninja -t deps` text output and populate tu_headers.
///
/// Format:
/// ```text
/// CMakeFiles/app.dir/src/main.c.o: #deps 2, deps mtime 123 (VALID)
///     ../src/main.c
///     ../include/util.h
///
/// ```
/// Object paths are matched to `tus` rows via the `CMakeFiles/<target>.dir/`
/// convention plus source-suffix matching. `build_dir` anchors relative
/// header paths.
pub fn ingest_ninja_deps(db: &mut Db, deps_output: &str, build_dir: &Path) -> Result<u64> {
    // Map (target name, normalized source suffix) -> tu id.
    let mut tus: Vec<(i64, String, String)> = Vec::new(); // (tu_id, target, source_path)
    {
        let mut stmt = db.conn.prepare(
            "SELECT tus.id, targets.name, tus.source_path
             FROM tus JOIN targets ON targets.id = tus.target_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for r in rows {
            tus.push(r?);
        }
    }

    let tx = db.conn.transaction()?;
    let mut ins = tx.prepare("INSERT INTO tu_headers(tu_id, header_path) VALUES (?1, ?2)")?;
    let mut inserted = 0u64;
    let mut current_tu: Option<i64> = None;
    let mut header_cache: HashMap<String, String> = HashMap::new();

    for line in deps_output.lines() {
        if line.is_empty() {
            current_tu = None;
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            // "path/to/obj.o: #deps N, ..."
            let obj = line.split(':').next().unwrap_or("").trim();
            current_tu = match_tu(obj, &tus);
            continue;
        }
        let Some(tu) = current_tu else { continue };
        let dep = line.trim();
        if dep.is_empty() {
            continue;
        }
        let normalized = header_cache
            .entry(dep.to_string())
            .or_insert_with(|| normalize_path(dep, build_dir))
            .clone();
        ins.execute(rusqlite::params![tu, normalized])?;
        inserted += 1;
    }
    drop(ins);
    tx.commit()?;
    Ok(inserted)
}

fn match_tu(obj_path: &str, tus: &[(i64, String, String)]) -> Option<i64> {
    // CMakeFiles/<target>.dir/<relative-source>.o
    let marker = "CMakeFiles/";
    let idx = obj_path.find(marker)?;
    let rest = &obj_path[idx + marker.len()..];
    let (target_dir, src_part) = rest.split_once(".dir/")?;
    let src = src_part
        .strip_suffix(".o")
        .or_else(|| src_part.strip_suffix(".obj"))?;
    tus.iter()
        .find(|(_, tname, spath)| tname == target_dir && spath.ends_with(src))
        .map(|(id, _, _)| *id)
}

/// Normalize a (possibly build-relative) dep path to an absolute path,
/// resolving `.`/`..` lexically. Deliberately does NOT resolve symlinks:
/// the rest of the database uses cmake's own path spelling, and mixing
/// realpaths with as-spelled paths is itself a §6.5 false-positive source.
fn normalize_path(path: &str, build_dir: &Path) -> String {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        build_dir.join(p)
    };
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    let mut out = PathBuf::new();
    for p in parts {
        out.push(p);
    }
    crate::cmake_path_spelling(&out.to_string_lossy())
}
