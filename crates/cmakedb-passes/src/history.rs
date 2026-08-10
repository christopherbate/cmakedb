//! `why-value`: the write history of a variable — a time-travel debugger
//! rendered as a table (design §4.2).

use anyhow::Result;
use cmakedb_db::Db;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct WriteRecord {
    pub event_id: i64,
    pub location: String,
    pub value: Option<String>,
    pub write_kind: String,
    pub scope_kind: String,
    /// Call chain of the enclosing scope, innermost first
    /// (`my_fn <- CMakeLists.txt:12 <- ...`).
    pub call_chain: Vec<String>,
    /// Number of reads that resolved to this write.
    pub resolved_reads: i64,
}

pub fn why_value(db: &Db, name: &str, at: Option<(&str, i64)>) -> Result<Vec<WriteRecord>> {
    // With --at file:line, restrict to writes that the read(s) at that
    // location resolved to; otherwise the full history.
    let mut records = Vec::new();
    let sql = if at.is_some() {
        "SELECT w.id, w.event_id, w.value, w.write_kind,
                coalesce(s.kind, 'cache'), s.id, s.name, s.opened_by_event
         FROM var_writes w
         LEFT JOIN scopes s ON s.id = w.scope_id
         WHERE w.name = ?1 AND w.id IN (
            SELECT r.resolved_write_id FROM var_reads r
            JOIN events e ON e.id = r.event_id
            JOIN files f ON f.id = e.file_id
            WHERE r.name = ?1 AND f.path LIKE '%' || ?2 AND e.line = ?3)
         ORDER BY w.id"
    } else {
        "SELECT w.id, w.event_id, w.value, w.write_kind,
                coalesce(s.kind, 'cache'), s.id, s.name, s.opened_by_event
         FROM var_writes w
         LEFT JOIN scopes s ON s.id = w.scope_id
         WHERE w.name = ?1
         ORDER BY w.id"
    };
    let mut stmt = db.conn.prepare(sql)?;
    let map = |r: &rusqlite::Row| -> rusqlite::Result<_> {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<i64>>(7)?,
        ))
    };
    let rows: Vec<_> = match at {
        Some((file, line)) => stmt
            .query_map(rusqlite::params![name, file, line], map)?
            .collect::<rusqlite::Result<_>>()?,
        None => stmt
            .query_map(rusqlite::params![name], map)?
            .collect::<rusqlite::Result<_>>()?,
    };

    for (wid, event_id, value, write_kind, scope_kind, scope_id, _sname, _opened) in rows {
        let location = db
            .event_location(event_id)
            .unwrap_or_else(|_| "<synthetic>".into());
        let resolved_reads: i64 = db.conn.query_row(
            "SELECT count(*) FROM var_reads WHERE resolved_write_id = ?1",
            [wid],
            |r| r.get(0),
        )?;
        records.push(WriteRecord {
            event_id,
            location,
            value,
            write_kind,
            scope_kind,
            call_chain: call_chain(db, scope_id)?,
            resolved_reads,
        });
    }
    Ok(records)
}

/// Walk scope parents, describing each hop by its kind/name and the call
/// site that opened it.
fn call_chain(db: &Db, mut scope_id: Option<i64>) -> Result<Vec<String>> {
    let mut chain = Vec::new();
    for _ in 0..32 {
        let Some(sid) = scope_id else { break };
        let row: Option<(String, Option<String>, Option<i64>, Option<i64>)> = db
            .conn
            .query_row(
                "SELECT kind, name, opened_by_event, parent_id FROM scopes WHERE id = ?1",
                [sid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();
        let Some((kind, name, opened_by, parent)) = row else {
            break;
        };
        if kind == "root" {
            break;
        }
        let site = opened_by
            .and_then(|e| db.event_location(e).ok())
            .unwrap_or_default();
        let label = match name {
            Some(n) if !n.is_empty() => format!("{kind} {n} ({site})"),
            _ => format!("{kind} ({site})"),
        };
        chain.push(label);
        scope_id = parent;
    }
    Ok(chain)
}
