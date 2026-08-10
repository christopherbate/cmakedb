//! AST-anchored patches (design §4.6).
//!
//! v1 scope: the `Patch` data model that `Finding.fix` carries, byte-anchored
//! application, and unified-diff rendering. The full modernize codemod
//! pipeline and the re-record verification loop are milestone M4 (§5.3) and
//! are not implemented yet.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// One edit anchored to a byte range of a file whose content hash was
/// recorded at analysis time. Application refuses to touch a file that has
/// changed since (position-stable node IDs, design §3.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edit {
    pub file: String,
    /// Content hash the byte offsets are valid against.
    pub content_hash: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub replacement: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Patch {
    pub title: String,
    pub edits: Vec<Edit>,
}

impl Patch {
    /// Apply to in-memory content; `current_hash` must match every edit's
    /// recorded hash. Edits must be non-overlapping.
    pub fn apply_to(&self, file: &str, content: &str, current_hash: &str) -> Result<String> {
        let mut edits: Vec<&Edit> = self.edits.iter().filter(|e| e.file == file).collect();
        edits.sort_by_key(|e| e.byte_start);
        for w in edits.windows(2) {
            if w[0].byte_end > w[1].byte_start {
                bail!("overlapping edits in patch '{}'", self.title);
            }
        }
        let mut out = String::with_capacity(content.len());
        let mut cursor = 0;
        for e in edits {
            if e.content_hash != current_hash {
                bail!(
                    "file {} changed since the recording; refusing to apply stale patch",
                    file
                );
            }
            if e.byte_end > content.len() {
                bail!("edit range out of bounds for {}", file);
            }
            out.push_str(&content[cursor..e.byte_start]);
            out.push_str(&e.replacement);
            cursor = e.byte_end;
        }
        out.push_str(&content[cursor..]);
        Ok(out)
    }

    /// Render a unified diff for all edits against provided file contents.
    pub fn unified_diff(&self, load: impl Fn(&str) -> Option<String>) -> String {
        let mut out = String::new();
        let mut files: Vec<&str> = self.edits.iter().map(|e| e.file.as_str()).collect();
        files.sort();
        files.dedup();
        for file in files {
            let Some(old) = load(file) else { continue };
            let hash = self
                .edits
                .iter()
                .find(|e| e.file == file)
                .map(|e| e.content_hash.clone())
                .unwrap_or_default();
            let Ok(new) = self.apply_to(file, &old, &hash) else {
                continue;
            };
            out.push_str(&simple_diff(file, &old, &new));
        }
        out
    }
}

/// Minimal line-based unified diff (single hunk per contiguous change run).
fn simple_diff(file: &str, old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    // Trim common prefix/suffix.
    let mut start = 0;
    while start < old_lines.len() && start < new_lines.len() && old_lines[start] == new_lines[start]
    {
        start += 1;
    }
    let mut old_end = old_lines.len();
    let mut new_end = new_lines.len();
    while old_end > start && new_end > start && old_lines[old_end - 1] == new_lines[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }
    if start == old_end && start == new_end {
        return String::new();
    }
    let mut out = format!("--- a/{file}\n+++ b/{file}\n");
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        start + 1,
        old_end - start,
        start + 1,
        new_end - start
    ));
    for l in &old_lines[start..old_end] {
        out.push_str(&format!("-{l}\n"));
    }
    for l in &new_lines[start..new_end] {
        out.push_str(&format!("+{l}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_and_refuse_stale() {
        let content = "set(A 1)\nset(B 2)\n";
        let p = Patch {
            title: "t".into(),
            edits: vec![Edit {
                file: "f.cmake".into(),
                content_hash: "h1".into(),
                byte_start: 4,
                byte_end: 5,
                replacement: "X".into(),
            }],
        };
        assert_eq!(
            p.apply_to("f.cmake", content, "h1").unwrap(),
            "set(X 1)\nset(B 2)\n"
        );
        assert!(p.apply_to("f.cmake", content, "other").is_err());
    }

    #[test]
    fn diff_renders() {
        let p = Patch {
            title: "t".into(),
            edits: vec![Edit {
                file: "f".into(),
                content_hash: String::new(),
                byte_start: 0,
                byte_end: 3,
                replacement: "put".into(),
            }],
        };
        let d = p.unified_diff(|_| Some("get(A)\n".into()));
        assert!(d.contains("-get(A)"), "{d}");
        assert!(d.contains("+put(A)"), "{d}");
    }
}
