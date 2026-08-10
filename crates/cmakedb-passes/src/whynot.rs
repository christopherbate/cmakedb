//! Negative provenance (Feature roadmap v2, Track A): why did something
//! NOT happen? The `why-*` family explains recorded facts; this module
//! explains their absence — the other half of real debugging.
//!
//! Engine: find every AST site that *could* have produced the missing
//! thing, then classify each against the recording:
//!  - **executed differently**: the site ran, but its expanded arguments
//!    produced something else;
//!  - **guard failed**: the site sits in a branch whose nearest *executed*
//!    guard chose another way — shown with the guard's observed condition
//!    values and where each value came from (one `why-value` hop);
//!  - **inside an uncalled function/macro**;
//!  - **file never ran** — with the includer sites that would have pulled
//!    it in classified recursively (depth-capped).
//!
//! No sites at all → an edit-distance suggestion against known names.

use anyhow::Result;
use cmakedb_db::Db;
use rusqlite::params;
use serde::Serialize;

use crate::{event_span, SourceSpan};

/// What the user reports as missing.
pub enum Missing {
    Link { target: String, dep: String },
    Target { name: String },
    Variable { name: String },
}

#[derive(Debug, Serialize)]
pub struct WhyNot {
    pub subject: String,
    /// Set when the thing actually exists — the answer is a redirect.
    pub already_exists: Option<String>,
    pub sites: Vec<Site>,
    /// Commands mentioning the token outside the direct candidate set
    /// (wrapper calls, list-building `set()`s) — context, not verdicts.
    pub mentions: Vec<Mention>,
    pub suggestion: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Site {
    pub location: SourceSpan,
    pub command: String,
    pub excerpt: String,
    pub status: SiteStatus,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SiteStatus {
    /// Ran, but the expanded arguments produced something else.
    ExecutedDifferently {
        observed: Vec<String>,
        /// Extra classification when we can tell what it did instead
        /// (e.g. the call links the dep into a *different* target).
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Never ran; the innermost *executed* guard chose another branch.
    GuardFailed { chain: Vec<Guard> },
    InUncalledFunction {
        name: String,
        defined_at: SourceSpan,
    },
    FileNeverRan {
        reason: String,
        includers: Vec<Site>,
    },
    /// Never ran and no executed guard was found on the ancestor chain
    /// (e.g. the whole call path was dynamic).
    NeverExecuted,
}

/// One guard on the path from the site outward. Unexecuted guards are
/// listed for context; the outermost entry is the executed one whose
/// condition decided the fate.
#[derive(Debug, Serialize)]
pub struct Guard {
    pub location: SourceSpan,
    pub condition: String,
    pub executed: bool,
    /// Distinct expanded evaluations (args joined, count).
    pub evaluations: Vec<(String, i64)>,
    /// Condition variables with observed value and its origin.
    pub values: Vec<GuardValue>,
}

#[derive(Debug, Serialize)]
pub struct GuardValue {
    pub name: String,
    pub value: Option<String>,
    pub write_kind: Option<String>,
    pub origin: Option<SourceSpan>,
}

#[derive(Debug, Serialize)]
pub struct Mention {
    pub location: SourceSpan,
    pub command: String,
    pub excerpt: String,
    pub executed: bool,
}

pub fn why_not(db: &Db, missing: &Missing) -> Result<WhyNot> {
    match missing {
        Missing::Link { target, dep } => why_not_link(db, target, dep),
        Missing::Target { name } => why_not_target(db, name),
        Missing::Variable { name } => why_not_variable(db, name),
    }
}

fn why_not_link(db: &Db, target: &str, dep: &str) -> Result<WhyNot> {
    let subject = format!("{target} does not link {dep}.");
    let Some(tid) = db.target_id(target)? else {
        return Ok(WhyNot {
            subject,
            already_exists: None,
            sites: vec![],
            mentions: vec![],
            suggestion: Some(format!(
                "target '{target}' itself does not exist in this recording — \
                 run `cmakedb why-not target {target}` first"
            )),
        });
    };
    // Does the edge already exist (directly)?
    let mut stmt = db.conn.prepare(
        "SELECT e.dst, t.name FROM tgt_edges e
         LEFT JOIN targets t ON t.id = e.dst_target WHERE e.src_target = ?1",
    )?;
    let edges: Vec<(String, Option<String>)> = stmt
        .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (dst, dst_name) in &edges {
        if crate::provenance::dst_matches(dst, dst_name.as_deref(), dep) {
            return Ok(WhyNot {
                subject,
                already_exists: Some(format!(
                    "{target} DOES link {dep} — run `cmakedb why-links {target} {dep}` \
                     for the propagation chain"
                )),
                sites: vec![],
                mentions: vec![],
                suggestion: None,
            });
        }
    }

    // Candidate sites: link commands whose raw text names the dependency.
    let sites = classify_sites(
        db,
        &["target_link_libraries", "link_libraries"],
        dep,
        false,
        Some(&|db, events| link_produced(db, events, tid, target, dep)),
    )?;
    let mentions = dedup_mentions(
        &sites,
        collect_mentions(db, dep, &["target_link_libraries", "link_libraries"])?,
    );
    let suggestion = if sites.is_empty() && mentions.is_empty() {
        let mut known: Vec<String> = edges
            .iter()
            .map(|(d, n)| n.clone().unwrap_or_else(|| d.clone()))
            .collect();
        let mut stmt = db.conn.prepare("SELECT name FROM targets")?;
        known.extend(
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
        crate::undefined::closest_name(dep, &known)
            .map(|s| format!("no site mentions '{dep}' — did you mean '{s}'?"))
    } else {
        None
    };
    Ok(WhyNot {
        subject,
        already_exists: None,
        sites,
        mentions,
        suggestion,
    })
}

fn why_not_target(db: &Db, name: &str) -> Result<WhyNot> {
    let subject = format!("target '{name}' was never created.");
    if let Some(tid) = db.target_id(name)? {
        let loc: Option<String> = db
            .conn
            .query_row(
                "SELECT defined_event FROM targets WHERE id = ?1",
                [tid],
                |r| r.get::<_, Option<i64>>(0),
            )?
            .and_then(|e| db.event_location(e).ok());
        return Ok(WhyNot {
            subject,
            already_exists: Some(format!(
                "target '{name}' DOES exist{}",
                loc.map(|l| format!(", defined at {l}")).unwrap_or_default()
            )),
            sites: vec![],
            mentions: vec![],
            suggestion: None,
        });
    }
    let defining = ["add_library", "add_executable", "add_custom_target"];
    // wrappers=true: `add_*` wrapper calls whose first argument is the
    // target name (add_llvm_library, add_my_executable, ...) are candidate
    // definition sites too — huge projects define almost everything through
    // wrappers.
    let sites = classify_sites(db, &defining, name, true, None)?;
    let mentions = dedup_mentions(&sites, collect_mentions(db, name, &defining)?);
    let suggestion = if sites.is_empty() && mentions.is_empty() {
        let mut stmt = db.conn.prepare("SELECT name FROM targets")?;
        let known: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        crate::undefined::closest_name(name, &known)
            .map(|s| format!("no site mentions '{name}' — did you mean '{s}'?"))
    } else {
        None
    };
    Ok(WhyNot {
        subject,
        already_exists: None,
        sites,
        mentions,
        suggestion,
    })
}

fn why_not_variable(db: &Db, name: &str) -> Result<WhyNot> {
    let subject = format!("variable '{name}' was never set.");
    let writes: i64 = db.conn.query_row(
        "SELECT count(*) FROM var_writes WHERE name = ?1 AND write_kind != 'synthetic'",
        [name],
        |r| r.get(0),
    )?;
    if writes > 0 {
        return Ok(WhyNot {
            subject,
            already_exists: Some(format!(
                "'{name}' WAS written {writes} time(s) — run `cmakedb why-value {name}` \
                 for the history"
            )),
            sites: vec![],
            mentions: vec![],
            suggestion: None,
        });
    }
    let setters = ["set", "option", "list", "string"];
    let sites = classify_sites(db, &setters, name, false, None)?;
    let mentions = dedup_mentions(&sites, collect_mentions(db, name, &setters)?);
    let suggestion = if sites.is_empty() && mentions.is_empty() {
        let mut stmt = db
            .conn
            .prepare("SELECT DISTINCT name FROM var_writes WHERE length(name) >= 4")?;
        let known: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        crate::undefined::closest_name(name, &known)
            .map(|s| format!("no site mentions '{name}' — did you mean '{s}'?"))
    } else {
        None
    };
    Ok(WhyNot {
        subject,
        already_exists: None,
        sites,
        mentions,
        suggestion,
    })
}

/// For link sites that DID execute: did any of their events produce a
/// matching edge? Returns None when they did (site is then irrelevant to
/// "why not"), or the observed expanded args when they produced other
/// things.
fn link_produced(
    db: &Db,
    events: &[i64],
    tid: i64,
    target: &str,
    dep: &str,
) -> Option<(Vec<String>, Option<String>)> {
    for e in events {
        let ok: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM tgt_edges g
                 LEFT JOIN targets t ON t.id = g.dst_target
                 WHERE g.origin_event = ?1 AND g.src_target = ?2",
                params![e, tid],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if ok > 0 {
            // The site created edges for our target; check match.
            let mut stmt = db
                .conn
                .prepare(
                    "SELECT g.dst, t.name FROM tgt_edges g
                     LEFT JOIN targets t ON t.id = g.dst_target
                     WHERE g.origin_event = ?1 AND g.src_target = ?2",
                )
                .ok()?;
            let dsts: Vec<(String, Option<String>)> = stmt
                .query_map(params![e, tid], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok()?
                .collect::<rusqlite::Result<_>>()
                .ok()?;
            if dsts
                .iter()
                .any(|(d, n)| crate::provenance::dst_matches(d, n.as_deref(), dep))
            {
                return None; // it actually produced the edge
            }
        }
    }
    // Executed but didn't produce the edge for this target: show what the
    // evaluations actually expanded to.
    let mut observed = Vec::new();
    for e in events.iter().take(4) {
        if let Ok(args) =
            db.conn
                .query_row("SELECT args_json FROM events WHERE id = ?1", [e], |r| {
                    r.get::<_, String>(0)
                })
        {
            let parsed: Vec<String> = serde_json::from_str(&args).unwrap_or_default();
            observed.push(parsed.join(" "));
        }
    }
    // Was this site really about a *different* target? If its events did
    // create matching edges, just with another src, say so — the site is a
    // red herring for our target, not a failed attempt.
    let mut others: Vec<String> = Vec::new();
    for e in events {
        let mut stmt = db
            .conn
            .prepare(
                "SELECT s.name, g.dst, t.name FROM tgt_edges g
                 JOIN targets s ON s.id = g.src_target
                 LEFT JOIN targets t ON t.id = g.dst_target
                 WHERE g.origin_event = ?1 AND g.src_target != ?2",
            )
            .ok()?;
        let rows: Vec<(String, String, Option<String>)> = stmt
            .query_map(params![e, tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .ok()?
            .collect::<rusqlite::Result<_>>()
            .ok()?;
        for (src, dst, dst_name) in rows {
            if crate::provenance::dst_matches(&dst, dst_name.as_deref(), dep)
                && !others.contains(&src)
            {
                others.push(src);
            }
        }
    }
    let note = (!others.is_empty()).then(|| {
        format!(
            "this call links {dep} into {}, not {target}",
            others.join(", ")
        )
    });
    Some((observed, note))
}

/// Word-boundary containment of `token` in raw argument text.
pub(crate) fn mentions_token(text: &str, token: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || "_:.+-".contains(c);
    let mut start = 0;
    while let Some(pos) = text[start..].find(token) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !text[..abs]
                .chars()
                .next_back()
                .map(&is_word)
                .unwrap_or(false);
        let after = abs + token.len();
        let after_ok =
            after >= text.len() || !text[after..].chars().next().map(&is_word).unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        start = abs + token.len().max(1);
    }
    false
}

type ProducedCheck<'a> = &'a dyn Fn(&Db, &[i64]) -> Option<(Vec<String>, Option<String>)>;

fn classify_sites(
    db: &Db,
    commands: &[&str],
    token: &str,
    wrappers: bool,
    produced_check: Option<ProducedCheck>,
) -> Result<Vec<Site>> {
    classify_sites_depth(db, commands, token, wrappers, produced_check, 0)
}

fn classify_sites_depth(
    db: &Db,
    commands: &[&str],
    token: &str,
    wrappers: bool,
    produced_check: Option<ProducedCheck>,
    depth: usize,
) -> Result<Vec<Site>> {
    let placeholders = commands.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    // With wrappers on, any `add_*` call whose first argument is the token
    // also qualifies (project-defined wrapper functions around
    // add_library/add_executable); first-arg position is checked below.
    let name_filter = if wrappers {
        format!("(c.name_lower IN ({placeholders}) OR c.name_lower LIKE 'add\\_%' ESCAPE '\\')")
    } else {
        format!("c.name_lower IN ({placeholders})")
    };
    let sql = format!(
        "SELECT c.node_id, c.name, c.args_text, n.file_id, n.line, f.path, f.in_source
         FROM commands c
         JOIN ast_nodes n ON n.id = c.node_id
         JOIN files f ON f.id = n.file_id
         WHERE {name_filter} AND c.args_text LIKE '%' || ? || '%'
         ORDER BY f.path, n.line"
    );
    let mut stmt = db.conn.prepare(&sql)?;
    let mut bind: Vec<&dyn rusqlite::ToSql> =
        commands.iter().map(|c| c as &dyn rusqlite::ToSql).collect();
    bind.push(&token);
    let rows: Vec<(i64, String, String, i64, i64, String, i64)> = stmt
        .query_map(&bind[..], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut sites = Vec::new();
    for (node, cmd, args_text, file_id, line, path, in_source) in rows {
        if in_source == 0 || !mentions_token(&args_text, token) {
            continue;
        }
        // Wrapper rows (not in the exact-command list) only count when the
        // token is the *first* argument — the position that names the
        // created target in every add_* convention. Builtin add_* commands
        // that don't define a target named by their first argument are
        // excluded outright.
        const NON_DEFINING_ADD: &[&str] = &[
            "add_subdirectory",
            "add_dependencies",
            "add_test",
            "add_definitions",
            "add_compile_definitions",
            "add_compile_options",
            "add_link_options",
            "add_custom_command",
        ];
        let cmd_lower = cmd.to_ascii_lowercase();
        if !commands.contains(&cmd_lower.as_str())
            && (NON_DEFINING_ADD.contains(&cmd_lower.as_str())
                || args_text
                    .split_whitespace()
                    .next()
                    .map(|a| a.trim_matches('"'))
                    != Some(token))
        {
            continue;
        }
        let location = SourceSpan::new(db.display_path(&path), line);
        let excerpt = truncate(&format!("{cmd}({args_text})"), 100);

        // Did this node execute?
        let mut ev_stmt = db
            .conn
            .prepare("SELECT id FROM events WHERE node_id = ?1 ORDER BY id LIMIT 16")?;
        let events: Vec<i64> = ev_stmt
            .query_map([node], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;

        let status = if !events.is_empty() {
            match produced_check {
                Some(check) => match check(db, &events) {
                    None => continue, // it produced the thing; not a why-not site
                    Some((observed, note)) => SiteStatus::ExecutedDifferently { observed, note },
                },
                // For target/variable sites: node executed but the thing
                // doesn't exist — the runtime name must have differed.
                None => {
                    let mut observed = Vec::new();
                    for e in events.iter().take(4) {
                        let args: String = db.conn.query_row(
                            "SELECT args_json FROM events WHERE id = ?1",
                            [e],
                            |r| r.get(0),
                        )?;
                        let parsed: Vec<String> = serde_json::from_str(&args).unwrap_or_default();
                        observed.push(parsed.join(" "));
                    }
                    SiteStatus::ExecutedDifferently {
                        observed,
                        note: None,
                    }
                }
            }
        } else {
            classify_unexecuted(db, node, file_id, depth)?
        };
        sites.push(Site {
            location,
            command: cmd,
            excerpt,
            status,
        });
    }
    Ok(sites)
}

/// Classify a node that never executed: walk ancestors innermost-out for
/// the nearest *executed* guard, an uncalled function, or a dead file.
fn classify_unexecuted(db: &Db, node: i64, file_id: i64, depth: usize) -> Result<SiteStatus> {
    // File never ran at all?
    let file_events: i64 = db.conn.query_row(
        "SELECT count(*) FROM events WHERE file_id = ?1",
        [file_id],
        |r| r.get(0),
    )?;
    if file_events == 0 {
        return file_never_ran(db, file_id, depth);
    }

    let mut chain: Vec<Guard> = Vec::new();
    let mut cur = node;
    for _ in 0..64 {
        let parent: Option<i64> = db.conn.query_row(
            "SELECT parent_id FROM ast_nodes WHERE id = ?1",
            [cur],
            |r| r.get(0),
        )?;
        let Some(p) = parent else { break };
        let kind: String =
            db.conn
                .query_row("SELECT kind FROM ast_nodes WHERE id = ?1", [p], |r| {
                    r.get(0)
                })?;
        match kind.as_str() {
            "if_condition" => {
                if let Some(guard) = analyze_guard(db, p, cur)? {
                    let executed = guard.executed;
                    chain.push(guard);
                    if executed {
                        return Ok(SiteStatus::GuardFailed { chain });
                    }
                }
            }
            "function_def" | "macro_def" => {
                if let Some((name, def_span, called)) = function_info(db, p)? {
                    if !called {
                        return Ok(SiteStatus::InUncalledFunction {
                            name,
                            defined_at: def_span,
                        });
                    }
                }
            }
            _ => {}
        }
        cur = p;
    }
    if chain.is_empty() {
        Ok(SiteStatus::NeverExecuted)
    } else {
        Ok(SiteStatus::GuardFailed { chain })
    }
}

/// The branch header (if/elseif/else command) governing `inner` within
/// this if_condition: the header node latest-starting strictly before it.
fn analyze_guard(db: &Db, condition_node: i64, inner: i64) -> Result<Option<Guard>> {
    let inner_start: i64 = db.conn.query_row(
        "SELECT byte_start FROM ast_nodes WHERE id = ?1",
        [inner],
        |r| r.get(0),
    )?;
    let header: Option<(i64, i64, String, String)> = db
        .conn
        .query_row(
            "SELECT n.id, n.line, c.name, c.args_text
             FROM ast_nodes n JOIN commands c ON c.node_id = n.id
             WHERE n.parent_id = ?1
               AND n.kind IN ('if_command','elseif_command','else_command')
               AND n.byte_start < ?2
             ORDER BY n.byte_start DESC LIMIT 1",
            params![condition_node, inner_start],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    let Some((hnode, hline, hcmd, hargs)) = header else {
        return Ok(None);
    };
    let hpath: String = db.conn.query_row(
        "SELECT f.path FROM ast_nodes n JOIN files f ON f.id = n.file_id WHERE n.id = ?1",
        [hnode],
        |r| r.get(0),
    )?;
    let location = SourceSpan::new(db.display_path(&hpath), hline);
    let condition = format!("{hcmd}({hargs})");

    // Header evaluations (it executing while our branch didn't = it chose
    // another way).
    let mut ev_stmt = db.conn.prepare(
        "SELECT args_json, count(*) FROM events WHERE node_id = ?1
         GROUP BY args_json ORDER BY count(*) DESC LIMIT 5",
    )?;
    let evaluations: Vec<(String, i64)> = ev_stmt
        .query_map([hnode], |r| {
            let raw: String = r.get(0)?;
            let parsed: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
            Ok((parsed.join(" "), r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let executed = !evaluations.is_empty();

    // Condition variables (ast_var_refs holds ${} refs AND bare condition
    // identifiers) with their observed values at the header's events.
    let mut values = Vec::new();
    if executed {
        let mut ref_stmt = db
            .conn
            .prepare("SELECT DISTINCT name FROM ast_var_refs WHERE node_id = ?1")?;
        let names: Vec<String> = ref_stmt
            .query_map([hnode], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for name in names {
            let hit: Option<(Option<i64>, Option<String>, Option<String>, Option<i64>)> = db
                .conn
                .query_row(
                    "SELECT w.id, w.value, w.write_kind, w.event_id
                     FROM var_reads r
                     JOIN events e ON e.id = r.event_id
                     LEFT JOIN var_writes w ON w.id = r.resolved_write_id
                     WHERE r.name = ?1 AND e.node_id = ?2
                     ORDER BY r.id DESC LIMIT 1",
                    params![name, hnode],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })?;
            match hit {
                Some((Some(_), value, kind, ev)) => {
                    let preseed = ev.and_then(|e| preseed_label(db, e, kind.as_deref(), &name));
                    match preseed {
                        Some(label) => values.push(GuardValue {
                            name,
                            value,
                            write_kind: Some(label),
                            origin: None,
                        }),
                        None => values.push(GuardValue {
                            name,
                            value,
                            write_kind: kind,
                            origin: ev.map(|e| event_span(db, e)),
                        }),
                    }
                }
                Some((None, ..)) => values.push(GuardValue {
                    name,
                    value: None,
                    write_kind: None,
                    origin: None,
                }),
                None => values.push(GuardValue {
                    name,
                    value: None,
                    write_kind: None,
                    origin: None,
                }),
            }
        }
    }
    Ok(Some(Guard {
        location,
        condition,
        executed,
        evaluations,
        values,
    }))
}

/// Recorder-synthesized preseed writes (command-line -D / preset
/// cacheVariables) are anchored to the very first event; a resolved cache
/// write there has no meaningful source location. Returns the display
/// label to use instead, or None for ordinary writes.
fn preseed_label(db: &Db, event_id: i64, write_kind: Option<&str>, name: &str) -> Option<String> {
    if write_kind != Some("cache") {
        return None;
    }
    let first: i64 = db
        .conn
        .query_row("SELECT min(id) FROM events", [], |r| r.get(0))
        .unwrap_or(0);
    if event_id != first {
        return None;
    }
    let argv: String = db
        .conn
        .query_row("SELECT value FROM meta WHERE key = 'argv'", [], |r| {
            r.get(0)
        })
        .unwrap_or_default();
    let from_cli = argv.split_whitespace().any(|a| {
        a.strip_prefix("-D")
            .map(|rest| {
                let head = rest.split('=').next().unwrap_or("");
                head == name || head.split(':').next() == Some(name)
            })
            .unwrap_or(false)
    });
    Some(if from_cli {
        format!("from the command line: -D{name}")
    } else {
        "pre-seeded cache entry (command line or preset)".to_string()
    })
}

/// (name, definition span, was it ever called) for a function/macro def
/// node.
fn function_info(db: &Db, def_node: i64) -> Result<Option<(String, SourceSpan, bool)>> {
    let row: Option<(String, i64, String)> = db
        .conn
        .query_row(
            "SELECT c.args_text, n.line, f.path
             FROM ast_nodes n
             JOIN commands c ON c.node_id = n.id
             JOIN files f ON f.id = n.file_id
             WHERE n.parent_id = ?1 AND n.kind IN ('function_command','macro_command')
             LIMIT 1",
            [def_node],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    let Some((args_text, line, path)) = row else {
        return Ok(None);
    };
    let name = args_text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches('"')
        .to_string();
    if name.is_empty() {
        return Ok(None);
    }
    let called: i64 = db.conn.query_row(
        "SELECT count(*) FROM events WHERE cmd_lower = lower(?1)",
        [&name],
        |r| r.get(0),
    )?;
    Ok(Some((
        name,
        SourceSpan::new(db.display_path(&path), line),
        called > 0,
    )))
}

fn file_never_ran(db: &Db, file_id: i64, depth: usize) -> Result<SiteStatus> {
    let path: String =
        db.conn
            .query_row("SELECT path FROM files WHERE id = ?1", [file_id], |r| {
                r.get(0)
            })?;
    let display = db.display_path(&path);
    let basename = std::path::Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let (reason, needle, includer_cmds): (String, String, &[&str]) = if basename == "CMakeLists.txt"
    {
        let dir = std::path::Path::new(&display)
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        (
            format!("{display} never executed — its directory was never added"),
            dir,
            &["add_subdirectory"],
        )
    } else {
        (
            format!("{display} never executed — the module was never included"),
            basename.clone(),
            &["include"],
        )
    };
    let includers = if depth < 2 && !needle.is_empty() {
        classify_sites_depth(db, includer_cmds, &needle, false, None, depth + 1)?
    } else {
        vec![]
    };
    Ok(SiteStatus::FileNeverRan { reason, includers })
}

/// Drop mentions already shown as (top-level) candidate sites.
fn dedup_mentions(sites: &[Site], mentions: Vec<Mention>) -> Vec<Mention> {
    mentions
        .into_iter()
        .filter(|m| {
            !sites
                .iter()
                .any(|s| s.location.to_string() == m.location.to_string())
        })
        .collect()
}

fn truncate(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn collect_mentions(db: &Db, token: &str, exclude_cmds: &[&str]) -> Result<Vec<Mention>> {
    let mut stmt = db.conn.prepare(
        "SELECT c.name, c.args_text, n.id, n.line, f.path
         FROM commands c
         JOIN ast_nodes n ON n.id = c.node_id
         JOIN files f ON f.id = n.file_id
         WHERE f.in_source = 1 AND c.args_text LIKE '%' || ?1 || '%'
         ORDER BY f.path, n.line LIMIT 200",
    )?;
    let rows: Vec<(String, String, i64, i64, String)> = stmt
        .query_map([token], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (cmd, args_text, node, line, path) in rows {
        if exclude_cmds.contains(&cmd.to_ascii_lowercase().as_str())
            || !mentions_token(&args_text, token)
        {
            continue;
        }
        let executed: i64 = db.conn.query_row(
            "SELECT count(*) FROM events WHERE node_id = ?1",
            [node],
            |r| r.get(0),
        )?;
        out.push(Mention {
            location: SourceSpan::new(db.display_path(&path), line),
            command: cmd.clone(),
            excerpt: truncate(&format!("{cmd}({args_text})"), 90),
            executed: executed > 0,
        });
        if out.len() >= 8 {
            break;
        }
    }
    Ok(out)
}

/// Human rendering, tree-styled like why-links.
pub fn render_text(wn: &WhyNot) -> String {
    let mut out = String::new();
    if let Some(exists) = &wn.already_exists {
        out.push_str(&format!("{exists}\n"));
        return out;
    }
    out.push_str(&wn.subject);
    if wn.sites.is_empty() {
        out.push_str(" No candidate site in the project could have produced it.\n");
    } else {
        out.push_str(" Candidate sites:\n");
        for s in &wn.sites {
            render_site(&mut out, s, 1);
        }
    }
    if !wn.mentions.is_empty() {
        out.push_str("other commands mentioning it (context):\n");
        for m in &wn.mentions {
            out.push_str(&format!(
                "  {} {} [{}]\n",
                m.location,
                m.excerpt,
                if m.executed {
                    "executed"
                } else {
                    "never executed"
                }
            ));
        }
    }
    if let Some(s) = &wn.suggestion {
        out.push_str(&format!("{s}\n"));
    }
    out
}

fn render_site(out: &mut String, s: &Site, depth: usize) {
    let pad = "  ".repeat(depth);
    out.push_str(&format!("{pad}{}  {}\n", s.location, s.excerpt));
    let pad2 = "  ".repeat(depth + 1);
    match &s.status {
        SiteStatus::ExecutedDifferently { observed, note } => {
            match note {
                Some(n) => out.push_str(&format!("{pad2}└─ executed, but {n}\n")),
                None => out.push_str(&format!(
                    "{pad2}└─ executed, but produced something else — evaluated as:\n"
                )),
            }
            for o in observed {
                out.push_str(&format!("{pad2}     {}\n", truncate(o, 90)));
            }
        }
        SiteStatus::GuardFailed { chain } => {
            for g in chain {
                if g.executed {
                    out.push_str(&format!(
                        "{pad2}└─ never executed: guard {} at {} chose another branch\n",
                        truncate(&g.condition, 70),
                        g.location
                    ));
                    for (args, count) in &g.evaluations {
                        out.push_str(&format!(
                            "{pad2}     evaluated {}x as: {}\n",
                            count,
                            truncate(args, 80)
                        ));
                    }
                    for v in &g.values {
                        let val = v.value.as_deref().unwrap_or("<undefined>");
                        let origin = match (&v.origin, v.write_kind.as_deref()) {
                            (Some(o), k) => format!(
                                " (set at {o}{})",
                                k.map(|k| format!(", {k}")).unwrap_or_default()
                            ),
                            (None, Some(k)) => format!(" ({k})"),
                            (None, None) => String::new(),
                        };
                        out.push_str(&format!("{pad2}     {} = \"{}\"{}\n", v.name, val, origin));
                    }
                } else {
                    out.push_str(&format!(
                        "{pad2}└─ inside unexecuted branch of {} at {}\n",
                        truncate(&g.condition, 70),
                        g.location
                    ));
                }
            }
        }
        SiteStatus::InUncalledFunction { name, defined_at } => {
            out.push_str(&format!(
                "{pad2}└─ inside function/macro '{name}' (defined at {defined_at}), \
                 which was never called in this configuration\n"
            ));
        }
        SiteStatus::FileNeverRan { reason, includers } => {
            out.push_str(&format!("{pad2}└─ {reason}\n"));
            if includers.is_empty() {
                out.push_str(&format!(
                    "{pad2}   (no executed or unexecuted site references it)\n"
                ));
            } else {
                out.push_str(&format!("{pad2}   sites that would have pulled it in:\n"));
                for inc in includers {
                    render_site(out, inc, depth + 2);
                }
            }
        }
        SiteStatus::NeverExecuted => {
            out.push_str(&format!(
                "{pad2}└─ never executed (no deciding guard found on its ancestor chain)\n"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_word_boundaries() {
        assert!(mentions_token("app PRIVATE zlib_stub", "zlib_stub"));
        assert!(!mentions_token("app PRIVATE zlib_stubby", "zlib_stub"));
        assert!(!mentions_token("myzlib_stub x", "zlib_stub"));
        assert!(mentions_token("\"zlib_stub\"", "zlib_stub"));
        assert!(mentions_token("${x} zlib_stub)", "zlib_stub"));
        assert!(!mentions_token("ns::zlib_stub", "zlib_stub")); // ':' is a word char
        assert!(mentions_token("zlib_stub", "zlib_stub"));
    }
}
