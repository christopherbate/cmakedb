# AGENTS.md

Guidance for AI agents working in this repository.

## What this project is

cmakedb is a semantic analysis toolkit for CMake built on **execution
tracing**: it records a real `cmake` configure (`--trace-expand
--trace-format=json-v1` + File API), joins the trace with tree-sitter ASTs
of every CMake file, stores the join in SQLite, and implements every
analysis as a query over that database. `design.md` is the authoritative
design document — section references like §4.2 in code comments point into
it. `ROADMAP.md` is the single source of truth for implementation status
against the design; `README.md` carries only a one-paragraph summary that
links to it (see `docs/documentation-policy.md`).

## Build and test

```sh
cargo build --workspace          # zero warnings expected — keep it that way
cargo test  --workspace          # requires cmake >= 3.25 on PATH
                                 # (one test also needs ninja; it self-skips)
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
```

CI (`.github/workflows/ci.yml`) enforces all of the above on Linux
x86_64/arm64 + macOS, plus a pinned cmake 3.25.3 floor leg (supported
CMake is >= 3.25 for the *recording binary* — `global_frame` requirement,
enforced with a loud ingestion error — while recorded projects may declare
any cmake_minimum_required; fixtures use `block()`, also 3.25), a
release-mode ingestion throughput floor, and a non-blocking Windows leg;
`fuzz.yml` runs cargo-fuzz weekly. Run the gates locally before pushing. Releases are
tag-driven (`.github/workflows/release.yml`) and build natively per
arch — keep new native deps buildable on plain ubuntu-22.04 images.

Integration tests (`crates/cmakedb-cli/tests/e2e.rs`) stage fixtures from
`fixtures/` into tempdirs and run real cmake configures — they take a few
seconds each. Never let recordings/build dirs land inside `fixtures/`
(`.gitignore` covers `fixtures/*/build/` and `fixtures/*/.cmakedb/`).

## Workflow rules

- **Commit immediately after each feature/milestone reaches a verified
  (tests-green) state.** Do not accumulate multiple features uncommitted.
  Check `git status` for stray artifacts before staging.
- **Follow `docs/documentation-policy.md`**: every fact has one home
  (`ROADMAP.md` = status/milestones, `README.md` = user-facing, this file
  = workflow/invariants, `design.md` = architecture), and docs are updated
  in the same commit as the change they describe. A completed milestone
  moves to *Done* in `ROADMAP.md` with its commit ref, in that commit.
- Analysis passes (L4) must stay **pure functions over the database** — no
  filesystem or process access inside a `Pass::run`. Anything needing the
  filesystem (e.g. `ninja -t deps`) happens at ingest time or in the CLI.
- Lower layers must not know about analyses (design §3: strict layering).
- Cross-check new ingestion behavior against a real trace before trusting
  it: generate one with
  `cmake --trace-expand --trace-format=json-v1 --trace-redirect=out.jsonl`.

## Crate map (design §5.2)

| Crate | Layer | Notes |
|---|---|---|
| `cmakedb-syntax` | L1 | tree-sitter-cmake ASTs, command/arg extraction, `varref` extraction |
| `cmakedb-record` | L2 | wraps real cmake; never reimplements it |
| `cmakedb-db` | L3 | schema (`schema.rs`), ingestion (`ingest/`): trace parse → AST join → scope/dataflow replay (`dataflow.rs`, the hardest file) → File API (`fileapi.rs`) → ninja deps (`deps.rs`) |
| `cmakedb-passes` | L4 | `Pass` trait, provenance walks, history, lint passes, SARIF/JSON/text renderers |
| `cmakedb-patch` | — | AST-anchored `Patch` model: hash-guarded edits, unified diffs |
| `cmakedb-lsp` | — | post-mortem language server; pure query helpers + thin tower-lsp wire |
| `cmakedb-cli` | — | `cmakedb` binary, config, diff |

There is deliberately **no WASM host crate**: `cmakedb-wasm` was removed
before the first release (wasmtime pulled ~a third of the dependency tree
and 16 open advisories for a provisional ABI). Do not reintroduce a
`wasmtime` dependency without reading the ROADMAP entry first — user
passes are SQL-only today.

`editors/vscode/` holds the VS Code extension (plain JS, no build step).
Milestone status lives in `ROADMAP.md` (M1–M5 done, hardening not).
If you implement an item, update `ROADMAP.md` (and README if
user-visible) in the same commit.

## Hard-won facts — do not rediscover these

Validated against CMake 4.2.1 traces; encoded in `ingest/dataflow.rs`:

- `frame` **resets per directory**; use `global_frame` as the scope-stack
  depth. The event *preceding* a depth increase is always the frame opener.
- Macro bodies DO open frames (their events point at the definition site)
  but do NOT create variable scopes → transparent scope entries.
- `end*()` commands (`endblock`, `endfunction`, …) are **never traced**;
  `block()` scopes close via AST byte-range checks, everything else via
  depth decrease.
- Expanded list variables arrive as **single `;`-joined arguments** in
  trace args (`target_link_libraries(t PRIVATE "A;B;C")`) — always split.
- **Paths keep cmake's own spelling.** Never `canonicalize()` paths that
  join against trace data — macOS `/var` vs `/private/var` broke every join.
  Dep-file paths are normalized lexically only. On Windows, CMake emits
  forward slashes while native paths use backslashes — route every path
  string that joins against trace/File API data through
  `cmakedb_db::cmake_path_spelling` (and prefix checks through
  `cmake_path_starts_with`, case-insensitive there); missing this broke
  every `in_source` join on the Windows CI leg.
- Multiple commands per line are invalid CMake (tree-sitter accepts them;
  real cmake errors).
- `*.h.cmake` files are configure_file templates, not CMake code — excluded
  from the source walk.
- try_compile scratch events (`/CMakeScratch/`, `/CMakeTmp/`) must stay out
  of the build graph.
- Reads come from *raw AST text* (`${X}` refs + bare identifiers in
  `if`/`elseif`/`while` conditions); the trace shows only expanded args.
  Function parameters, `ARGC/ARGV*/ARGN`, and `cmake_parse_arguments`
  outputs are synthesized as writes so reads resolve.
- json-v1 args are **per-source-argument expansions**: an unquoted arg
  that expands to nothing appears as `''` but the real command never
  received it; a quoted `""` is a real argument. Disambiguate via the
  joined AST's argument kinds (found by §6.2 snapshot validation).
- try_compile/try_run scratch configures execute nested **inside** the
  trace but are an isolated variable world in real CMake — their scopes
  are opaque `try_compile` containment scopes and their cache writes are
  localized (also found by snapshot validation).
- Directory requirement commands differ in retroactivity (validated
  against CMake 4.2.1, exploited by LLVM's MCTargetDesc private-header
  hack): `include_directories`, `add_definitions` and
  `add_compile_definitions` also apply to targets **already created** in
  the same directory; `add_compile_options` and `link_libraries` affect
  only targets created later. Subdirectories snapshot the parent's
  directory properties at `add_subdirectory` time.

## Performance context

LLVM soak baseline (§6.4, Apple Silicon): 308k events → 140 MB db,
ingest ~6.6 s (~47k events/s), full lint 25 s. Indexes are dropped before
bulk ingestion and rebuilt after (`schema::INDEXES`) — keep new indexes in
that constant, not inline in `SCHEMA`. If you touch ingestion hot paths,
re-measure with a sparse llvm-project clone (llvm + cmake + third-party +
libc subdirs) before and after.
