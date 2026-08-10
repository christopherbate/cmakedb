//! Core schema, design §4.1 (extended with provenance columns discovered
//! necessary during implementation: trace-vs-fileapi source tags, alias
//! resolution, function definitions).

pub const SCHEMA_VERSION: i64 = 3;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

-- L1 ---------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS files (
    id           INTEGER PRIMARY KEY,
    path         TEXT NOT NULL UNIQUE,
    content_hash TEXT,             -- null if the file was not parseable/readable
    in_source    INTEGER NOT NULL DEFAULT 0,  -- under the recorded source root
    has_errors   INTEGER NOT NULL DEFAULT 0,
    content      TEXT              -- recorded source text (project files only);
                                   -- lets patch planning stay a pure function
                                   -- over the db (§3.4) with byte offsets
                                   -- valid against exactly this content
);

CREATE TABLE IF NOT EXISTS ast_nodes (
    id         INTEGER PRIMARY KEY,
    file_id    INTEGER NOT NULL REFERENCES files(id),
    kind       TEXT NOT NULL,
    byte_start INTEGER NOT NULL,
    byte_end   INTEGER NOT NULL,
    line       INTEGER NOT NULL,   -- 1-based
    col        INTEGER NOT NULL,   -- 1-based
    parent_id  INTEGER
);

CREATE TABLE IF NOT EXISTS commands (
    node_id    INTEGER PRIMARY KEY REFERENCES ast_nodes(id),
    name       TEXT NOT NULL,
    name_lower TEXT NOT NULL,
    args_text  TEXT NOT NULL       -- raw (unexpanded) argument text
);

-- Static variable references found in a command's raw argument text.
CREATE TABLE IF NOT EXISTS ast_var_refs (
    node_id INTEGER NOT NULL REFERENCES ast_nodes(id),
    name    TEXT NOT NULL
);

-- L2 ---------------------------------------------------------------------
-- One row per executed command evaluation; id is execution order.
CREATE TABLE IF NOT EXISTS events (
    id           INTEGER PRIMARY KEY,
    node_id      INTEGER REFERENCES ast_nodes(id),   -- joined AST node (nullable)
    file_id      INTEGER NOT NULL REFERENCES files(id),
    line         INTEGER NOT NULL,
    cmd          TEXT NOT NULL,
    cmd_lower    TEXT NOT NULL,
    args_json    TEXT NOT NULL,     -- expanded arguments, JSON array
    scope_id     INTEGER REFERENCES scopes(id),
    frame        INTEGER,
    global_frame INTEGER,
    time_abs     REAL,
    elapsed_us   INTEGER
);

-- Runtime scope tree reconstructed from trace frames (+ block tracking).
CREATE TABLE IF NOT EXISTS scopes (
    id              INTEGER PRIMARY KEY,
    parent_id       INTEGER REFERENCES scopes(id),
    kind            TEXT NOT NULL,   -- root|directory|function|macro|include|block|unknown
    name            TEXT,            -- function/macro name, directory path, included file
    transparent     INTEGER NOT NULL DEFAULT 0, -- no own variable table
    opened_by_event INTEGER,
    closed_by_event INTEGER
);

-- Derived: variable dataflow ---------------------------------------------
CREATE TABLE IF NOT EXISTS var_writes (
    id         INTEGER PRIMARY KEY,
    event_id   INTEGER NOT NULL REFERENCES events(id),
    scope_id   INTEGER REFERENCES scopes(id),  -- effective (opaque) scope
    name       TEXT NOT NULL,
    value      TEXT,                -- null when the trace doesn't show the result
    write_kind TEXT NOT NULL        -- set|cache|env|parent_scope|unset|synthetic
);

CREATE TABLE IF NOT EXISTS var_reads (
    id                INTEGER PRIMARY KEY,
    event_id          INTEGER NOT NULL REFERENCES events(id),
    scope_id          INTEGER REFERENCES scopes(id),
    name              TEXT NOT NULL,
    resolved_write_id INTEGER REFERENCES var_writes(id),  -- dominating write; null = undefined
    read_kind         TEXT NOT NULL DEFAULT 'expand'      -- expand|condition|listarg|env
);

-- Function/macro definitions observed in the trace.
CREATE TABLE IF NOT EXISTS func_defs (
    id       INTEGER PRIMARY KEY,
    event_id INTEGER NOT NULL REFERENCES events(id),
    name     TEXT NOT NULL,
    name_lower TEXT NOT NULL,
    kind     TEXT NOT NULL,          -- function|macro
    file_id  INTEGER NOT NULL REFERENCES files(id),
    line     INTEGER NOT NULL,
    params_json TEXT NOT NULL        -- declared parameter names
);

-- Final graph (File API) joined with trace origins -----------------------
CREATE TABLE IF NOT EXISTS targets (
    id            INTEGER PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    type          TEXT,              -- EXECUTABLE|STATIC_LIBRARY|...|IMPORTED|INTERFACE_LIBRARY
    imported      INTEGER NOT NULL DEFAULT 0,
    alias_of      TEXT,              -- if this is an ALIAS target
    defined_event INTEGER REFERENCES events(id),
    in_file_api   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS tgt_edges (
    id           INTEGER PRIMARY KEY,
    src_target   INTEGER NOT NULL REFERENCES targets(id),
    dst          TEXT NOT NULL,      -- as written (alias-resolved name or external lib)
    dst_target   INTEGER REFERENCES targets(id),  -- null if external
    visibility   TEXT NOT NULL,      -- PUBLIC|PRIVATE|INTERFACE
    origin_event INTEGER REFERENCES events(id)
);

-- One row per property mutation, in execution order.
CREATE TABLE IF NOT EXISTS tgt_props (
    id           INTEGER PRIMARY KEY,
    target_id    INTEGER NOT NULL REFERENCES targets(id),
    prop         TEXT NOT NULL,
    value        TEXT,
    appended     INTEGER NOT NULL DEFAULT 0,
    origin_event INTEGER REFERENCES events(id)
);

-- Usage requirements: from trace (as written, with visibility) and from
-- the File API (final resolved, per compile group).
CREATE TABLE IF NOT EXISTS usage_reqs (
    id           INTEGER PRIMARY KEY,
    target_id    INTEGER NOT NULL REFERENCES targets(id),
    kind         TEXT NOT NULL,      -- include|define|option|feature|link
    value        TEXT NOT NULL,
    visibility   TEXT,               -- null for resolved File API rows
    source       TEXT NOT NULL,      -- trace|fileapi
    origin_event INTEGER REFERENCES events(id)
);

-- Optional: build reality (dep files) ------------------------------------
CREATE TABLE IF NOT EXISTS tus (
    id          INTEGER PRIMARY KEY,
    target_id   INTEGER NOT NULL REFERENCES targets(id),
    source_path TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tu_headers (
    tu_id       INTEGER NOT NULL REFERENCES tus(id),
    header_path TEXT NOT NULL
);

-- CMake's own configure-time diagnostics, parsed from captured stderr
-- (schema v3). file/line are as cmake printed them ('' / 0 when the
-- block carried no location).
CREATE TABLE IF NOT EXISTS configure_diagnostics (
    id       INTEGER PRIMARY KEY,
    severity TEXT,                  -- error|warning|note
    kind     TEXT,                  -- parenthesized tag (dev, ...), 'deprecation', or ''
    file     TEXT,
    line     INTEGER,
    message  TEXT
);

"#;

/// Indexes are created after bulk ingestion (incremental index maintenance
/// roughly halves ingest throughput at LLVM scale, §6.6).
pub const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS ix_events_loc    ON events(file_id, line);
CREATE INDEX IF NOT EXISTS ix_events_cmd    ON events(cmd_lower);
CREATE INDEX IF NOT EXISTS ix_nodes_file    ON ast_nodes(file_id, line);
CREATE INDEX IF NOT EXISTS ix_writes_name   ON var_writes(name, scope_id);
CREATE INDEX IF NOT EXISTS ix_reads_name    ON var_reads(name);
CREATE INDEX IF NOT EXISTS ix_reads_write   ON var_reads(resolved_write_id);
CREATE INDEX IF NOT EXISTS ix_edges_src     ON tgt_edges(src_target);
CREATE INDEX IF NOT EXISTS ix_edges_dst     ON tgt_edges(dst_target);
CREATE INDEX IF NOT EXISTS ix_reqs_target   ON usage_reqs(target_id, kind);
CREATE INDEX IF NOT EXISTS ix_varrefs_node  ON ast_var_refs(node_id);
CREATE INDEX IF NOT EXISTS ix_varrefs_name  ON ast_var_refs(name);
CREATE INDEX IF NOT EXISTS ix_props_target  ON tgt_props(target_id, prop);
"#;
