//! Overshare analysis (design §4.4): PUBLIC/INTERFACE usage requirements
//! that no consumer actually needs, cross-checked against compiler dep
//! files. Advisory by design — header need is a proxy.

use anyhow::Result;
use cmakedb_db::Db;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::{event_span, Finding, Pass, PassConfig, Severity, SourceSpan};

pub struct Overshare;

impl Pass for Overshare {
    fn id(&self) -> &'static str {
        "overshare"
    }
    fn description(&self) -> &'static str {
        "PUBLIC/INTERFACE requirements unused by any consumer (needs dep files)"
    }

    fn run(&self, db: &Db, _cfg: &PassConfig) -> Result<Vec<Finding>> {
        let have_deps: i64 = db
            .conn
            .query_row("SELECT count(*) FROM tu_headers", [], |r| r.get(0))?;
        if have_deps == 0 {
            return Ok(vec![Finding {
                rule: self.id().into(),
                severity: Severity::Note,
                message: "overshare requires compiler dep files: build the tree, then run \
                          `cmakedb overshare --deps-from <build-dir>` to ingest `ninja -t deps`"
                    .into(),
                primary: SourceSpan::new("<recording>", 0),
                related: vec![],
                fix: None,
            }]);
        }

        // Forward propagating edges: consumer -> dependency.
        let mut edges: Vec<(i64, Option<i64>, String, Option<i64>)> = Vec::new();
        {
            let mut stmt = db.conn.prepare(
                "SELECT src_target, dst_target, visibility, origin_event
                 FROM tgt_edges WHERE dst_target IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            for r in rows {
                edges.push(r?);
            }
        }
        // Reverse adjacency: dependency -> (consumer, edge visibility).
        let mut consumers_of: HashMap<i64, Vec<(i64, bool)>> = HashMap::new();
        for (src, dst, vis, _) in &edges {
            let Some(dst) = *dst else { continue };
            let propagating = vis == "PUBLIC" || vis == "INTERFACE";
            consumers_of
                .entry(dst)
                .or_default()
                .push((*src, propagating));
        }

        // Headers included by each target's TUs.
        let mut target_headers: HashMap<i64, HashSet<String>> = HashMap::new();
        {
            let mut stmt = db.conn.prepare(
                "SELECT t.target_id, h.header_path FROM tu_headers h
                 JOIN tus t ON t.id = h.tu_id",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for r in rows {
                let (tid, h) = r?;
                target_headers.entry(tid).or_default().insert(h);
            }
        }

        // PUBLIC/INTERFACE include requirements as written (trace rows carry
        // visibility + origin).
        let mut stmt = db.conn.prepare(
            "SELECT r.target_id, t.name, r.value, r.visibility, r.origin_event
             FROM usage_reqs r JOIN targets t ON t.id = r.target_id
             WHERE r.kind = 'include' AND r.source = 'trace'
               AND r.visibility IN ('PUBLIC', 'INTERFACE')
             ORDER BY r.id",
        )?;
        let reqs: Vec<(i64, String, String, String, Option<i64>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut findings = Vec::new();
        for (tid, tname, dir, vis, origin) in reqs {
            // Consumers via propagation-admitting reverse closure (§4.4):
            // X consuming T sees T's interface regardless of X's edge
            // visibility, but X *re-exports* it to its own consumers only
            // when X links (toward T) with PUBLIC/INTERFACE at every hop.
            let mut consumers: HashSet<i64> = HashSet::new();
            let mut reexporters: HashSet<i64> = HashSet::new();
            let mut queue: VecDeque<i64> = VecDeque::new();
            reexporters.insert(tid);
            queue.push_back(tid);
            while let Some(x) = queue.pop_front() {
                for (c, propagating) in consumers_of.get(&x).into_iter().flatten() {
                    consumers.insert(*c);
                    if *propagating && reexporters.insert(*c) {
                        queue.push_back(*c);
                    }
                }
            }

            let dir_norm = normalize(&dir);
            let used_by_consumer = consumers.iter().any(|c| {
                target_headers
                    .get(c)
                    .map(|hs| hs.iter().any(|h| normalize(h).starts_with(&dir_norm)))
                    .unwrap_or(false)
            });
            if used_by_consumer {
                continue;
            }
            let used_by_self = target_headers
                .get(&tid)
                .map(|hs| hs.iter().any(|h| normalize(h).starts_with(&dir_norm)))
                .unwrap_or(false);

            let primary = origin
                .map(|e| event_span(db, e))
                .unwrap_or_else(|| SourceSpan::new("<unknown>", 0));
            let (msg, evidence) = if used_by_self {
                (
                    format!(
                        "{tname}: {vis} include dir '{dir}' is used only by {tname}'s own \
                         sources in {} consumer(s) checked — consider PRIVATE",
                        consumers.len()
                    ),
                    "no consumer TU includes headers under this directory",
                )
            } else if consumers.is_empty() && vis == "INTERFACE" {
                // An interface lib with no consumers is a different smell;
                // skip to avoid noise.
                continue;
            } else {
                (
                    format!(
                        "{tname}: {vis} include dir '{dir}' is not used by {tname} or any \
                         of its {} consumer(s) — consider removing",
                        consumers.len()
                    ),
                    "no TU of the target or its consumers includes headers under it",
                )
            };
            findings.push(Finding {
                rule: self.id().into(),
                severity: Severity::Note, // advisory by design (§4.4)
                message: msg,
                primary,
                related: vec![(SourceSpan::new("<evidence>", 0), evidence.into())],
                fix: None,
            });
        }
        Ok(findings)
    }
}

fn normalize(p: &str) -> String {
    let mut s = p.replace('\\', "/");
    if !s.ends_with('/') {
        s.push('/');
    }
    s
}
