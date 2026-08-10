//! Streaming parser for CMake's `--trace-format=json-v1` JSONL output.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct TraceEvent {
    pub file: String,
    pub line: i64,
    /// Present for multi-line invocations; kept for schema completeness.
    #[serde(default)]
    #[allow(dead_code)]
    pub line_end: Option<i64>,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub time: f64,
    #[serde(default)]
    pub frame: i64,
    #[serde(default)]
    pub global_frame: i64,
    /// Set for `cmake_language(DEFER)`-executed commands.
    #[serde(default)]
    #[allow(dead_code)]
    pub defer: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct VersionLine {
    version: VersionInfo,
}

#[derive(Debug, Deserialize)]
struct VersionInfo {
    major: i64,
    #[allow(dead_code)]
    minor: i64,
}

/// Pass 1: the set of distinct file paths appearing in the trace.
pub fn collect_files(trace_path: &Path) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for_each_event(trace_path, |e| {
        if !out.contains(&e.file) {
            out.insert(e.file.clone());
        }
        Ok(())
    })?;
    Ok(out)
}

/// Stream all events in order. The first line must be a json-v1 version
/// header; malformed/truncated trailing lines fail cleanly (§6.5: no
/// partial-commit — the caller runs inside one transaction).
pub fn for_each_event(trace_path: &Path, f: impl FnMut(&TraceEvent) -> Result<()>) -> Result<()> {
    let file = std::fs::File::open(trace_path)
        .with_context(|| format!("opening trace {}", trace_path.display()))?;
    for_each_event_reader(std::io::BufReader::new(file), f)
}

/// Reader-based variant; also the fuzzing entry point (§6.5).
pub fn for_each_event_reader(
    reader: impl BufRead,
    mut f: impl FnMut(&TraceEvent) -> Result<()>,
) -> Result<()> {
    let mut lines = reader.lines();

    let header = match lines.next() {
        Some(l) => l?,
        None => bail!("empty trace file"),
    };
    let v: VersionLine = serde_json::from_str(&header)
        .context("trace file does not start with a json-v1 version header")?;
    if v.version.major != 1 {
        bail!("unsupported trace format version {}", v.version.major);
    }

    for (i, line) in lines.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let ev: TraceEvent = serde_json::from_str(&line)
            .with_context(|| format!("malformed trace event at line {}", i + 2))?;
        f(&ev)?;
    }
    Ok(())
}

/// Validate a trace held in memory: parse every event, discarding them.
/// Used by the fuzz harness — must never panic, only return Err.
pub fn scan_trace_bytes(bytes: &[u8]) -> Result<u64> {
    let mut n = 0u64;
    for_each_event_reader(std::io::BufReader::new(bytes), |_| {
        n += 1;
        Ok(())
    })?;
    Ok(n)
}
