# cmakedb — Design Plan

A semantic analysis toolkit for CMake, built on execution tracing rather than static interpretation.

---

## 1. Introduction and Motivation

### 1.1 Problem statement

CMake is the de facto build configuration language for C and C++, yet its tooling ecosystem is a generation behind the languages it builds. Existing tools (`cmake-lint`, `cmake-format`, `cmakelint`) operate purely on syntax. None of them can answer the questions that actually consume engineering time on large projects:

- *Why does target A end up linking libz?*
- *Which of our 40 `option()` declarations does nobody read anymore?*
- *Which `PUBLIC` include directories should really be `PRIVATE`?*
- *Where was this variable last written, and by whom?*
- *What would break if we deleted this `.cmake` module?*

These are **semantic** questions. They require knowing what the CMake program *did* when it ran, not what its source text looks like. Today they are answered with `grep`, `--trace-expand` firehoses, and hours of bisection.

### 1.2 Why static analysis alone cannot work

The CMake language resists conventional static analysis:

- **Dynamic scoping.** Variable visibility depends on the runtime call chain (`function` vs `macro`, `PARENT_SCOPE`, directory scope inheritance).
- **Stringly-typed everything.** `${${prefix}_LIBRARIES}` cannot be resolved without knowing runtime values.
- **Configure-time is a program, not a manifest.** Control flow depends on the toolchain, cache state, environment, and platform probes.
- **Generator expressions** defer evaluation past configure time entirely.

A static analyzer would have to reimplement the CMake interpreter and chase upstream language changes forever.

### 1.3 The key insight

CMake already **emits everything needed** to reconstruct semantics:

1. `cmake --trace-expand --trace-format=json-v1` — every command evaluation, with expanded arguments and source location.
2. The **File API** (`.cmake/api/v1`) — the final, fully resolved build graph as JSON.
3. Compiler dependency files (`.d` files / `ninja -t deps`) — which headers each translation unit actually included.

cmakedb's thesis: **correlate the source ASTs with a recorded execution trace and the final build graph, load the join into a queryable database, and build every analysis as a query over that database.** This is the CMake analog of a compilation database plus rr-style post-mortem debugging.

### 1.4 Goals and non-goals

**Goals**

- A persistent, queryable semantic database of one configure run.
- Provenance queries: link/include/flag lineage with source locations at every hop.
- Dead code detection: unread options, uncalled functions, unincluded modules.
- Visibility analysis: `PUBLIC` usage requirements no consumer actually needs.
- Mechanical modernization patches (`include_directories` → `target_include_directories`).
- CI-friendly output (SARIF) and in-editor diagnostics (LSP).

**Non-goals (v1)**

- Analyzing all configurations simultaneously. One trace = one configuration; multi-config analysis is done by diffing databases across presets.
- A live interactive debugger. Post-mortem time-travel queries cover most real needs and are far cheaper to build.
- Reimplementing or forking the CMake interpreter.
- Generator-expression evaluation beyond what the File API already resolves.

---

## 2. User-Facing Interface

### 2.1 CLI

A single binary, `cmakedb`, with subcommands. All analysis subcommands operate on a previously recorded database.

```text
# Record: wraps the real cmake configure, produces .cmakedb/trace.db
cmakedb record --preset default            # uses CMakePresets.json
cmakedb record -- -S . -B build -DFOO=ON   # raw cmake args after --

# Provenance
cmakedb why-links <target> <lib>           # full propagation chain with file:line hops
cmakedb why-includes <target> <dir>        # same for include directories
cmakedb why-flag <target> <flag>           # same for compile options/definitions
cmakedb why-value <variable> [--at file:line]  # write history of a variable

# Dead code
cmakedb dead options                       # option()s never read
cmakedb dead functions                     # functions/macros never invoked
cmakedb dead modules                       # .cmake files never included
cmakedb dead variables                     # set but never read (scoped, noise-filtered)

# Visibility
cmakedb overshare [--deps-from build/]     # PUBLIC reqs unused by consumers,
                                           # cross-checked against compiler dep files

# Overrides & scope bugs
cmakedb clobbers                           # incompatible multi-writes to vars/properties
cmakedb scope-leaks                        # macro writes that escaped intended scope

# Modernization
cmakedb modernize --check                  # report legacy constructs + planned patches
cmakedb modernize --fix                    # apply AST-anchored patches in place

# Raw access
cmakedb query "SELECT ..."                 # direct SQL against the database
cmakedb lint [--format sarif|text|json]    # run all enabled passes
cmakedb diff <db1> <db2>                   # compare two recordings (presets, commits)
```

### 2.2 Example session

```text
$ cmakedb record --preset release
Recorded 48,211 command evaluations across 312 files → .cmakedb/trace.db (14.2 MB)

$ cmakedb why-links app z
app links z (libz.so) via:
  app
  └─ PRIVATE net            src/app/CMakeLists.txt:12
     └─ PUBLIC http         src/net/CMakeLists.txt:33
        └─ INTERFACE ZLIB::ZLIB
                            cmake/FindDeps.cmake:9 (find_package(ZLIB))
Suggestion: http's ZLIB usage is implementation-only in 14/14 consumers.
Consider PRIVATE at src/net/CMakeLists.txt:33. Run `cmakedb overshare` to verify.

$ cmakedb dead options
UNREAD  BUILD_LEGACY_DRIVER   cmake/options.cmake:41   (set, never read)
UNREAD  ENABLE_PROFILING      cmake/options.cmake:57   (read only by dead fn `setup_prof`)
```

### 2.3 Output formats

- **Text** (default): human-readable, colored, with `file:line` locations everywhere.
- **SARIF 2.1.0**: for GitHub code scanning / GitLab / generic CI annotation.
- **JSON**: stable machine-readable schema for scripting, versioned.
- **Patches**: `modernize --fix` emits standard unified diffs (or applies them) anchored to AST nodes, preserving comments and formatting.

### 2.4 LSP server

`cmakedb lsp` runs a language server backed by the most recent recording:

- **Diagnostics**: all lint passes surfaced inline (stale-marked if source changed since recording).
- **Hover**: on a variable → last written value(s) and write location for the enclosing scope; on a target → resolved link/include closure.
- **Go-to-definition**: variables → dominating write site; functions → definition; targets → `add_library`/`add_executable` site.
- **Code actions**: apply individual modernization patches.
- **Custom request** `cmakedb/provenance`: powers a "why?" tree view in editor extensions.

### 2.5 Configuration

`.cmakedb.toml` at the repository root:

```toml
[record]
preset = "default"
capture-scopes = true        # scope snapshots (see §3.2); adds ~10-20% configure time

[lint]
enable = ["dead-options", "overshare", "clobbers", "scope-leaks"]
disable = []
fail-on = "warning"          # CI exit-code threshold

[lint.dead-variables]
ignore-patterns = ["CMAKE_.*", ".*_FOUND"]   # suppress well-known noise

[modernize]
passes = ["include-directories", "link-libraries", "add-definitions"]

[custom-commands]            # teach the parser project wrapper functions
add_project_library = { pargs = 1, kwargs = { SOURCES = "*", DEPS = "*" } }
```

### 2.6 CI integration

```yaml
- run: cmakedb record --preset ci
- run: cmakedb lint --format sarif --output cmakedb.sarif
- uses: github/codeql-action/upload-sarif@v3
  with: { sarif_file: cmakedb.sarif }
```

`cmakedb diff` additionally supports "ratchet" workflows: fail CI only on *new* findings relative to a baseline database committed from the main branch.

---

## 3. Architecture

Four layers, strictly separated. Lower layers know nothing about analyses.

```text
┌────────────────────────────────────────────────────────────┐
│  L4  Analysis passes (plugins)                             │
│      dead-code · provenance · overshare · clobbers ·       │
│      modernize · custom user passes                        │
├────────────────────────────────────────────────────────────┤
│  L3  Semantic database (SQLite)                            │
│      joined view of AST × trace × File API × dep files     │
├──────────────────────────┬─────────────────────────────────┤
│  L1  Syntax layer        │  L2  Execution recorder         │
│  lossless ASTs of all    │  real `cmake` run under         │
│  .cmake / CMakeLists.txt │  json-v1 trace + scope snapshots│
│  (tree-sitter-cmake)     │  + File API query + dep files   │
└──────────────────────────┴─────────────────────────────────┘
```

### 3.1 L1 — Syntax layer

- Parses every `.cmake` and `CMakeLists.txt` reachable from the source tree into a **lossless** AST: comments, whitespace, and bracket/quote style preserved, so codemods can round-trip byte-identically outside edited regions.
- Built on **tree-sitter-cmake** (error-tolerant, incremental — required for the LSP) with a thin typed layer on top that models CMake's real grammar: command invocations, positional/keyword arguments, variable references inside strings, generator-expression spans (kept opaque), block structures (`if`/`foreach`/`function`/`macro`).
- Every AST node gets a **position-stable node ID**: `(file content hash, byte offset, node kind)`. Trace events are joined to nodes by `(file, line)` plus argument-text disambiguation when multiple commands share a line.
- Custom command signatures from `.cmakedb.toml` (§2.5) refine keyword-argument parsing for project wrapper functions, exactly as cmakelang does — without this, wrapper-heavy projects produce misparsed argument structure.

### 3.2 L2 — Execution recorder

Runs the **real** CMake binary; never reimplements it. Four capture channels:

1. **JSON trace**: `cmake --trace-expand --trace-format=json-v1 --trace-redirect=trace.jsonl`. Each line: file, line, command name, expanded args, elapsed time, and (v2 of the format) global frame/scope counters. This is ground truth for *what executed with what values* — including everything static analysis cannot see (values from `execute_process`, platform probes, cache).
2. **Scope snapshots**: a small injected CMake script (via `CMAKE_PROJECT_INCLUDE_BEFORE` + `variable_watch` on selected variables + a `cmake_language(DEFER)` hook per directory) that dumps directory-scope variable tables at scope entry/exit to a sidecar file. This is what lets `why-value` distinguish *scopes*, since the raw trace shows evaluations but not scope-table state. Optional (`capture-scopes`), ~10–20% configure overhead.
3. **File API**: the recorder writes a query for `codemodel-v2`, `cache-v2`, and `toolchains-v1` before configuring, then ingests the reply. This yields the *final resolved* target graph: link closures, include directories, compile definitions per target per config — with generator expressions already evaluated by CMake itself.
4. **Compiler dep files** (optional, post-build): `ninja -t deps` or `.d` files, ingested by `overshare` to know which headers each TU actually included.

The recorder also captures the environment, cmake version, toolchain file hash, and cache state, so a database is reproducible and diffable.

**Honest limitation, by design**: one recording = one configuration (one platform, one preset, one set of cache values). Branches not taken are invisible. The intended workflow is per-preset recording in CI plus `cmakedb diff`.

### 3.3 L3 — Semantic database

A single SQLite file. Ingestion joins the three sources:

- Trace events → AST node IDs (via file:line + argument matching).
- Trace-level target mutations (`target_link_libraries` calls, property writes) → File API final targets (via target name + backtrace, which the File API also provides per graph edge).
- Dep-file entries → File API translation units.

SQLite is deliberate: zero-dependency single-file artifact, committable as a CI baseline, directly queryable by users (`cmakedb query`), fast enough at this scale (a very large project produces low millions of trace events — trivial for SQLite with proper indexes). Schema in §4.1.

### 3.4 L4 — Analysis passes

Each pass is a plugin implementing:

```rust
trait Pass {
    fn id(&self) -> &str;                      // "dead-options"
    fn run(&self, db: &Db, cfg: &PassConfig) -> Vec<Finding>;
}

struct Finding {
    rule: String,
    severity: Severity,
    message: String,
    primary: SourceSpan,                        // file:line:col range
    related: Vec<(SourceSpan, String)>,         // provenance hops, evidence
    fix: Option<Patch>,                         // AST-anchored edit
}
```

Passes are pure functions over the database — no filesystem or process access — which makes them trivially testable and parallelizable. Built-in passes ship in-tree; user passes load as WASM plugins (same trait over a WIT interface) or, pragmatically in v1, as SQL files with a small result-shape convention.

`Finding` maps 1:1 onto SARIF `result` objects; `related` becomes SARIF `relatedLocations`, giving provenance chains natively in code-scanning UIs.

---

## 4. Algorithms and Data Model

### 4.1 Core schema (abridged)

```sql
-- L1
CREATE TABLE files      (id INTEGER PK, path TEXT, content_hash TEXT);
CREATE TABLE ast_nodes  (id INTEGER PK, file_id INT, kind TEXT,
                         byte_start INT, byte_end INT,
                         line INT, col INT, parent_id INT);
CREATE TABLE commands   (node_id INT PK,          -- command invocation nodes
                         name TEXT, name_lower TEXT);

-- L2: one row per executed command evaluation
CREATE TABLE events     (id INTEGER PK,           -- monotonic = execution order
                         node_id INT,             -- joined AST node (nullable)
                         file_id INT, line INT,
                         cmd TEXT, args_json TEXT, -- expanded arguments
                         scope_id INT, elapsed_us INT);

-- Scope tree reconstructed from trace + snapshots
CREATE TABLE scopes     (id INTEGER PK, parent_id INT,
                         kind TEXT,               -- directory|function|macro|block
                         opened_by_event INT, closed_by_event INT);

-- Derived: variable dataflow
CREATE TABLE var_writes (id INTEGER PK, event_id INT, scope_id INT,
                         name TEXT, value TEXT,
                         write_kind TEXT);        -- set|cache|env|parent_scope|unset
CREATE TABLE var_reads  (id INTEGER PK, event_id INT, scope_id INT,
                         name TEXT, resolved_write_id INT); -- dominating write

-- Final graph from File API
CREATE TABLE targets    (id INTEGER PK, name TEXT, type TEXT,
                         defined_event INT);
CREATE TABLE tgt_edges  (id INTEGER PK, src_target INT, dst TEXT,
                         dst_target INT,          -- null if external lib
                         visibility TEXT,         -- PUBLIC|PRIVATE|INTERFACE
                         origin_event INT);       -- the target_link_libraries call
CREATE TABLE tgt_props  (target_id INT, prop TEXT, value TEXT,
                         origin_event INT);       -- one row per mutation, ordered
CREATE TABLE usage_reqs (target_id INT, kind TEXT, -- include|define|option|feature
                         value TEXT, visibility TEXT, origin_event INT);

-- Optional: build reality
CREATE TABLE tus        (id INTEGER PK, target_id INT, source_path TEXT);
CREATE TABLE tu_headers (tu_id INT, header_path TEXT);

CREATE INDEX ix_events_loc   ON events(file_id, line);
CREATE INDEX ix_writes_name  ON var_writes(name, scope_id);
CREATE INDEX ix_edges_src    ON tgt_edges(src_target);
```

### 4.2 Variable dataflow (`why-value`, `dead variables`, `clobbers`)

**Read→write resolution.** For each `var_reads` row, the *dominating write* is resolved with CMake's actual lookup rule replayed over recorded data: walk the scope chain from the reading scope upward (function scopes copy-on-entry from the caller's dynamic chain — this is why scopes are keyed by the runtime scope tree, not the lexical file tree); within each scope take the latest `var_writes` row with `event_id` < the read's `event_id`; fall back to cache writes. `PARENT_SCOPE` writes are recorded against the parent scope but timestamped at the writing event. Scope snapshots (§3.2) validate this replay: at every snapshot boundary the replayed table must equal the dumped table, and mismatches fail ingestion loudly rather than producing silently wrong provenance.

- **`why-value X`**: all writes that some read of `X` resolved to, in execution order, each with source location, value, scope kind, and the call chain of the enclosing scope. A time-travel debugger rendered as a table.
- **`dead variables`**: writes with no resolving read, filtered by scope liveness (a `PARENT_SCOPE` write is only dead if unread in the parent too) and by ignore patterns, since many `_FOUND`-style variables exist for other tools to read.
- **`clobbers`**: two writes in the same scope where the second's resolving reads never observed the first, and values differ — flagging both locations. Restricted to non-loop contexts (a `foreach` rewriting its variable is not a clobber; loop membership comes from the AST).
- **`scope-leaks`**: writes executed inside a *macro* body (macros don't create scopes) targeting names also written in the caller's scope — the classic macro-vs-function bug.

### 4.3 Provenance walk (`why-links`, `why-includes`, `why-flag`)

The final link closure comes from the File API; the *explanation* comes from joining each closure member back to `tgt_edges`/`usage_reqs` origin events:

```text
explain(target T, dependency D):
  paths = all simple paths T →* D over tgt_edges where interface
          propagation rules admit transitivity
          (edge from X to Y contributes to X's consumers iff
           visibility ∈ {PUBLIC, INTERFACE})
  for each path: annotate every edge with origin_event → file:line
  rank paths: shortest first; collapse shared prefixes into a tree
```

Because `tgt_edges` stores the *originating call event*, every hop in the output has a real source location — including hops introduced inside third-party `Find*.cmake` modules, which is where such chains usually vanish today. The same walk over `usage_reqs` implements `why-includes` and `why-flag`; for flags, `tgt_props` mutation order additionally reveals *overridden* values (a `clobbers` special case at the property level).

### 4.4 Overshare analysis (`overshare`)

For each `PUBLIC`/`INTERFACE` include directory `I` on target `T`:

1. Compute consumers `C(T)` from the reverse transitive closure over propagation-admitting edges.
2. From `tu_headers`, check whether any TU of any consumer includes any header physically under `I` (path-prefix match after normalization/symlink resolution).
3. If no consumer TU uses it but `T`'s own TUs do → suggest `PRIVATE`. If nobody uses it at all → suggest removal.

Same skeleton for `PUBLIC` link edges, using a weaker but useful signal: a consumer needs `D` transitively iff some consumer TU includes headers from `D`'s interface include dirs, or `D` appears in the consumer's own explicit requirements. Findings below full confidence are reported as suggestions with the evidence attached rather than as errors — the pass is explicitly *advisory* because header-based need is a proxy (ODR/link-time needs can exist without includes). This pass requires a completed build (dep files); `cmakedb` states that in the finding metadata.

### 4.5 Dead code

- **`dead options`**: `option()`/`set(... CACHE ...)` events whose variable has no resolving read anywhere in the trace, *and* is not referenced by presets/toolchain files (checked textually as a guard). Reads that occur only inside otherwise-dead functions are reported with that qualifier (see example in §2.2).
- **`dead functions`**: `function`/`macro` definition events with zero invocation events. Trace-based, so wrapper indirection and `cmake_language(CALL)` are handled for free — if it ran, it's live. Caveat surfaced in output: "not called *in this configuration*."
- **`dead modules`**: files present under configured source roots with zero events, minus modules matched by `install()` arguments or preset references (they may be consumed downstream).

### 4.6 Modernization codemods (`modernize`)

Example, the flagship pass — directory-scope `include_directories()` → target-scoped calls:

1. Find each `include_directories(ARGS)` event `E` in directory scope `S`.
2. Affected set = targets whose `defined_event` occurs after `E` within `S` or its child directory scopes (replaying CMake's actual inheritance semantics over recorded scope/event order).
3. For each affected target, check the File API: does the directory-level include actually appear in its final include list (it can be shadowed)? Keep only real effects.
4. Emit: delete the `include_directories` AST node; insert `target_include_directories(<tgt> PRIVATE ARGS)` after each affected target's defining command node. Initial visibility is `PRIVATE` unless the `overshare` machinery proves consumers use it, in which case `PUBLIC` — i.e., the codemod composes with §4.4 rather than guessing.
5. Patches are anchored to node IDs, not line numbers, and applied through the lossless AST so untouched bytes are untouched.
6. **Verification loop**: after applying, `cmakedb` re-records and asserts the File API build graph is isomorphic (same targets, same resolved requirement sets). A codemod that changes the graph is auto-reverted and reported. This property — *mechanically verified behavior preservation* — is the feature that makes `--fix` trustworthy and is only possible because recording is cheap.

Same pattern for `link_libraries`, `add_definitions`, `add_compile_options`, and policy-bump assistance (flag constructs whose behavior changes under the new policy, using trace evidence of whether the changed behavior is exercised).

### 4.7 Diffing

`cmakedb diff A.db B.db` aligns targets by name, events by (file, AST-node), variables by (name, scope path), and reports: added/removed targets and edges, changed final requirement sets, findings introduced/resolved. Alignment by AST node (content-hash based) keeps diffs stable across unrelated edits.

---

## 5. Implementation Plan

### 5.1 Language and stack

**Rust** for the core. Rationale: single static binary (critical for CI adoption — no Python environment drift), first-class tree-sitter bindings, excellent SQLite (`rusqlite`) and LSP (`tower-lsp`) ecosystems, performance headroom for million-event ingestion, and WASM plugin hosting (`wasmtime`) later.

| Component | Choice |
|---|---|
| Parser | `tree-sitter-cmake` + typed wrapper crate (own the grammar fork if upstream gaps appear) |
| Storage | SQLite via `rusqlite`; schema migrations via `refinery` |
| Trace ingestion | `serde_json` streaming (JSONL), single pass |
| CLI | `clap`; human output via `annotate-snippets` for rustc-style spans |
| SARIF | `serde-sarif` |
| LSP | `tower-lsp` |
| Patching | own lossless-printing layer over the AST (the one piece with no off-the-shelf answer) |
| Plugins v1 | SQL-file passes; v2: WASM via `wasmtime` + WIT interface |

Prototyping note: analysis passes are SQL-heavy, so new passes can be prototyped in a notebook against the `.db` file with zero Rust — this is an explicit design payoff of the SQLite choice, and doubles as the user extension story.

### 5.2 Crate layout

```text
cmakedb/
  crates/
    cmakedb-syntax/      # L1: parsing, node IDs, lossless printing
    cmakedb-record/      # L2: cmake wrapper, trace/scope/file-api capture
    cmakedb-db/          # L3: schema, ingestion, query helpers
    cmakedb-passes/      # L4: built-in passes
    cmakedb-patch/       # AST-anchored edits, verification loop
    cmakedb-lsp/         # language server
    cmakedb-cli/         # binary
  fixtures/              # test projects (see §6)
```

### 5.3 Milestones

**M1 — Recorder + DB (weeks 1–6).** `record` works on real projects (validate against LLVM and a large proprietary-style fixture); ingestion of trace + File API; scope replay with snapshot validation; `cmakedb query`.

**M2 — First value (weeks 7–12).** `why-links`, `why-value`, `dead options`. Text + SARIF output. This is the MVP that answers the two highest-pain questions; ship it and gather feedback.

**M3 — Breadth (weeks 13–20).** `clobbers`, `scope-leaks`, `dead functions/modules/variables`, `overshare` (dep-file ingestion), `diff`, ratchet workflow.

**M4 — Modernize + LSP (weeks 21–30).** Lossless printing, patch engine, verification loop, `include_directories` codemod first, then the rest; LSP with hover-provenance and diagnostics.

**M5 — Extensibility (after).** WASM pass API, editor extension with the provenance tree view.

> **Design revision (pre-1.0 release):** the WASM pass API is deferred, so M5 ships as SQL-file passes plus the editor extension. An ABI v0 host was built on `wasmtime` and then removed: it added 81 crates that existed for nothing else (~a third of the dependency tree, including a Cranelift JIT) and carried 16 open RUSTSEC advisories — several sandbox escapes, one on a published release target — in exchange for a capability that was SQL-plus-computation. The §3.4 plugin story is unchanged in intent: v1 is SQL files, and WASM returns at v2 against the WIT/component-model interface, which was always the destination. Rationale recorded here rather than treated as drift; status in `ROADMAP.md`.

### 5.4 Compatibility policy

- Supported CMake: ≥ 3.17 (File API v2 + `json-v1` trace both stable there); scope snapshots need ≥ 3.19 (`cmake_language(DEFER)`) and degrade gracefully below.
- The trace and File API are versioned, documented CMake interfaces — this is the load-bearing reason the architecture doesn't chase interpreter internals. CI runs the full suite against oldest-supported, latest-release, and CMake master to catch drift early.

---

## 6. Testing Strategy

### 6.1 Fixture projects (the backbone)

A corpus in `fixtures/`, each a tiny CMake project encoding one semantic phenomenon with a **committed expectation file**:

- scope semantics: function vs macro, `PARENT_SCOPE`, directory inheritance, `block()`
- propagation: PRIVATE/PUBLIC/INTERFACE chains, diamond deps, `$<LINK_ONLY:>`
- legacy constructs for each codemod
- pathological parsing: bracket args, line continuations, multiple commands per line, generator expressions in every position

Test = record fixture → run pass → compare findings (locations, messages, fix patches) against golden files. Golden updates are reviewed diffs.

### 6.2 Replay validation as a standing test

The scope-replay-vs-snapshot cross-check (§4.2) runs in every recording, in production, not just tests. Any divergence between replayed variable tables and CMake's actual dumped tables is a hard error with a minimized repro dumped to disk. This converts users' real projects into a continuous conformance suite for the trickiest component.

### 6.3 Codemod verification tests

For every codemod fixture: apply `--fix`, re-record, assert build-graph isomorphism (the same check the production verification loop performs), and additionally assert byte-identical output outside edited spans (lossless-printing guarantee). Property test: applying a patch then its inverse round-trips the file exactly.

### 6.4 Differential and soak testing

- **Real-world corpus in CI**: record + full lint over pinned checkouts of LLVM, OpenCV, Qt Base, and 2–3 wrapper-function-heavy projects. Assertions: no crashes, ingestion invariants hold (every event joins to a node or is explainably synthetic), finding counts within tolerance of baseline. Runtime budget tracked per project.
- **CMake-version matrix** (§5.4) doubles as drift detection for the trace format.

### 6.5 Fuzzing and unit layers

- Fuzz the parser (`cargo-fuzz`) with grammar-aware mutation of fixture files; invariant: never panic, lossless reprint of whatever parsed.
- Fuzz trace ingestion with malformed/truncated JSONL; invariant: clean error, no partial-commit database.
- Standard unit tests for the query helpers, path canonicalization (symlinks, case-insensitive filesystems — a real source of `overshare` false positives), and SARIF serialization against the schema.

### 6.6 Performance gates

Benchmarks in CI (`criterion` + the LLVM fixture): ingestion ≥ 50k events/sec, `why-links` < 100 ms warm, full lint over LLVM < 60 s excluding the configure itself. Regressions beyond 15% fail the build.

---

## 7. Risks and Mitigations

| Risk | Mitigation |
|---|---|
| Trace format changes upstream | Versioned ingestion adapters; CMake-master CI leg; formats are documented/versioned interfaces |
| Scope replay diverges from real CMake semantics | Snapshot cross-validation in production (§6.2); fail loud, never guess |
| Single-configuration blindness misleads users | Every finding is labeled with its recording's preset; `diff` + per-preset CI is the documented workflow |
| `overshare` false positives (headers ≠ full need) | Advisory severity, evidence attached, ratchet-friendly |
| Wrapper-heavy projects parse poorly | `custom-commands` config; wrapper-heavy fixtures in the corpus |
| Codemod breaks a build in a way graph isomorphism misses | Isomorphism covers requirement sets, not just topology; `--fix` is opt-in per pass; auto-revert on verification failure |

---

## 8. Summary

cmakedb treats a CMake configure run as a recorded program execution, joins that recording with lossless source ASTs and CMake's own resolved build graph, and stores the result as a SQLite database that both ships with a set of high-value analyses (provenance, dead code, visibility, verified codemods) and is directly queryable for everything nobody anticipated. By building exclusively on CMake's emitted, versioned artifacts — never reimplementing the language — it stays correct by construction on exactly the dynamic behavior that has made every static approach fail.
