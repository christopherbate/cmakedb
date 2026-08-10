//! L2 execution recorder (design §3.2).
//!
//! Runs the *real* CMake under `--trace-expand --trace-format=json-v1`,
//! writes a File API query beforehand, and hands both artifacts to the L3
//! ingester. Never reimplements CMake.

pub mod verify;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cmakedb_db::ingest::{ingest, IngestInput, IngestStats};
use cmakedb_db::Db;

#[derive(Debug, Clone, Default)]
pub struct RecordOptions {
    pub preset: Option<String>,
    /// Raw cmake arguments (everything after `--`).
    pub raw_args: Vec<String>,
    pub source_dir: Option<PathBuf>,
    pub build_dir: Option<PathBuf>,
    /// Database output path; default `<source>/.cmakedb/trace.db`.
    pub db_path: Option<PathBuf>,
    pub cmake_exe: Option<String>,
    /// Scope snapshots (design §3.2 channel 2): inject a
    /// CMAKE_PROJECT_INCLUDE_BEFORE hook that dumps directory-scope
    /// variable tables; ingestion cross-validates the replay against them
    /// and fails loudly on divergence (§6.2). ~10-20% configure overhead.
    pub capture_scopes: bool,
}

pub struct RecordResult {
    pub db_path: PathBuf,
    pub stats: IngestStats,
}

pub fn record(opts: &RecordOptions) -> Result<RecordResult> {
    let cmake = opts.cmake_exe.clone().unwrap_or_else(|| "cmake".into());

    // Everything is absolutized up front: cmake runs with cwd = source_dir,
    // and the trace-redirect path must resolve from anywhere.
    let source_dir = std::path::absolute(resolve_source_dir(opts)?)?;
    let build_dir = std::path::absolute(resolve_build_dir(opts, &source_dir)?)?;
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("creating build dir {}", build_dir.display()))?;

    let db_path = std::path::absolute(
        opts.db_path
            .clone()
            .unwrap_or_else(|| source_dir.join(".cmakedb/trace.db")),
    )?;
    let out_dir = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| source_dir.join(".cmakedb"));
    std::fs::create_dir_all(&out_dir)?;
    let trace_path = out_dir.join("trace.jsonl");

    // File API query (design §3.2 channel 3): codemodel + cache + toolchains.
    let query_dir = build_dir.join(".cmake/api/v1/query/client-cmakedb");
    std::fs::create_dir_all(&query_dir)?;
    std::fs::write(
        query_dir.join("query.json"),
        serde_json::to_string_pretty(&json!({
            "requests": [
                {"kind": "codemodel", "version": 2},
                {"kind": "cache", "version": 2},
                {"kind": "toolchains", "version": 1}
            ]
        }))?,
    )?;

    // Run the real cmake with tracing.
    // cmake runs from the caller's cwd so that user-supplied relative raw
    // args resolve as typed; preset mode passes -S so the presets file is
    // found regardless of cwd.
    let mut cmd = Command::new(&cmake);
    if let Some(preset) = &opts.preset {
        cmd.arg("-S").arg(&source_dir).arg("--preset").arg(preset);
        // An explicit build dir overrides the preset's `binaryDir` on the
        // cmake command line too — required for presets that declare no
        // binaryDir (schema v3+), and without it cmake would configure a
        // different directory than the one we read the File API from.
        if opts.build_dir.is_some() {
            cmd.arg("-B").arg(&build_dir);
        }
    } else if !opts.raw_args.is_empty() {
        cmd.args(&opts.raw_args);
    } else {
        cmd.arg("-S").arg(&source_dir).arg("-B").arg(&build_dir);
    }
    cmd.arg("--trace-expand")
        .arg("--trace-format=json-v1")
        .arg(format!("--trace-redirect={}", trace_path.display()));

    // Scope snapshots (§3.2 channel 2): a hook macro dumps the visible
    // variable table at each project() call and at top-directory end
    // (cmake_language DEFER). Values are hex-encoded so arbitrary content
    // survives CMake string handling.
    let snapshots_path = out_dir.join("scopes.tsv");
    if opts.capture_scopes {
        let _ = std::fs::remove_file(&snapshots_path);
        let hook_path = out_dir.join("scope_hook.cmake");
        std::fs::write(&hook_path, scope_hook_script(&snapshots_path))?;
        cmd.arg(format!(
            "-DCMAKE_PROJECT_INCLUDE_BEFORE={}",
            hook_path.display()
        ));
    }

    eprintln!("cmakedb: running {:?}", cmd.get_args().collect::<Vec<_>>());
    // Capture stderr (CMake's own warning/error blocks feed the
    // configure-warnings channel) while forwarding it live so the user
    // still sees the configure as it happens. stdout stays inherited, so
    // reading the single stderr pipe to EOF on this thread cannot
    // deadlock. Lines pass through byte-preserving except for lossy UTF-8
    // conversion of the capture.
    cmd.stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to run `{cmake}` — is it on PATH?"))?;
    let mut captured_stderr = Vec::new();
    if let Some(pipe) = child.stderr.take() {
        let mut reader = BufReader::new(pipe);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let _ = std::io::stderr().write_all(&line);
                    captured_stderr.extend_from_slice(&line);
                }
            }
        }
    }
    let status = child
        .wait()
        .with_context(|| format!("waiting for `{cmake}`"))?;
    // Written even on failure: the diagnostics explain *why* it failed.
    let stderr_path = out_dir.join("configure-stderr.txt");
    std::fs::write(&stderr_path, &captured_stderr)
        .with_context(|| format!("writing {}", stderr_path.display()))?;
    if !status.success() {
        bail!("cmake configure failed (exit {status}); no database written");
    }

    // Ingest.
    let cmake_version = cmake_version(&cmake).unwrap_or_default();
    let reply_dir = build_dir.join(".cmake/api/v1/reply");
    // Remove a stale db so a failed re-record can't masquerade as fresh.
    let _ = std::fs::remove_file(&db_path);
    let mut db = Db::create(&db_path)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    // Keep cmake's own spelling of the dirs (no symlink resolution): trace
    // file paths and CMAKE_*_DIR values use the paths as passed, and the
    // database must join on that spelling.
    let input = IngestInput {
        trace_path: trace_path.clone(),
        source_dir: source_dir.clone(),
        build_dir: build_dir.clone(),
        reply_dir: Some(reply_dir),
        snapshots_path: opts.capture_scopes.then(|| snapshots_path.clone()),
        preseeded_cache: preseeded_cache_entries(opts, &source_dir),
        stderr_path: Some(stderr_path.clone()),
        meta: vec![
            ("cmake_version".into(), cmake_version),
            ("preset".into(), opts.preset.clone().unwrap_or_default()),
            ("argv".into(), opts.raw_args.join(" ")),
            ("recorded_at_epoch".into(), now),
            ("platform".into(), std::env::consts::OS.into()),
        ],
    };
    // §6.5: ingestion failures must not leave a half-usable database — the
    // transaction already rolls back, and removing the file makes the
    // failure unambiguous to later `open_db` calls.
    let stats = match ingest(&mut db, &input) {
        Ok(stats) => stats,
        Err(e) => {
            drop(db);
            let _ = std::fs::remove_file(&db_path);
            return Err(e.context("ingesting recording (no database written)"));
        }
    };
    Ok(RecordResult { db_path, stats })
}

/// The injected snapshot hook (§3.2). A macro (not a function!) so the
/// dump sees the *caller's* variable scope. Underscore-prefixed temps are
/// filtered from the dump and unset afterwards. Each block starts with a
/// SNAPSHOT header line; ingestion pairs blocks with the hook's
/// `file(APPEND)` trace events in order.
fn scope_hook_script(snapshots_path: &Path) -> String {
    format!(
        r#"# generated by cmakedb record --capture-scopes
if(NOT COMMAND _cmakedb_snapshot)
  macro(_cmakedb_snapshot _cdb_tag)
    get_cmake_property(_cdb_vars VARIABLES)
    set(_cdb_out "SNAPSHOT\t${{_cdb_tag}}\n")
    foreach(_cdb_v IN LISTS _cdb_vars)
      if(NOT _cdb_v MATCHES "^_cdb_")
        string(HEX "${{${{_cdb_v}}}}" _cdb_hex)
        string(APPEND _cdb_out "${{_cdb_v}}\t${{_cdb_hex}}\n")
      endif()
    endforeach()
    file(APPEND "{snap}" "${{_cdb_out}}")
    unset(_cdb_out)
    unset(_cdb_vars)
    unset(_cdb_hex)
  endmacro()
endif()
_cmakedb_snapshot(project-before)
cmake_language(DEFER CALL _cmakedb_snapshot dir-end)
"#,
        snap = snapshots_path.display()
    )
}

/// Cache entries the user pre-seeds: `-DNAME[:TYPE]=VALUE` raw args plus
/// the chosen preset's `cacheVariables` (walking `inherits`, nearest
/// definition winning). These exist before the first trace event.
fn preseeded_cache_entries(opts: &RecordOptions, source_dir: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |name: &str, value: String| {
        if !name.is_empty() && !out.iter().any(|(n, _)| n == name) {
            out.push((name.to_string(), value));
        }
    };
    // -DNAME=V, -DNAME:TYPE=V, and the split form `-D NAME=V`.
    let mut expect_def = false;
    for a in &opts.raw_args {
        let def = if expect_def {
            expect_def = false;
            Some(a.as_str())
        } else if a == "-D" {
            expect_def = true;
            None
        } else {
            a.strip_prefix("-D")
        };
        if let Some(def) = def {
            if let Some((name_ty, value)) = def.split_once('=') {
                let name = name_ty.split(':').next().unwrap_or(name_ty);
                push(name, value.to_string());
            }
        }
    }
    // Preset cacheVariables across the inherits chain.
    if let Some(preset) = &opts.preset {
        let presets = load_preset_entries(source_dir).unwrap_or_default();
        let find = |n: &str| {
            presets
                .iter()
                .find(|p| p.get("name").and_then(|x| x.as_str()) == Some(n))
        };
        let mut queue = vec![preset.clone()];
        let mut seen = std::collections::HashSet::new();
        while let Some(name) = queue.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(p) = find(&name) else { continue };
            for (k, v) in p
                .get("cacheVariables")
                .and_then(|c| c.as_object())
                .into_iter()
                .flatten()
            {
                let value = match v {
                    Value::String(s) => s.clone(),
                    Value::Bool(b) => if *b { "ON" } else { "OFF" }.to_string(),
                    Value::Object(o) => o
                        .get("value")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    other => other.to_string(),
                };
                push(k, value);
            }
            match p.get("inherits") {
                Some(Value::String(s)) => queue.push(s.clone()),
                Some(Value::Array(a)) => {
                    queue.extend(a.iter().filter_map(|v| v.as_str().map(String::from)))
                }
                _ => {}
            }
        }
    }
    out
}

fn resolve_source_dir(opts: &RecordOptions) -> Result<PathBuf> {
    if let Some(s) = &opts.source_dir {
        return Ok(s.clone());
    }
    if let Some(i) = opts.raw_args.iter().position(|a| a == "-S") {
        if let Some(s) = opts.raw_args.get(i + 1) {
            return Ok(PathBuf::from(s));
        }
    }
    for a in &opts.raw_args {
        if let Some(s) = a.strip_prefix("-S") {
            if !s.is_empty() {
                return Ok(PathBuf::from(s));
            }
        }
    }
    Ok(std::env::current_dir()?)
}

fn resolve_build_dir(opts: &RecordOptions, source_dir: &Path) -> Result<PathBuf> {
    if let Some(b) = &opts.build_dir {
        return Ok(b.clone());
    }
    // Raw -B is relative to the caller's cwd (cmake runs from there too).
    if let Some(i) = opts.raw_args.iter().position(|a| a == "-B") {
        if let Some(b) = opts.raw_args.get(i + 1) {
            return Ok(std::env::current_dir()?.join(b));
        }
    }
    for a in &opts.raw_args {
        if let Some(b) = a.strip_prefix("-B") {
            if !b.is_empty() {
                return Ok(std::env::current_dir()?.join(b));
            }
        }
    }
    if let Some(preset) = &opts.preset {
        return preset_binary_dir(source_dir, preset);
    }
    Ok(source_dir.join("build"))
}

/// Every `configurePresets` entry from CMakeUserPresets.json and
/// CMakePresets.json (user presets first, so a `find`-by-name over the
/// combined list lets user presets shadow project presets). Missing files
/// are fine; malformed JSON is an error.
fn load_preset_entries(source_dir: &Path) -> Result<Vec<Value>> {
    let mut presets: Vec<Value> = Vec::new();
    for name in ["CMakeUserPresets.json", "CMakePresets.json"] {
        let p = source_dir.join(name);
        if let Ok(content) = std::fs::read_to_string(&p) {
            let v: Value = serde_json::from_str(&content)
                .with_context(|| format!("parsing {}", p.display()))?;
            if let Some(arr) = v.get("configurePresets").and_then(|c| c.as_array()) {
                presets.extend(arr.iter().cloned());
            }
        }
    }
    Ok(presets)
}

/// Names of every non-hidden configure preset (no `"hidden": true`) from
/// CMakePresets.json / CMakeUserPresets.json, in file order (user presets
/// first), deduplicated by name. Errors if neither file defines any
/// `configurePresets`. This is the discovery step of `cmakedb matrix`.
pub fn list_configure_presets(source_dir: &Path) -> Result<Vec<String>> {
    let presets = load_preset_entries(source_dir)?;
    if presets.is_empty() {
        bail!(
            "no CMakePresets.json with configurePresets found in {}",
            source_dir.display()
        );
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in &presets {
        let Some(name) = p.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if p.get("hidden").and_then(|h| h.as_bool()) == Some(true) {
            continue;
        }
        if seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

/// Resolve `binaryDir` for a configure preset, following `inherits` across
/// CMakePresets.json and CMakeUserPresets.json, expanding the common macros.
/// Errors when the preset (or its inherits chain) declares no binaryDir.
pub fn preset_binary_dir(source_dir: &Path, preset: &str) -> Result<PathBuf> {
    let presets = load_preset_entries(source_dir)?;
    if presets.is_empty() {
        bail!(
            "no CMakePresets.json with configurePresets found in {}",
            source_dir.display()
        );
    }
    let find = |name: &str| {
        presets
            .iter()
            .find(|p| p.get("name").and_then(|n| n.as_str()) == Some(name))
    };

    // Walk the inherits chain for binaryDir.
    let mut queue = vec![preset.to_string()];
    let mut seen = std::collections::HashSet::new();
    while let Some(name) = queue.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(p) = find(&name) else {
            if name == preset {
                bail!("preset '{preset}' not found");
            }
            continue;
        };
        if let Some(bd) = p.get("binaryDir").and_then(|b| b.as_str()) {
            let expanded = expand_preset_macros(bd, source_dir, preset);
            let path = PathBuf::from(expanded);
            return Ok(if path.is_absolute() {
                path
            } else {
                source_dir.join(path)
            });
        }
        match p.get("inherits") {
            Some(Value::String(s)) => queue.push(s.clone()),
            Some(Value::Array(a)) => {
                queue.extend(a.iter().filter_map(|v| v.as_str().map(String::from)))
            }
            _ => {}
        }
    }
    bail!(
        "preset '{preset}' has no binaryDir (directly or inherited); \
         pass an explicit build dir with -B"
    )
}

fn expand_preset_macros(s: &str, source_dir: &Path, preset: &str) -> String {
    let mut out = s
        .replace("${sourceDir}", &source_dir.to_string_lossy())
        .replace("${presetName}", preset)
        .replace(
            "${sourceDirName}",
            &source_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        )
        .replace("${dollar}", "$");
    // $env{VAR}
    while let Some(start) = out.find("$env{") {
        let Some(end) = out[start..].find('}') else {
            break;
        };
        let var = &out[start + 5..start + end];
        let val = std::env::var(var).unwrap_or_default();
        out = format!("{}{}{}", &out[..start], val, &out[start + end + 1..]);
    }
    out
}

fn cmake_version(cmake: &str) -> Option<String> {
    let out = Command::new(cmake).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .next()?
        .strip_prefix("cmake version ")
        .map(String::from)
}
