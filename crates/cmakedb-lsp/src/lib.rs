//! Language server backed by the most recent recording (design §2.4).
//!
//! The wire layer ([`server`]) is deliberately thin; everything answerable
//! is implemented here as pure functions over the database so it can be
//! tested without an LSP client. Post-mortem by design: answers come from
//! the recording, and diagnostics are stale-marked when a file has changed
//! since it was recorded.

// Row tuples straight out of SQL queries are clearer inline than aliases.
#![allow(clippy::type_complexity)]

pub mod server;

use anyhow::Result;
use cmakedb_db::Db;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Locate `.cmakedb/trace.db` in `root` or any ancestor.
pub fn find_db(root: &Path) -> Option<PathBuf> {
    let mut cur = Some(root);
    while let Some(d) = cur {
        let p = d.join(".cmakedb/trace.db");
        if p.is_file() {
            return Some(p);
        }
        cur = d.parent();
    }
    None
}

/// The identifier-ish word under a (0-based line, character) position.
/// CMake "words" include target namespaces (`foo::bar`) and dots/dashes.
pub fn word_at(text: &str, line: usize, character: usize) -> Option<String> {
    let l = text.lines().nth(line)?;
    let is_word = |c: char| c.is_ascii_alphanumeric() || "_:.-".contains(c);
    let chars: Vec<char> = l.chars().collect();
    let mut i = character.min(chars.len().saturating_sub(1));
    if i < chars.len() && !is_word(chars[i]) && i > 0 {
        i -= 1;
    }
    if i >= chars.len() || !is_word(chars[i]) {
        return None;
    }
    let mut start = i;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = i;
    while end + 1 < chars.len() && is_word(chars[end + 1]) {
        end += 1;
    }
    let word: String = chars[start..=end].iter().collect();
    let word = word.trim_matches(|c| c == '.' || c == '-').to_string();
    if word.is_empty() {
        None
    } else {
        Some(word)
    }
}

#[derive(Debug, Clone)]
pub struct FileDiagnostic {
    /// 1-based line.
    pub line: i64,
    pub severity: cmakedb_passes::Severity,
    pub message: String,
    pub rule: String,
}

/// All lint findings grouped by absolute file path (§2.4 diagnostics).
pub fn diagnostics(db: &Db) -> Result<HashMap<String, Vec<FileDiagnostic>>> {
    let source_dir = db.get_meta("source_dir")?.unwrap_or_default();
    let mut out: HashMap<String, Vec<FileDiagnostic>> = HashMap::new();
    for pass in cmakedb_passes::builtin_passes() {
        let cfg = default_pass_config();
        for f in pass.run(db, &cfg)? {
            if f.primary.file.starts_with('<') {
                continue;
            }
            let abs = if Path::new(&f.primary.file).is_absolute() {
                f.primary.file.clone()
            } else {
                format!("{}/{}", source_dir.trim_end_matches('/'), f.primary.file)
            };
            out.entry(abs).or_default().push(FileDiagnostic {
                line: f.primary.line,
                severity: f.severity,
                message: f.message,
                rule: f.rule,
            });
        }
    }
    Ok(out)
}

fn default_pass_config() -> cmakedb_passes::PassConfig {
    cmakedb_passes::PassConfig {
        ignore: cmakedb_passes::default_ignore_patterns()
            .into_iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect(),
        options: serde_json::Value::Null,
    }
}

/// True when `path`'s current content differs from the recording.
pub fn is_stale(db: &Db, path: &str, current_text: &str) -> bool {
    let recorded: Option<Option<String>> = db
        .conn
        .query_row(
            "SELECT content_hash FROM files WHERE path = ?1",
            [path],
            |r| r.get(0),
        )
        .ok();
    match recorded {
        Some(Some(hash)) => cmakedb_syntax::hash_content(current_text) != hash,
        _ => false, // unknown file: nothing to be stale against
    }
}

/// Hover text for a word (§2.4): targets show their resolved closure
/// summary; variables show their write history for the enclosing scope.
pub fn hover(db: &Db, path: &str, line: i64, word: &str) -> Result<Option<String>> {
    // Target?
    if let Some(tid) = db.target_id(word)? {
        let (name, ttype): (String, Option<String>) =
            db.conn
                .query_row("SELECT name, type FROM targets WHERE id = ?1", [tid], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
        let mut s = format!("**target {name}** ({})\n", ttype.as_deref().unwrap_or("?"));
        let mut stmt = db.conn.prepare(
            "SELECT visibility, dst FROM tgt_edges WHERE src_target = ?1 ORDER BY id LIMIT 12",
        )?;
        let links: Vec<(String, String)> = stmt
            .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if !links.is_empty() {
            s.push_str("\nlinks:\n");
            for (v, d) in links {
                s.push_str(&format!("- {v} {d}\n"));
            }
        }
        let incs: i64 = db.conn.query_row(
            "SELECT count(DISTINCT value) FROM usage_reqs
             WHERE target_id = ?1 AND kind = 'include' AND source = 'fileapi'",
            [tid],
            |r| r.get(0),
        )?;
        s.push_str(&format!("\nresolved include dirs: {incs} (File API)"));
        return Ok(Some(s));
    }

    // Variable: precise resolution for a read on this exact line, then the
    // general write history.
    let mut s = String::new();
    let precise: Option<(String, Option<String>)> = db
        .conn
        .query_row(
            "SELECT w.write_kind, w.value FROM var_reads r
             JOIN events e ON e.id = r.event_id
             JOIN files f ON f.id = e.file_id
             JOIN var_writes w ON w.id = r.resolved_write_id
             WHERE r.name = ?1 AND f.path = ?2 AND e.line = ?3
             ORDER BY r.id LIMIT 1",
            rusqlite::params![word, path, line],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    if let Some((kind, value)) = precise {
        s.push_str(&format!(
            "**{word}** here = `{}` ({kind})\n\n",
            value.as_deref().unwrap_or("<not captured>")
        ));
    }
    let records = cmakedb_passes::history::why_value(db, word, None)?;
    if records.is_empty() && s.is_empty() {
        return Ok(None);
    }
    if !records.is_empty() {
        s.push_str(&format!("write history ({} write(s)):\n", records.len()));
        for r in records.iter().rev().take(5).rev() {
            s.push_str(&format!(
                "- `{}` at {} ({}/{})\n",
                r.value.as_deref().unwrap_or("<not captured>"),
                r.location,
                r.scope_kind,
                r.write_kind
            ));
        }
    }
    Ok(Some(s))
}

/// Go-to-definition (§2.4): dominating write for a read at the position;
/// function/macro definitions; target definition sites. Returns an
/// absolute `(path, 1-based line)`.
pub fn definition(db: &Db, path: &str, line: i64, word: &str) -> Result<Option<(String, i64)>> {
    // Read on this line -> its dominating write.
    let hit: Option<(String, i64)> = db
        .conn
        .query_row(
            "SELECT wf.path, we.line FROM var_reads r
             JOIN events e ON e.id = r.event_id
             JOIN files f ON f.id = e.file_id
             JOIN var_writes w ON w.id = r.resolved_write_id
             JOIN events we ON we.id = w.event_id
             JOIN files wf ON wf.id = we.file_id
             WHERE r.name = ?1 AND f.path = ?2 AND e.line = ?3
             ORDER BY r.id LIMIT 1",
            rusqlite::params![word, path, line],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    if hit.is_some() {
        return Ok(hit);
    }
    // Function/macro definition.
    let hit: Option<(String, i64)> = db
        .conn
        .query_row(
            "SELECT f.path, d.line FROM func_defs d JOIN files f ON f.id = d.file_id
             WHERE d.name_lower = lower(?1) ORDER BY d.id DESC LIMIT 1",
            [word],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    if hit.is_some() {
        return Ok(hit);
    }
    // Target definition site.
    if let Some(tid) = db.target_id(word)? {
        let hit: Option<(String, i64)> = db
            .conn
            .query_row(
                "SELECT f.path, e.line FROM targets t
                 JOIN events e ON e.id = t.defined_event
                 JOIN files f ON f.id = e.file_id WHERE t.id = ?1",
                [tid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if hit.is_some() {
            return Ok(hit);
        }
    }
    // Fall back to the variable's last write anywhere.
    let hit: Option<(String, i64)> = db
        .conn
        .query_row(
            "SELECT f.path, e.line FROM var_writes w
             JOIN events e ON e.id = w.event_id
             JOIN files f ON f.id = e.file_id
             WHERE w.name = ?1 ORDER BY w.id DESC LIMIT 1",
            [word],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    Ok(hit)
}

/// Resolve an editor path to the recording's spelling of it (symlinks:
/// /var vs /private/var on macOS).
pub fn db_path_for(db: &Db, editor_path: &Path) -> String {
    let exact = cmakedb_db::cmake_path_spelling(&editor_path.to_string_lossy());
    let known: i64 = db
        .conn
        .query_row(
            "SELECT count(*) FROM files WHERE path = ?1",
            [&exact],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if known > 0 {
        return exact;
    }
    if let Ok(canon) = editor_path.canonicalize() {
        let c = canon.to_string_lossy().to_string();
        // Compare against canonicalized recorded paths.
        let mut stmt = match db.conn.prepare("SELECT path FROM files") {
            Ok(s) => s,
            Err(_) => return exact,
        };
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .and_then(|r| r.collect())
            .unwrap_or_default();
        for p in rows {
            if Path::new(&p)
                .canonicalize()
                .map(|x| x == canon)
                .unwrap_or(false)
                || p == c
            {
                return p;
            }
        }
    }
    exact
}

// --- Provenance tree data (§2.4 custom request / editor tree view) --------

/// Targets in the recording, for the tree-view roots.
pub fn targets_list(db: &Db) -> Result<Vec<(String, String)>> {
    let mut stmt = db.conn.prepare(
        "SELECT name, coalesce(type, '?') FROM targets
         WHERE alias_of IS NULL AND in_file_api = 1 ORDER BY name",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Direct link edges of a target: (dst display name, visibility, origin
/// location, dst-is-target). Expanding these recursively renders the
/// provenance tree.
pub fn edges_of(db: &Db, target: &str) -> Result<Vec<(String, String, Option<String>, bool)>> {
    let Some(tid) = db.target_id(target)? else {
        return Ok(vec![]);
    };
    let mut stmt = db.conn.prepare(
        "SELECT coalesce(t.name, e.dst), e.visibility, e.origin_event,
                e.dst_target IS NOT NULL
         FROM tgt_edges e LEFT JOIN targets t ON t.id = e.dst_target
         WHERE e.src_target = ?1 ORDER BY e.id",
    )?;
    let rows: Vec<(String, String, Option<i64>, bool)> = stmt
        .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .map(|(dst, vis, origin, is_target)| {
            (
                dst,
                vis,
                origin.and_then(|e| db.event_location(e).ok()),
                is_target,
            )
        })
        .collect())
}

// --- Code actions: apply modernize patches (§2.4) --------------------------

/// A ready-to-apply text edit against the *current* buffer; only produced
/// when the buffer matches the recording (patches are byte-anchored).
#[derive(Debug, Clone)]
pub struct FixAction {
    pub title: String,
    /// (start_line, start_char_utf16, end_line, end_char_utf16, new_text),
    /// all 0-based, per file path.
    pub edits: Vec<(String, u32, u32, u32, u32, String)>,
}

/// Modernize fixes whose primary finding sits on `line` (1-based) of
/// `path`, converted to LSP-shaped ranges. Buffers that diverged from the
/// recording get none (stale offsets must never be applied).
pub fn code_actions(
    db: &Db,
    path: &str,
    line: i64,
    current_texts: &HashMap<String, String>,
) -> Result<Vec<FixAction>> {
    let path = cmakedb_db::cmake_path_spelling(path);
    let current_texts: HashMap<String, &String> = current_texts
        .iter()
        .map(|(path, text)| (cmakedb_db::cmake_path_spelling(path), text))
        .collect();
    let source_dir = db.get_meta("source_dir")?.unwrap_or_default();
    let abs = |f: &str| {
        if Path::new(f).is_absolute() {
            f.to_string()
        } else {
            format!("{}/{}", source_dir.trim_end_matches('/'), f)
        }
    };
    let mut out = Vec::new();
    for finding in cmakedb_passes::modernize::plan(db, &[])? {
        let Some(fix) = &finding.fix else { continue };
        if !cmakedb_db::cmake_path_eq(&abs(&finding.primary.file), &path)
            || finding.primary.line != line
        {
            continue;
        }
        let mut edits = Vec::new();
        let mut ok = true;
        for e in &fix.edits {
            let recorded: Option<String> = db
                .conn
                .query_row(
                    "SELECT content FROM files WHERE path = ?1",
                    [&e.file],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let Some(recorded) = recorded else {
                ok = false;
                break;
            };
            // Refuse when the open buffer (if any) diverged from the
            // recording — byte offsets would land in the wrong place.
            if let Some(current) = current_texts.get(&e.file) {
                if cmakedb_syntax::hash_content(current) != cmakedb_syntax::hash_content(&recorded)
                {
                    ok = false;
                    break;
                }
            }
            let (sl, sc) = byte_to_position(&recorded, e.byte_start);
            let (el, ec) = byte_to_position(&recorded, e.byte_end);
            edits.push((e.file.clone(), sl, sc, el, ec, e.replacement.clone()));
        }
        if ok && !edits.is_empty() {
            out.push(FixAction {
                title: format!("cmakedb: {}", fix.title),
                edits,
            });
        }
    }
    Ok(out)
}

/// Byte offset -> 0-based (line, UTF-16 character), per the LSP spec.
pub fn byte_to_position(text: &str, byte: usize) -> (u32, u32) {
    let byte = byte.min(text.len());
    let prefix = &text[..byte];
    let line = prefix.matches('\n').count() as u32;
    let line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let character = text[line_start..byte].encode_utf16().count() as u32;
    (line, character)
}
