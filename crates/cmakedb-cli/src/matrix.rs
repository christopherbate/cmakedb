//! `cmakedb matrix` — the user guide's multi-configuration workflow
//! (record per preset, intersect findings with `--also`) as one command.
//!
//! Records every requested preset sequentially into
//! `.cmakedb/matrix/<preset>.db`, prints a per-preset summary table to
//! stderr, then runs all enabled lint passes on the first successful
//! recording intersected with every other successful one. Recording is
//! deliberately sequential: parallel cmake configures of one source tree
//! can collide (in-source generated files, FetchContent/download caches
//! shared across build dirs, and the recorder's intermediate
//! `trace.jsonl` living next to the databases), and interleaved
//! configure output would be unreadable.

use anyhow::{bail, Context, Result};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Instant;

use cmakedb_db::ingest::IngestStats;
use cmakedb_db::Db;
use cmakedb_passes::{report, Severity};

use crate::config::Config;
use crate::Format;

pub struct MatrixArgs {
    /// Presets to record; empty = discover all non-hidden configure
    /// presets from CMakePresets.json/CMakeUserPresets.json.
    pub presets: Vec<String>,
    pub fail_fast: bool,
    pub format: Format,
    pub output: Option<PathBuf>,
    pub fail_on: Option<String>,
    pub path: Vec<String>,
}

struct PresetOutcome {
    preset: String,
    db_path: PathBuf,
    seconds: f64,
    /// Ok(stats) for a recorded preset, Err(rendered error) for a failed
    /// one (configure or ingestion failure — either way no database).
    result: Result<IngestStats, String>,
}

pub fn run_matrix(args: MatrixArgs) -> Result<i32> {
    let source_dir = std::env::current_dir()?;
    let cfg = Config::load(&source_dir)?;
    let presets = if args.presets.is_empty() {
        let discovered = cmakedb_record::list_configure_presets(&source_dir)?;
        if discovered.is_empty() {
            bail!(
                "every configure preset in {} is hidden — pass --preset",
                source_dir.display()
            );
        }
        discovered
    } else {
        args.presets.clone()
    };

    let matrix_dir = source_dir.join(".cmakedb/matrix");
    std::fs::create_dir_all(&matrix_dir)
        .with_context(|| format!("creating {}", matrix_dir.display()))?;

    let mut outcomes: Vec<PresetOutcome> = Vec::new();
    for preset in &presets {
        let db_path = matrix_dir.join(db_file_name(preset));
        // Build where the preset itself says (its resolved binaryDir);
        // presets that declare none get a per-preset dir of ours.
        let build_dir = match cmakedb_record::preset_binary_dir(&source_dir, preset) {
            Ok(_) => None, // record() resolves the same binaryDir itself
            Err(_) => Some(source_dir.join(".cmakedb/matrix-build").join(preset)),
        };
        let opts = cmakedb_record::RecordOptions {
            preset: Some(preset.clone()),
            raw_args: Vec::new(),
            source_dir: Some(source_dir.clone()),
            build_dir,
            db_path: Some(db_path.clone()),
            cmake_exe: None,
            capture_scopes: cfg.record.capture_scopes,
        };
        eprintln!("cmakedb matrix: recording preset '{preset}'");
        let t0 = Instant::now();
        let result = cmakedb_record::record(&opts)
            .map(|r| r.stats)
            .map_err(|e| format!("{e:#}"));
        let failed = result.is_err();
        outcomes.push(PresetOutcome {
            preset: preset.clone(),
            db_path,
            seconds: t0.elapsed().as_secs_f64(),
            result,
        });
        if failed && args.fail_fast {
            eprintln!("cmakedb matrix: --fail-fast — skipping remaining presets");
            break;
        }
    }

    // Per-preset summary (stderr: stdout carries the findings).
    eprint!("{}", summary_table(&outcomes));

    let any_failed = outcomes.iter().any(|o| o.result.is_err());
    let ok: Vec<&PresetOutcome> = outcomes.iter().filter(|o| o.result.is_ok()).collect();
    if ok.is_empty() {
        bail!("no preset recorded successfully — nothing to lint");
    }

    // All enabled passes on the first successful recording, intersected
    // with every other successful one (same identity as `lint --also`).
    let db = Db::open(&ok[0].db_path)?;
    let also: Vec<PathBuf> = ok[1..].iter().map(|o| o.db_path.clone()).collect();
    let mut findings = crate::run_enabled_passes(&db, &cfg)?;
    crate::filter_findings(&mut findings, &args.path);
    crate::intersect_with_recordings(&mut findings, &also, &cfg, None)?;
    crate::apply_severity_overrides(&mut findings, &cfg)?;

    let rendered = match args.format {
        Format::Text => report::render_text(&findings, std::io::stdout().is_terminal()),
        Format::Json => report::render_json(&findings),
        Format::Sarif => report::render_sarif(&findings),
    };
    match &args.output {
        Some(p) => {
            std::fs::write(p, rendered).with_context(|| format!("writing {}", p.display()))?
        }
        None => print!("{rendered}"),
    }

    let threshold: Severity = args
        .fail_on
        .unwrap_or_else(|| cfg.lint.fail_on.clone())
        .parse()?;
    let gated = findings.iter().any(|f| f.severity >= threshold);
    Ok(if any_failed || gated { 1 } else { 0 })
}

/// Database file name for a preset: path separators (legal in preset
/// names? cmake says no, but be safe) and `:` become `_`.
fn db_file_name(preset: &str) -> String {
    let safe: String = preset
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '_'
            } else {
                c
            }
        })
        .collect();
    format!("{safe}.db")
}

fn summary_table(outcomes: &[PresetOutcome]) -> String {
    let name_w = outcomes
        .iter()
        .map(|o| o.preset.chars().count())
        .chain(["preset".len()])
        .max()
        .unwrap_or(6);
    let mut out = String::new();
    out.push_str(&format!(
        "\n{:name_w$}  {:>8}  {:>8}  {:>8}  status\n",
        "preset", "events", "targets", "seconds"
    ));
    for o in outcomes {
        match &o.result {
            Ok(stats) => out.push_str(&format!(
                "{:name_w$}  {:>8}  {:>8}  {:>8.2}  ok\n",
                o.preset, stats.events, stats.targets, o.seconds
            )),
            Err(e) => out.push_str(&format!(
                "{:name_w$}  {:>8}  {:>8}  {:>8.2}  FAILED: {}\n",
                o.preset,
                "-",
                "-",
                o.seconds,
                e.lines().next().unwrap_or("recording failed")
            )),
        }
    }
    out.push('\n');
    out
}
