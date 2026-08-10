# cmakedb

A semantic analysis toolkit for CMake, built on execution tracing rather than
static interpretation. See [design.md](design.md) for the full design.

cmakedb records a real `cmake` configure run (`--trace-expand
--trace-format=json-v1` + the File API), joins the trace with lossless
tree-sitter ASTs of every CMake file, and stores the result in a single
SQLite database. Every analysis is a query over that database.

**[docs/user-guide.md](docs/user-guide.md)** documents every capability
and, for every detection, exactly when it can be a false positive — read
it before wiring findings into CI gates.

## Quick start

```text
# Record a configure run (wraps the real cmake)
cmakedb record -- -S . -B build -G Ninja
cmakedb record --preset default            # or via CMakePresets.json
cmakedb record --capture-scopes -- ...     # + scope snapshots: ingestion
                                           #   cross-validates the replay
                                           #   against CMake's own dumps

# Provenance: full propagation chains with file:line at every hop
$ cmakedb why-links app z
app links z via:
app
└─ PRIVATE net    [src/app/CMakeLists.txt:2]
   └─ PUBLIC http    [src/net/CMakeLists.txt:2]
      └─ INTERFACE ZLIB::ZLIB    [src/http/CMakeLists.txt:3]
         └─ INTERFACE z    [cmake/FindDeps.cmake:2]
final resolved evidence (File API):
  -lz

$ cmakedb why-flag app NET_VERSION
$ cmakedb why-includes app include/
$ cmakedb why-value STALE_VALUE            # write history, time-travel style

# Negative provenance: why did something NOT happen?
$ cmakedb why-not links app zlib_stub
app does not link zlib_stub. Candidate sites:
  CMakeLists.txt:9  target_link_libraries(app PRIVATE zlib_stub)
    └─ never executed: guard if(ENABLE_COMPRESSION) at CMakeLists.txt:8 chose another branch
         ENABLE_COMPRESSION = "OFF" (set at CMakeLists.txt:3, cache)
$ cmakedb why-not target BrainF            # ... | why-not set SPECIAL_FLAG

# Dead code
$ cmakedb dead options
warning [dead-options] UNREAD BUILD_LEGACY_DRIVER (set, never read) — not read in this configuration
  --> CMakeLists.txt:3
warning [dead-options] UNREAD ENABLE_PROFILING (read only by code that never executed)
  --> CMakeLists.txt:4
      CMakeLists.txt:8: referenced here, but this code never ran
$ cmakedb dead functions | dead modules | dead variables
$ cmakedb dead options --path src/net      # scope any finding command
                                           # to a subdirectory
$ cmakedb dead options --also macos.db --also full.db
                                           # intersect across preset
                                           # recordings: only findings
                                           # dead in EVERY config survive

# Scope bugs
$ cmakedb clobbers                         # value overwritten before any read
$ cmakedb scope-leaks                      # macro writes escaping into callers

# Visibility (needs a built tree for dep files)
$ ninja -C build && cmakedb overshare --deps-from build
note [overshare] http: PUBLIC include dir '.../include' is used only by http's
own sources in 2 consumer(s) checked — consider PRIVATE

# CI
$ cmakedb lint --format sarif --output cmakedb.sarif   # exit 1 at fail-on level
$ cmakedb diff a/.cmakedb/trace.db b/.cmakedb/trace.db # compare recordings

# Modernize: verified codemods (auto-reverted if the build graph changes)
$ cmakedb modernize --check              # findings + planned patches as diffs
$ cmakedb modernize --fix                # apply, re-record, verify isomorphism

# In-editor: post-mortem LSP (diagnostics, hover, go-to-definition)
$ cmakedb lsp                            # stdio language server

# Everything else
$ cmakedb query "SELECT name FROM targets"

# User passes: drop .sql files into .cmakedb/passes/
#   SELECT f.path AS file, e.line AS line, '…' AS message FROM events e ...
# and they run as lint passes with the file stem as the rule id.
```

Configuration lives in `.cmakedb.toml` (see design §2.5): pass
enable/disable, `fail-on` threshold, per-pass `ignore-patterns`.

## Status

**All five design milestones (M1–M5) are complete and validated** —
recorder and semantic database, every provenance/dead-code/visibility
analysis, SARIF/JSON/text output, `diff`, config, scope-snapshot capture
(`--capture-scopes`), the modernize codemods with the re-record
verification loop, `cmakedb lsp`, and user SQL passes plus a VS Code
extension — with two caveats recorded in the roadmap (WASM user passes
are deferred, so extensibility ships as SQL passes only; the VS Code
extension has not yet run in a live editor). The one open item is the
hardening track's Windows /
case-insensitive-filesystem path audit (§6.5); fuzzing, the pinned-CMake
version-floor CI leg, and the performance gates have all landed.
**Windows is experimental**: its CI leg is non-blocking and prebuilt
binaries ship for Linux only. See [ROADMAP.md](ROADMAP.md) — the single
source of truth for status, milestone breakdowns, and acceptance
criteria — and [docs/documentation-policy.md](docs/documentation-policy.md)
for how documentation is maintained.

Known deviations / approximations:

- `frame` in the json-v1 trace resets per directory; scope reconstruction
  uses `global_frame` (validated against CMake 4.2 traces).
- `end*()` commands are never traced, so `block()` scopes close via AST
  byte-range checks rather than an explicit event.
- Command result values that the trace doesn't expose (e.g. `list(APPEND)`
  results, `find_*` outcomes) are recorded as writes with NULL values.
- Paths keep cmake's own spelling (no symlink resolution) so all four data
  sources join consistently; dep-file paths are normalized lexically.
- SARIF is emitted directly (small, schema-tested) rather than via
  `serde-sarif`.

## Real-world validation (design §6.4)

Soak-tested against llvm-project (llvm subproject, Release/AArch64,
CMake 4.2.1, Apple Silicon):

- 308,427 trace events across 840 files → 140 MB database; 702 targets,
  1,409 link edges, 141k variable writes, 461k reads (98.1% resolved).
- Ingestion invariants hold: **0 unjoined events** in project files; all
  unjoined events elsewhere are explainably synthetic (`cmake_language(EVAL)`
  virtual files, deleted TryCompile scratch dirs).
- Ingestion ≈ 6.6 s (~47k events/s, ~205k rows/s) on top of the ~13.6 s
  traced configure; `why-links` answers in <10 ms warm; full lint (454
  findings) in 25 s — within the §6.6 gates except ingestion, which is ~6%
  under its 50k events/s target.
- Findings spot-checked as real: config-excluded modules (Sphinx docs,
  cross-compile, `FindDIASDK`), dead options with the "read only by code
  that never executed" qualifier, and `scope-leaks` catching
  `add_llvm_library`'s known `cmake_parse_arguments` leakage through macro
  scope. Provenance walks LLVM's component system with origins inside
  `LLVM-Config.cmake`/`LLVM-Build.cmake`.

LLVM-scale lessons folded back into ingestion: expanded list variables
arrive as single `;`-joined trace arguments (edges/requirements are split),
`*.h.cmake` configure templates are excluded from parsing, dead-module
guards cover any command referencing the file (e.g. `configure_file`), and
indexes are built after bulk load.

## Installation

Supported CMake: **>= 3.25 for the binary that runs `cmakedb record`**
(it must emit the json-v1 `global_frame` field the scope replay is built
on; 3.25.3 is the oldest version our CI verifies). The floor is about
the *binary*, not the project: a project declaring
`cmake_minimum_required(VERSION 3.20)` — or 2.8 — records fine under a
modern binary, and cmakedb fails loudly (rather than degrading) if a
recording was made with a binary too old to support the replay.

Prebuilt Linux binaries (x86_64 and aarch64) are attached to
[GitHub releases](https://github.com/christopherbate/cmakedb/releases);
each tarball ships the binary, this README, the user guide, and both
license files, with a `.sha256` alongside. Otherwise
`cargo build --release -p cmakedb-cli` (the binary lands in
`target/release/cmakedb`).

Building from source needs **Rust >= 1.95** (the floor comes from
`libsqlite3-sys`; CI pins a leg to it). The prebuilt binaries have no
Rust requirement.

**Recording an untrusted repository runs its code.** `cmakedb record`
invokes real `cmake`, which executes the project's CMake files — no more
and no less dangerous than configuring it by hand. Analysis commands read
only the recording. See [SECURITY.md](SECURITY.md) for the full trust
model, including `--no-user-passes` for linting repositories you do not
trust.

## Development

```text
cargo build --workspace
cargo test  --workspace     # needs cmake (and ninja for one test) on PATH
```

CI (GitHub Actions) gates every PR on `cargo fmt --check`,
`clippy -D warnings`, `cargo deny check` (licenses + RUSTSEC advisories),
a Rust 1.95 MSRV leg, and the full test suite across Linux x86_64,
Linux arm64, and macOS; tagging `v*` builds and publishes the release
binaries (built natively per-arch, glibc 2.35 baseline, smoke-tested by
recording a real configure before packaging).

[CONTRIBUTING.md](CONTRIBUTING.md) covers the architectural ground rules
(never reimplement CMake; passes stay pure functions over the database;
every detection ships with its false-positive analysis).

Crate layout matches design §5.2: `cmakedb-syntax`, `cmakedb-record`,
`cmakedb-db`, `cmakedb-passes`, `cmakedb-patch`, `cmakedb-cli`, plus
`fixtures/` (each fixture encodes one semantic phenomenon and is exercised
end-to-end by `crates/cmakedb-cli/tests/e2e.rs`).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

`deny.toml` records the license policy for the dependency tree, including
the permissive-arm elections for the few dual-licensed crates (see
`cargo deny check licenses`).
