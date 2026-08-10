//! Codemod verification loop (design §4.6 step 6): apply patches, re-record
//! with the same source/build dirs (the build dir's cache preserves the
//! original configuration), and assert build-graph isomorphism. A patch set
//! that changes the graph is auto-reverted and reported. This is the
//! property that makes `--fix` trustworthy.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cmakedb_db::diff::graph_isomorphism_diff;
use cmakedb_db::Db;
use cmakedb_patch::Patch;

use crate::{record, RecordOptions};

#[derive(Debug)]
pub struct FixOutcome {
    pub applied_files: Vec<PathBuf>,
    /// Empty when the graph was preserved; the differences otherwise.
    pub problems: Vec<String>,
    /// True when problems were found and the source edits were rolled back.
    pub reverted: bool,
}

impl FixOutcome {
    pub fn verified(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Apply `patches` to the working tree recorded in `db_path`, re-record,
/// and verify. On success the recording at `db_path` is replaced by the
/// fresh post-fix recording; on mismatch all edits are rolled back.
pub fn apply_and_verify(db_path: &Path, patches: &[Patch]) -> Result<FixOutcome> {
    let (source_dir, build_dir) = {
        let db = Db::open(db_path)?;
        let src = db
            .get_meta("source_dir")?
            .context("recording has no source_dir metadata")?;
        let bld = db
            .get_meta("build_dir")?
            .context("recording has no build_dir metadata")?;
        (PathBuf::from(src), PathBuf::from(bld))
    };

    // Merge all edits into one patch per run so cross-patch overlaps are
    // rejected rather than silently misapplied.
    let merged = Patch {
        title: "modernize".into(),
        edits: patches
            .iter()
            .flat_map(|p| p.edits.iter().cloned())
            .collect(),
    };
    if merged.edits.is_empty() {
        bail!("no applicable fixes");
    }
    let mut files: Vec<String> = merged.edits.iter().map(|e| e.file.clone()).collect();
    files.sort();
    files.dedup();

    // Apply, keeping originals for rollback. The content-hash check makes
    // stale patches (file changed since the recording) a hard error before
    // anything is written.
    let mut originals: HashMap<String, String> = HashMap::new();
    let mut new_contents: HashMap<String, String> = HashMap::new();
    for file in &files {
        let current = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
        let current_hash = cmakedb_syntax::hash_content(&current);
        let updated = merged
            .apply_to(file, &current, &current_hash)
            .with_context(|| format!("applying patch to {file}"))?;
        originals.insert(file.clone(), current);
        new_contents.insert(file.clone(), updated);
    }
    for (file, content) in &new_contents {
        std::fs::write(file, content).with_context(|| format!("writing {file}"))?;
    }
    let rollback = |originals: &HashMap<String, String>| -> Result<()> {
        for (file, content) in originals {
            std::fs::write(file, content).with_context(|| format!("rolling back {file}"))?;
        }
        Ok(())
    };

    // Re-record into a sibling temp db; recording is cheap by design.
    let verify_db = db_path.with_extension("verify.db");
    let opts = RecordOptions {
        source_dir: Some(source_dir),
        build_dir: Some(build_dir),
        db_path: Some(verify_db.clone()),
        ..Default::default()
    };
    let rerecord = record(&opts);
    let problems = match &rerecord {
        Ok(_) => {
            let old = Db::open(db_path)?;
            let new = Db::open(&verify_db)?;
            graph_isomorphism_diff(&old, &new)?
        }
        Err(e) => vec![format!("re-configure failed after applying fixes: {e:#}")],
    };

    if problems.is_empty() {
        // The fresh recording is the accurate post-fix state; promote it.
        std::fs::rename(&verify_db, db_path).with_context(|| {
            format!(
                "promoting {} over {}",
                verify_db.display(),
                db_path.display()
            )
        })?;
        Ok(FixOutcome {
            applied_files: files.iter().map(PathBuf::from).collect(),
            problems,
            reverted: false,
        })
    } else {
        rollback(&originals)?;
        let _ = std::fs::remove_file(&verify_db);
        Ok(FixOutcome {
            applied_files: vec![],
            problems,
            reverted: true,
        })
    }
}
