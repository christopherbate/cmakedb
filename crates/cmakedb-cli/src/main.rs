//! `cmakedb` CLI (design §2.1).

mod baseline;
mod config;
mod deps;
mod explain;
mod graph;
mod matrix;
mod profile;

use cmakedb_db::diff;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command as ProcCommand;

use cmakedb_db::Db;
use cmakedb_passes::{builtin_passes, report, Finding, Severity};
use config::Config;

#[derive(Parser)]
#[command(
    name = "cmakedb",
    version,
    about = "Semantic analysis toolkit for CMake, built on execution tracing",
    long_about = "cmakedb records a real `cmake` configure (JSON trace + File API), joins it \
                  with lossless source ASTs into a queryable SQLite database, and answers \
                  semantic questions (provenance, dead code, visibility) over that recording."
)]
struct Cli {
    /// Path to the recording database (default: .cmakedb/trace.db).
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
    Sarif,
}

#[derive(Subcommand)]
enum Command {
    /// Record a cmake configure run into a semantic database.
    Record {
        /// Configure preset from CMakePresets.json.
        #[arg(long)]
        preset: Option<String>,
        #[arg(long)]
        source_dir: Option<PathBuf>,
        #[arg(long)]
        build_dir: Option<PathBuf>,
        /// Capture scope snapshots and cross-validate the replay (§3.2);
        /// adds ~10-20% configure time. Also via `.cmakedb.toml`
        /// [record] capture-scopes.
        #[arg(long)]
        capture_scopes: bool,
        /// Raw cmake arguments after `--` (e.g. `-- -S . -B build -DFOO=ON`).
        #[arg(last = true)]
        raw: Vec<String>,
    },
    /// Explain why a target links a library, with file:line at every hop.
    WhyLinks {
        target: String,
        lib: String,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },
    /// Explain why a target has an include directory.
    WhyIncludes {
        target: String,
        dir: String,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },
    /// Explain why a target has a compile definition/option.
    WhyFlag {
        target: String,
        flag: String,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },
    /// Explain why something did NOT happen (negative provenance).
    WhyNot {
        #[command(subcommand)]
        what: WhyNotWhat,
        #[arg(long, value_enum, default_value = "text", global = true)]
        format: Format,
    },
    /// Write history of a variable (time-travel table).
    WhyValue {
        variable: String,
        /// Restrict to the write observed by the read at file:line.
        #[arg(long)]
        at: Option<String>,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },
    /// Dead-code detection.
    Dead {
        #[command(subcommand)]
        what: DeadWhat,
        /// Only report findings under this source-relative path prefix
        /// (repeatable; e.g. --path src/net).
        #[arg(long, global = true)]
        path: Vec<String>,
        /// Additional recordings (other presets/platforms); only findings
        /// present in EVERY recording are reported (repeatable).
        #[arg(long, global = true)]
        also: Vec<PathBuf>,
    },
    /// Incompatible multi-writes to variables (overwritten before read).
    Clobbers {
        /// Only report findings under this source-relative path prefix.
        #[arg(long)]
        path: Vec<String>,
        /// Additional recordings; only findings present in EVERY recording
        /// are reported (repeatable).
        #[arg(long)]
        also: Vec<PathBuf>,
    },
    /// Macro writes that escaped into the caller's scope.
    ScopeLeaks {
        /// Only report findings under this source-relative path prefix.
        #[arg(long)]
        path: Vec<String>,
        /// Additional recordings; only findings present in EVERY recording
        /// are reported (repeatable).
        #[arg(long)]
        also: Vec<PathBuf>,
    },
    /// PUBLIC requirements no consumer needs (cross-checked with dep files).
    Overshare {
        /// Build directory to ingest `ninja -t deps` from before analysis.
        #[arg(long)]
        deps_from: Option<PathBuf>,
        /// Only report findings under this source-relative path prefix.
        #[arg(long)]
        path: Vec<String>,
        /// Additional recordings; only findings present in EVERY recording
        /// are reported (repeatable).
        #[arg(long)]
        also: Vec<PathBuf>,
    },
    /// Report (--check) or apply (--fix) mechanical modernization patches.
    /// --fix re-records and auto-reverts if the build graph changes.
    Modernize {
        /// Report legacy constructs and show planned patches as diffs.
        #[arg(long)]
        check: bool,
        /// Apply the patches, then verify build-graph isomorphism.
        #[arg(long, conflicts_with = "check")]
        fix: bool,
        /// Only touch legacy commands under this source-relative path
        /// prefix (insertions may still land elsewhere: a directory
        /// command can affect targets defined in child directories).
        #[arg(long)]
        path: Vec<String>,
    },
    /// Export the dependency graph (DOT / Mermaid / JSON) with
    /// visibility-styled edges and per-edge origin file:line.
    Graph {
        #[arg(long, value_enum, default_value = "dot")]
        format: graph::GraphFormat,
        /// Limit to this target's forward dependency closure (followed
        /// through every visibility, including PRIVATE).
        #[arg(long)]
        target: Option<String>,
        /// Keep only targets defined under this source-relative path
        /// prefix (repeatable; e.g. --path src/net).
        #[arg(long)]
        path: Vec<String>,
        /// Drop external (non-target) link destinations.
        #[arg(long)]
        no_external: bool,
    },
    /// Run a language server (stdio) backed by the most recent recording.
    Lsp,
    /// Configure-time hotspot report from the recorded event timings.
    /// Self time is approximate (time-to-next-event); scope totals are
    /// true wall-clock inclusive spans.
    Profile {
        /// Rows per section.
        #[arg(long, default_value_t = 15)]
        top: usize,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
        /// Emit folded stacks from the scope tree (flamegraph.pl /
        /// inferno / speedscope input) instead of the report; each line's
        /// value is that scope's exclusive time in microseconds.
        #[arg(long, conflicts_with = "compare")]
        flamegraph: bool,
        /// Compare against another recording: per-scope-name inclusive
        /// deltas (this recording minus OTHER_DB), sorted by |delta|.
        #[arg(long, value_name = "OTHER_DB")]
        compare: Option<PathBuf>,
    },
    /// What did this line do? Every recorded evaluation of the command
    /// at a source location: expanded arguments, call chains, effects
    /// (writes, edges, requirements, targets), and their influence.
    Explain {
        /// Source location as file:line (source-relative or absolute).
        location: String,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },
    /// Inventory of external dependencies (find_package / FetchContent /
    /// ExternalProject) as the configure resolved them, with resolution
    /// status, pinning classification, and CycloneDX 1.5 export.
    Deps {
        #[arg(long, value_enum, default_value = "text")]
        format: deps::DepsFormat,
    },
    /// Run raw SQL against the database.
    Query { sql: String },
    /// Run all enabled passes.
    Lint {
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
        #[arg(long)]
        output: Option<PathBuf>,
        /// Exit non-zero at/above this severity: note|warning|error.
        #[arg(long)]
        fail_on: Option<String>,
        /// Only report findings under this source-relative path prefix
        /// (repeatable). fail-on applies to the filtered set.
        #[arg(long)]
        path: Vec<String>,
        /// Additional recordings (other presets/platforms); only findings
        /// present in EVERY recording survive, and fail-on applies to that
        /// intersection (repeatable).
        #[arg(long)]
        also: Vec<PathBuf>,
        /// Report only findings absent from this baseline file
        /// (finding-level ratchet, user guide §8); fail-on applies to the
        /// surviving set.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Write the current (post-filter) findings as a baseline file,
        /// then report normally. Typically run from the main branch
        /// without --baseline.
        #[arg(long)]
        write_baseline: Option<PathBuf>,
        /// Skip user passes (`.cmakedb/passes/*.sql`) and run only the
        /// built-ins. Use when linting a repository you do not trust:
        /// user passes are SQL supplied by the tree being analyzed.
        #[arg(long)]
        no_user_passes: bool,
    },
    /// Record every configure preset and lint the multi-config
    /// intersection (the user guide's multi-configuration workflow in one
    /// command). Presets are recorded sequentially into
    /// .cmakedb/matrix/<preset>.db; a failing preset is reported and the
    /// rest still record (exit non-zero if any failed).
    Matrix {
        /// Preset(s) to record (repeatable). Default: every non-hidden
        /// configure preset from CMakePresets.json/CMakeUserPresets.json.
        #[arg(long)]
        preset: Vec<String>,
        /// Stop at the first preset that fails to record instead of
        /// continuing and reporting per-preset status.
        #[arg(long)]
        fail_fast: bool,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
        /// Write the intersected findings here instead of stdout (the
        /// per-preset summary table always goes to stderr).
        #[arg(long)]
        output: Option<PathBuf>,
        /// Exit non-zero at/above this severity in the intersected
        /// findings: note|warning|error. A failed preset is non-zero
        /// regardless.
        #[arg(long)]
        fail_on: Option<String>,
        /// Only report findings under this source-relative path prefix
        /// (repeatable). fail-on applies to the filtered set.
        #[arg(long)]
        path: Vec<String>,
        /// Skip user passes (`.cmakedb/passes/*.sql`) and run only the
        /// built-ins. Use when linting a repository you do not trust:
        /// user passes are SQL supplied by the tree being analyzed.
        #[arg(long)]
        no_user_passes: bool,
    },
    /// Compare two recordings (presets, commits).
    Diff {
        db1: PathBuf,
        db2: PathBuf,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },
}

#[derive(Subcommand)]
enum WhyNotWhat {
    /// Why does a target NOT link a library?
    Links { target: String, lib: String },
    /// Why does a target NOT exist?
    Target { name: String },
    /// Why was a variable never set?
    Set { variable: String },
}

#[derive(Subcommand)]
enum DeadWhat {
    /// option()s never read.
    Options,
    /// Functions/macros never invoked.
    Functions,
    /// .cmake files / directories never executed.
    Modules,
    /// Variables set but never read (noise-filtered).
    Variables,
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("cmakedb: error: {e:#}");
            std::process::exit(2);
        }
    }
}

fn open_db(cli_db: &Option<PathBuf>) -> Result<Db> {
    let path = cli_db
        .clone()
        .unwrap_or_else(|| PathBuf::from(".cmakedb/trace.db"));
    if !path.exists() {
        bail!(
            "no recording at {} — run `cmakedb record` first (or pass --db)",
            path.display()
        );
    }
    Db::open(&path)
}

fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Record {
            preset,
            source_dir,
            build_dir,
            capture_scopes,
            raw,
        } => {
            let cwd = std::env::current_dir()?;
            let cfg = Config::load(source_dir.as_deref().unwrap_or(&cwd))?;
            let opts = cmakedb_record::RecordOptions {
                preset: preset.or(cfg.record.preset),
                raw_args: raw,
                source_dir,
                build_dir,
                db_path: cli.db,
                cmake_exe: None,
                capture_scopes: capture_scopes || cfg.record.capture_scopes,
            };
            let result = cmakedb_record::record(&opts)?;
            let size = std::fs::metadata(&result.db_path)
                .map(|m| m.len())
                .unwrap_or(0);
            println!(
                "Recorded {} command evaluations across {} files → {} ({:.1} MB)",
                result.stats.events,
                result.stats.files,
                result.db_path.display(),
                size as f64 / 1e6
            );
            println!(
                "  {} AST-joined events, {} variable writes, {} reads, {} targets, {} link edges",
                result.stats.joined_events,
                result.stats.var_writes,
                result.stats.var_reads,
                result.stats.targets,
                result.stats.edges
            );
            Ok(0)
        }

        Command::WhyLinks {
            target,
            lib,
            format,
        } => {
            let db = open_db(&cli.db)?;
            let ex = cmakedb_passes::provenance::why_links(&db, &target, &lib)?;
            print_explanation(&ex, format, &format!("{target} links {lib} via:"));
            Ok(0)
        }
        Command::WhyIncludes {
            target,
            dir,
            format,
        } => {
            let db = open_db(&cli.db)?;
            let ex = cmakedb_passes::provenance::why_requirement(&db, &target, "include", &dir)?;
            print_explanation(
                &ex,
                format,
                &format!("{target} gets include dir {dir} via:"),
            );
            Ok(0)
        }
        Command::WhyFlag {
            target,
            flag,
            format,
        } => {
            let db = open_db(&cli.db)?;
            // Try defines first, then compile options.
            let ex = cmakedb_passes::provenance::why_requirement(&db, &target, "define", &flag)
                .or_else(|_| {
                    cmakedb_passes::provenance::why_requirement(&db, &target, "option", &flag)
                })?;
            print_explanation(&ex, format, &format!("{target} gets flag {flag} via:"));
            Ok(0)
        }

        Command::WhyNot { what, format } => {
            let db = open_db(&cli.db)?;
            let missing = match what {
                WhyNotWhat::Links { target, lib } => {
                    cmakedb_passes::whynot::Missing::Link { target, dep: lib }
                }
                WhyNotWhat::Target { name } => cmakedb_passes::whynot::Missing::Target { name },
                WhyNotWhat::Set { variable } => {
                    cmakedb_passes::whynot::Missing::Variable { name: variable }
                }
            };
            let wn = cmakedb_passes::whynot::why_not(&db, &missing)?;
            match format {
                Format::Json => println!("{}", serde_json::to_string_pretty(&wn)?),
                Format::Sarif => bail!("why-not supports --format text|json"),
                Format::Text => print!("{}", cmakedb_passes::whynot::render_text(&wn)),
            }
            Ok(0)
        }

        Command::WhyValue {
            variable,
            at,
            format,
        } => {
            let db = open_db(&cli.db)?;
            let at_parsed = match &at {
                Some(s) => {
                    let (f, l) = s.rsplit_once(':').context("--at expects file:line")?;
                    Some((f, l.parse::<i64>().context("--at expects file:line")?))
                }
                None => None,
            };
            let records = cmakedb_passes::history::why_value(&db, &variable, at_parsed)?;
            match format {
                Format::Json => println!("{}", serde_json::to_string_pretty(&records)?),
                _ => {
                    if records.is_empty() {
                        println!("no writes of '{variable}' in this recording");
                    } else {
                        println!("write history of {variable}:");
                        for r in &records {
                            let value = r.value.as_deref().unwrap_or("<not captured>");
                            println!(
                                "  [{}] {:14} {:28} = {} ({} read{})",
                                r.event_id,
                                format!("{}/{}", r.scope_kind, r.write_kind),
                                r.location,
                                value,
                                r.resolved_reads,
                                if r.resolved_reads == 1 { "" } else { "s" }
                            );
                            for hop in &r.call_chain {
                                println!("      via {hop}");
                            }
                        }
                    }
                }
            }
            Ok(0)
        }

        Command::Dead { what, path, also } => {
            let db = open_db(&cli.db)?;
            let cfg = Config::load(&std::env::current_dir()?)?;
            let id = match what {
                DeadWhat::Options => "dead-options",
                DeadWhat::Functions => "dead-functions",
                DeadWhat::Modules => "dead-modules",
                DeadWhat::Variables => "dead-variables",
            };
            run_single_pass(&db, &cfg, id, &path, &also)
        }
        Command::Clobbers { path, also } => {
            let db = open_db(&cli.db)?;
            let cfg = Config::load(&std::env::current_dir()?)?;
            run_single_pass(&db, &cfg, "clobbers", &path, &also)
        }
        Command::ScopeLeaks { path, also } => {
            let db = open_db(&cli.db)?;
            let cfg = Config::load(&std::env::current_dir()?)?;
            run_single_pass(&db, &cfg, "scope-leaks", &path, &also)
        }
        Command::Overshare {
            deps_from,
            path,
            also,
        } => {
            let mut db = open_db(&cli.db)?;
            if let Some(build) = &deps_from {
                ingest_deps(&mut db, build)?;
            }
            let cfg = Config::load(&std::env::current_dir()?)?;
            run_single_pass(&db, &cfg, "overshare", &path, &also)
        }

        Command::Modernize {
            check: _,
            fix,
            path,
        } => {
            let db_path = cli
                .db
                .clone()
                .unwrap_or_else(|| PathBuf::from(".cmakedb/trace.db"));
            let db = open_db(&cli.db)?;
            let cfg = Config::load(&std::env::current_dir()?)?;
            let mut findings = cmakedb_passes::modernize::plan(&db, &cfg.modernize.passes)?;
            filter_findings(&mut findings, &path);
            print!(
                "{}",
                report::render_text(&findings, std::io::stdout().is_terminal())
            );
            let patches: Vec<_> = findings.iter().filter_map(|f| f.fix.clone()).collect();
            if patches.is_empty() {
                println!("nothing to fix");
                return Ok(0);
            }
            if !fix {
                // --check (the default): render planned patches as unified
                // diffs against the recorded content.
                println!("\nplanned patches ({}):", patches.len());
                for p in &patches {
                    let diff_text = p.unified_diff(|file| {
                        db.conn
                            .query_row("SELECT content FROM files WHERE path = ?1", [file], |r| {
                                r.get::<_, Option<String>>(0)
                            })
                            .ok()
                            .flatten()
                    });
                    print!("{diff_text}");
                }
                println!("\nrun `cmakedb modernize --fix` to apply with verification");
                return Ok(0);
            }
            drop(db); // --fix re-records and replaces the database file
            let outcome = cmakedb_record::verify::apply_and_verify(&db_path, &patches)?;
            if outcome.verified() {
                println!(
                    "applied {} patch(es) across {} file(s); re-recorded and verified: \
                     build graph is unchanged",
                    patches.len(),
                    outcome.applied_files.len()
                );
                Ok(0)
            } else {
                eprintln!("verification FAILED — all edits rolled back:");
                for p in &outcome.problems {
                    eprintln!("  {p}");
                }
                Ok(1)
            }
        }
        Command::Graph {
            format,
            target,
            path,
            no_external,
        } => {
            let db = open_db(&cli.db)?;
            let g = graph::build(&db, target.as_deref(), &path, !no_external)?;
            print!("{}", graph::render(&g, format)?);
            Ok(0)
        }
        Command::Lsp => {
            cmakedb_lsp::server::run_stdio()?;
            Ok(0)
        }

        Command::Profile {
            top,
            format,
            flamegraph,
            compare,
        } => {
            let db = open_db(&cli.db)?;
            if flamegraph {
                if !matches!(format, Format::Text) {
                    bail!(
                        "--flamegraph emits folded stacks (text only) — drop --format \
                         and redirect stdout to a .folded file"
                    );
                }
                print!("{}", profile::flamegraph(&db)?);
                return Ok(0);
            }
            if let Some(other) = compare {
                let other_db = Db::open(&other)?;
                let c = profile::compare(&db, &other_db, top)?;
                match format {
                    Format::Json => println!("{}", serde_json::to_string_pretty(&c)?),
                    Format::Sarif => bail!("profile supports --format text|json"),
                    Format::Text => print!("{}", profile::render_compare_text(&c)),
                }
                return Ok(0);
            }
            let p = profile::profile(&db, top)?;
            match format {
                Format::Json => println!("{}", serde_json::to_string_pretty(&p)?),
                Format::Sarif => bail!("profile supports --format text|json"),
                Format::Text => print!("{}", profile::render_text(&p)),
            }
            Ok(0)
        }

        Command::Explain { location, format } => {
            let db = open_db(&cli.db)?;
            let (file, line) = location
                .rsplit_once(':')
                .context("explain expects a file:line location")?;
            let line: i64 = line
                .parse()
                .context("explain expects a file:line location")?;
            let ex = explain::explain(&db, file, line)?;
            match format {
                Format::Json => println!("{}", serde_json::to_string_pretty(&ex)?),
                Format::Sarif => bail!("explain supports --format text|json"),
                Format::Text => print!("{}", explain::render_text(&ex)),
            }
            Ok(0)
        }

        Command::Deps { format } => {
            let db = open_db(&cli.db)?;
            let inv = deps::inventory(&db)?;
            match format {
                deps::DepsFormat::Text => print!("{}", deps::render_text(&inv)),
                deps::DepsFormat::Json => println!("{}", serde_json::to_string_pretty(&inv)?),
                deps::DepsFormat::Cyclonedx => println!(
                    "{}",
                    serde_json::to_string_pretty(&deps::render_cyclonedx(&inv))?
                ),
            }
            Ok(0)
        }

        Command::Query { sql } => {
            let db = open_db(&cli.db)?;
            let mut stmt = db.conn.prepare(&sql)?;
            let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            println!("{}", names.join("\t"));
            let n = names.len();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let mut cells = Vec::with_capacity(n);
                for i in 0..n {
                    let v = row.get_ref(i)?;
                    cells.push(match v {
                        rusqlite::types::ValueRef::Null => "NULL".to_string(),
                        rusqlite::types::ValueRef::Integer(x) => x.to_string(),
                        rusqlite::types::ValueRef::Real(x) => x.to_string(),
                        rusqlite::types::ValueRef::Text(t) => {
                            String::from_utf8_lossy(t).to_string()
                        }
                        rusqlite::types::ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()),
                    });
                }
                println!("{}", cells.join("\t"));
            }
            Ok(0)
        }

        Command::Lint {
            format,
            output,
            fail_on,
            path,
            also,
            baseline: baseline_path,
            write_baseline,
            no_user_passes,
        } => {
            let db = open_db(&cli.db)?;
            let cfg = Config::load(&std::env::current_dir()?)?;
            let mut findings =
                run_enabled_passes(&db, &cfg, UserPasses::from_flag(no_user_passes))?;
            filter_findings(&mut findings, &path);
            intersect_with_recordings(&mut findings, &also, &cfg, None)?;
            if let Some(bp) = &baseline_path {
                let suppressed = baseline::load(bp)?;
                findings.retain(|f| !suppressed.contains(&baseline::key(f)));
            }
            // Overrides apply after the reported set is decided (path /
            // --also / baseline filters): they change how a finding
            // gates and renders, never whether it is reported, and
            // fail-on below sees the overridden severities.
            apply_severity_overrides(&mut findings, &cfg)?;
            if let Some(bp) = &write_baseline {
                baseline::write(bp, &findings)?;
                eprintln!(
                    "wrote baseline with {} finding(s) to {}",
                    findings.len(),
                    bp.display()
                );
            }
            let rendered = match format {
                Format::Text => report::render_text(&findings, std::io::stdout().is_terminal()),
                Format::Json => report::render_json(&findings),
                Format::Sarif => report::render_sarif(&findings),
            };
            match output {
                Some(p) => std::fs::write(&p, rendered)
                    .with_context(|| format!("writing {}", p.display()))?,
                None => print!("{rendered}"),
            }
            let threshold: Severity = fail_on.unwrap_or(cfg.lint.fail_on.clone()).parse()?;
            let failed = findings.iter().any(|f| f.severity >= threshold);
            Ok(if failed { 1 } else { 0 })
        }

        Command::Matrix {
            preset,
            fail_fast,
            format,
            output,
            fail_on,
            path,
            no_user_passes,
        } => matrix::run_matrix(matrix::MatrixArgs {
            presets: preset,
            fail_fast,
            format,
            output,
            fail_on,
            path,
            user_passes: UserPasses::from_flag(no_user_passes),
        }),

        Command::Diff { db1, db2, format } => {
            let a = Db::open(&db1)?;
            let b = Db::open(&db2)?;
            let report = diff::diff(&a, &b)?;
            match format {
                Format::Json => println!("{}", report.render_json()),
                _ => print!("{}", report.render_text()),
            }
            Ok(if report.is_empty() { 0 } else { 1 })
        }
    }
}

/// Whether user-supplied passes from the analyzed tree may run.
///
/// Separate from `Config` on purpose: `.cmakedb.toml` and
/// `.cmakedb/passes/` both live in the repository under analysis, so the
/// trust decision belongs to whoever invoked cmakedb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserPasses {
    Allow,
    Skip,
}

impl UserPasses {
    /// Map the `--no-user-passes` flag to the policy it selects.
    pub fn from_flag(no_user_passes: bool) -> UserPasses {
        if no_user_passes {
            UserPasses::Skip
        } else {
            UserPasses::Allow
        }
    }
}

/// Run every enabled pass over one recording. Shared body of `lint` and
/// `matrix`; filters, intersection, baselines and severity overrides are
/// applied by the caller.
///
/// `user_passes` is a *caller* decision, never a config one: user passes
/// are SQL files that live in the analyzed tree, so a recording of a
/// hostile repository must not be able to opt itself back in.
fn run_enabled_passes(db: &Db, cfg: &Config, user_passes: UserPasses) -> Result<Vec<Finding>> {
    let mut findings: Vec<Finding> = Vec::new();
    let mut passes = builtin_passes();
    if user_passes == UserPasses::Allow {
        for sql in cfg.sql_passes()? {
            passes.push(Box::new(sql));
        }
    }
    for pass in passes {
        if !cfg.pass_enabled(pass.id()) {
            continue;
        }
        let pass_cfg = cfg.pass_config(pass.id());
        findings.extend(
            pass.run(db, &pass_cfg)
                .with_context(|| format!("pass {}", pass.id()))?,
        );
    }
    Ok(findings)
}

/// Keep only findings whose primary location falls under one of the given
/// source-relative path prefixes. Matching is by path component, so
/// `src/net` matches `src/net/CMakeLists.txt` but not `src/nettle/...`.
/// Related locations outside the filter stay attached — they are evidence.
fn filter_findings(findings: &mut Vec<Finding>, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let normalized: Vec<String> = paths
        .iter()
        .map(|p| {
            cmakedb_db::cmake_path_spelling(p)
                .trim_start_matches("./")
                .trim_end_matches('/')
                .to_string()
        })
        .collect();
    findings.retain(|f| {
        let file = f.primary.file.trim_start_matches("./");
        normalized
            .iter()
            .any(|p| file == p || file.starts_with(&format!("{p}/")))
    });
}

/// Multi-configuration intersection (user guide §1, blind spot B1): keep
/// only findings that appear — same rule, same declaration site — in every
/// additional recording too. A finding surviving all recorded
/// configurations cannot be a single-configuration artifact.
fn intersect_with_recordings(
    findings: &mut Vec<Finding>,
    also: &[PathBuf],
    cfg: &Config,
    single_pass: Option<&str>,
) -> Result<()> {
    if also.is_empty() {
        return Ok(());
    }
    // Finding identity is shared with the lint baseline (baseline.rs).
    let key = baseline::key;
    for db_path in also {
        let other = Db::open(db_path)
            .with_context(|| format!("opening --also recording {}", db_path.display()))?;
        let mut keys: std::collections::HashSet<_> = std::collections::HashSet::new();
        for pass in builtin_passes() {
            let run_it = match single_pass {
                Some(id) => pass.id() == id,
                None => cfg.pass_enabled(pass.id()),
            };
            if !run_it {
                continue;
            }
            for f in pass.run(&other, &cfg.pass_config(pass.id()))? {
                keys.insert(key(&f));
            }
        }
        findings.retain(|f| keys.contains(&key(f)));
    }
    let n = also.len() + 1;
    for f in findings.iter_mut() {
        f.message
            .push_str(&format!(" [present in all {n} recordings]"));
    }
    Ok(())
}

/// Apply `[lint.severity]` overrides from `.cmakedb.toml` (rule id →
/// note|warning|error) so teams tune the fail-on gate per rule without
/// forking passes. Unknown severity values fail loudly; overrides for
/// rules that produced no findings are harmless.
fn apply_severity_overrides(findings: &mut [Finding], cfg: &Config) -> Result<()> {
    if cfg.lint.severity.is_empty() {
        return Ok(());
    }
    let mut overrides = std::collections::HashMap::new();
    for (rule, sev) in &cfg.lint.severity {
        let parsed: Severity = sev
            .parse()
            .with_context(|| format!("[lint.severity] {rule} = \"{sev}\""))?;
        overrides.insert(rule.as_str(), parsed);
    }
    for f in findings.iter_mut() {
        if let Some(&s) = overrides.get(f.rule.as_str()) {
            f.severity = s;
        }
    }
    Ok(())
}

fn run_single_pass(
    db: &Db,
    cfg: &Config,
    id: &str,
    paths: &[String],
    also: &[PathBuf],
) -> Result<i32> {
    let pass = builtin_passes()
        .into_iter()
        .find(|p| p.id() == id)
        .with_context(|| format!("unknown pass {id}"))?;
    let mut findings = pass.run(db, &cfg.pass_config(id))?;
    filter_findings(&mut findings, paths);
    intersect_with_recordings(&mut findings, also, cfg, Some(id))?;
    print!(
        "{}",
        report::render_text(&findings, std::io::stdout().is_terminal())
    );
    Ok(if findings.is_empty() { 0 } else { 1 })
}

fn ingest_deps(db: &mut Db, build_dir: &Path) -> Result<()> {
    if !build_dir.join("build.ninja").exists() {
        bail!(
            "{} has no build.ninja — overshare dep ingestion currently supports the \
             Ninja generator (design §3.2 channel 4)",
            build_dir.display()
        );
    }
    let out = ProcCommand::new("ninja")
        .arg("-C")
        .arg(build_dir)
        .args(["-t", "deps"])
        .output()
        .context("running `ninja -t deps` — is ninja on PATH?")?;
    if !out.status.success() {
        bail!(
            "`ninja -t deps` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let n = cmakedb_db::ingest::ingest_ninja_deps(db, &text, build_dir)?;
    eprintln!(
        "ingested {n} header dependencies from {}",
        build_dir.display()
    );
    if n == 0 {
        eprintln!(
            "note: 0 deps — has the tree been built? (`ninja -C {}`)",
            build_dir.display()
        );
    }
    Ok(())
}

fn print_explanation(ex: &cmakedb_passes::provenance::Explanation, format: Format, heading: &str) {
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(ex).expect("serialize")),
        _ => {
            println!("{heading}");
            let mut out = String::new();
            cmakedb_passes::provenance::render_tree(&ex.tree, &mut out, "", true, 0);
            print!("{out}");
            if !ex.final_evidence.is_empty() {
                println!("final resolved evidence (File API):");
                for e in &ex.final_evidence {
                    println!("  {e}");
                }
            }
        }
    }
}
