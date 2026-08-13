# Roadmap and Status

Single source of truth for project status (see
[docs/documentation-policy.md](docs/documentation-policy.md)). Milestone
numbers refer to [design.md](design.md) §5.3.

## Feature roadmap v2 — for CMake users

Proposed next-generation features, prioritized by user pain and grounded
in evidence the database already records (nothing here requires
reimplementing CMake). Per docs/documentation-policy.md, every shipped
item lands with docs (and an FP profile if it detects anything) in the
same commit. Items marked (L) are large.

### Track A — Debugging & explanation UX

- [x] **`cmakedb explain <file:line>`** (this commit) — "what did this
      line do?": every evaluation of the commands at that location with
      expanded values (distinct argument sets collapsed with counts),
      call-chain context, the writes/edges/requirements/targets/
      properties it produced, and one hop of influence (reads resolved
      to each write with first locations; File API link evidence per
      edge). Never-executed lines get a clearly-labeled static AST
      context (enclosing if/loop/definition headers) instead. Text +
      versioned JSON; LLVM-validated on 100+-evaluation lines.
- [x] **Negative provenance: `why-not`** (this commit) — `why-not links
      <target> <lib>` / `why-not target <name>` / `why-not set <var>`.
      Finds every AST site that could have produced the missing thing
      (including `add_*` wrapper calls naming the target first) and
      classifies each: guard-failed (innermost *executed* guard with
      observed condition values and their provenance, including
      command-line `-D` preseeds), executed-differently (with an
      explicit "links X into other-target, not yours" note),
      inside-uncalled-function, file-never-ran (includer sites
      classified recursively), plus mentions context and edit-distance
      suggestions. Existing things redirect to the matching `why-*`.
      Text + versioned JSON. LLVM-validated (BrainF →
      `if(LLVM_INCLUDE_EXAMPLES)` → `-DLLVM_INCLUDE_EXAMPLES=OFF` chain
      through two never-run directories). Side fix surfaced by
      validation: non-FORCE `set(CACHE)`/`option()`/`find_*` no longer
      overwrite an existing cache entry in dataflow resolution
      (CacheSoft), so guard values now show the *effective* -D/preset
      value instead of the declared default.
- [x] **`cmakedb graph`** (this commit) — dependency graph export (DOT /
      Mermaid / JSON) with visibility-styled edges (solid/dashed/dotted),
      `--target` forward-closure and `--path` filtering, `--no-external`,
      and per-edge origin `file:line` in tooltips/link text/JSON. Unlike
      `cmake --graphviz`, edges carry provenance and as-written
      visibility. JSON is versioned (`cmakedb_graph_version: 1`).
- [ ] **`cmakedb bisect <a.db> <b.db>`** — first-divergence analysis
      between two recordings: the earliest event where the traces
      diverge, plus the dataflow chain feeding it. Turns "works in CI,
      broken here" from log-diffing into one command. Evidence: aligned
      event streams (diff §4.7 alignment) + dominating-write chains.

### Track B — CI & ecosystem

- [ ] **Diff-aware lint (`--changed-since <rev>`)** — report only
      findings whose declaration site is on lines touched since a git
      rev; pairs with the baseline for PR-scoped gating without a stored
      baseline file. Needs git plumbing in the CLI only.
- [ ] **GitHub Action** — marketplace action wrapping record → lint →
      SARIF upload → baseline ratchet, with the prebuilt release
      binaries; turns adoption into five YAML lines.
- [x] **`cmakedb matrix`** (this commit) — record every non-hidden
      configure preset (or `--preset` selections) sequentially into
      `.cmakedb/matrix/<preset>.db`, per-preset status table, then all
      enabled lint passes on the `--also` intersection of the successful
      recordings; failing presets are reported and skipped
      (`--fail-fast` to stop), exit non-zero on any failure and
      `--fail-on` against the intersection. Side fix: `record --preset X
      --build-dir Y` now passes `-B` to cmake, so an explicit build dir
      actually overrides the preset's binaryDir (it previously configured
      one directory and read the File API from another).
- [x] **Build-config SBOM: `cmakedb deps`** (this commit) — inventory
      of `find_package` / `FetchContent` / `ExternalProject`
      dependencies with resolved versions, locations, pinning status
      (fetchcontent-pinning's evidence reused), and CycloneDX JSON
      export. The configure trace is the only place this inventory is
      complete and exact. Shipped: text / versioned JSON / CycloneDX
      1.5 output; find_package resolution read back from `<Pkg>_FOUND`
      / `_VERSION` / `_DIR` writes (case-insensitive); pinning classes
      share `is_commit_sha` with the lint; purls derived only for
      github sources pinned to a full SHA (never fabricated); imported
      targets grouped by `Ns::` namespace. LLVM-validated (7 entries,
      OCaml correctly not-found, ZLIB correctly absent — its
      find_package was gated off by `-DLLVM_ENABLE_ZLIB=OFF`).

### Track C — Codemods v2 (the §4.6 verification loop exists; reuse it)

- [ ] **`cmake_minimum_required` bump assistant** — given a target
      version, report which policy behavior changes the project would
      actually exercise (trace evidence per policy, e.g. CMP0054 quoted
      arguments observed in executed if()s), then apply the bump behind
      the re-record verification loop. Design §4.6 names this; the
      policy-hygiene metadata table seeds it.
- [ ] **Dead-code removal codemod** — `modernize --fix`-style removal for
      dead options/functions/modules findings that survive an `--also`
      intersection across every recorded preset, verified by the
      isomorphism loop. Deleting dead CMake is scarier than deleting dead
      C++; the verification loop is what makes it offerable.
- [ ] **Variable rename** — dataflow-safe rename across writes/reads
      (dominating-write graph gives exact rename sites; AST anchors give
      exact spans; the §6.2-validated replay is what makes this
      trustworthy). (L)

### Track D — Profile v2

- [x] **Flamegraph export** (this commit) — folded-stack output from the
      scope tree (`cmakedb profile --flamegraph > out.folded`) for
      flamegraph.pl/inferno/speedscope; values are per-scope exclusive
      microseconds (they sum to the configure span — LLVM-validated:
      9385 stacks summing to exactly the 13.04s span).
- [x] **`cmakedb profile --compare <b.db>`** (this commit) —
      configure-time regression detection between recordings: per-scope
      inclusive deltas aligned by display-normalized (kind, name), call
      counts, appeared/disappeared marking, span delta; text + versioned
      JSON (the CI floor knob is a jq check on `span_delta_us`, guide
      §"Comparing recordings").

### Track E — Engine investments

- [ ] **Value-carrying intersection key** — extend the `--also`/baseline
      finding identity with an optional observed-value component,
      unlocking sound single-evaluation dead-branch detection in
      constant-conditions (documented future work there).
- [ ] **WIT/component-model WASM ABI v1** — reintroduce WASM user passes
      against a component-model interface when guest tooling matures.
      Supersedes the removed ABI v0 host (see M5); the bar for bringing
      back a JIT-sized dependency is that the runtime's advisory record
      and footprint justify the capability over SQL passes.
- [ ] **cmake-master CI leg** — trace-format drift detection against
      CMake dev binaries (needs a nightly-binary source).
- [ ] **`custom-commands` config consumption** (§2.5 debt) — wrapper
      signatures refining argument parsing beyond the
      `cmake_parse_arguments` synthesis.

### Track F — Simplification (help users shrink their CMake)

The differentiator over text-level tools (cmake-lint, cmake-format):
every proposal below is grounded in recorded evidence, and the
mechanical ones ride the modernize verification loop (apply → re-record
→ diff the semantic model → auto-revert on any change), which turns
risky refactors into safe codemods. Blanket caveat, documented per
rule: everything is proven *for the recorded configuration(s)* — fixes
should only auto-apply when the finding survives a `matrix`/`--also`
intersection across every recorded preset.

Requirement placement & narrowing:

- [x] **Directory-command demotion** (this commit) —
      `include_directories`, `link_libraries`, `add_definitions`,
      `add_compile_options` at directory scope → target-scoped
      equivalents. The trace names exactly which targets were created
      under the directory scope after the command ran; propose
      `target_*(<tgt> PRIVATE …)` per affected target. FP: a sibling
      directory relying on inheritance we misattribute — exactly the
      class the re-record diff catches. Recommended first pick:
      canonical modernization ask, exact evidence, reuses overshare +
      modernize machinery. (L)
      *Result*: shipped as the report-only `directory-commands` built-in
      (+ `add_compile_definitions`), with affected-set semantics
      validated against real cmake (retroactivity of
      include_directories/add_definitions/add_compile_definitions;
      add_subdirectory property snapshots) — LLVM: 126 raw events → 13
      directory-scope project sites → 13 findings, 0 FPs after tuning
      (two naive-"dead" claims disproved and converted by the
      retroactivity model; see the user guide's FP analysis).
- [ ] **Unused include-dir pruning** — a `target_include_directories`
      entry where no TU of the target includes any header under that
      directory. Evidence: `tus`/`tu_headers` dep-file ingestion
      (documented precondition: a *built* tree). FP: generated
      headers, other-config-only headers (B1), IDE convenience paths.
- [x] **Shadowed-requirement dedup** (this commit) — the same include
      dir/definition reaching a target multiple ways (directory scope +
      explicit target call + transitive PUBLIC dep, one hop). CMake
      dedups the compile line, so this is invisible at build time but
      real maintenance noise. Evidence: trace `usage_reqs` vs File API
      resolved sets.
      *Result*: shipped as the report-only `shadowed-requirements`
      built-in (note severity), reusing the validated directory-scope
      retroactivity model, with genex/SYSTEM/try_compile guards and a
      same-directory standalone-build heuristic — LLVM: 113 direct
      declarations × 13 directory commands × 1406 link edges → 0
      findings, confirmed genuine by an independent SQL sweep (0 raw
      value collisions) and a planted-redundancy detection check.
- [x] **Transitively redundant link edges** (this commit) —
      `target_link_libraries(app PRIVATE a b)` where `b` already
      arrives via `a`'s PUBLIC interface. FP: intentional static-
      archive link ordering / ODR-sensitive cases — report-only by
      default, fix behind a flag.
      *Result*: shipped as the report-only `redundant-links` built-in
      (note severity), bounded PUBLIC/INTERFACE chain search (depth cap
      8, cycle guard) — LLVM: 1019 raw → 16 findings after the
      multi-target origin-site guard discovered in validation (LLVM's
      component-closure machinery links deliberate full static-archive
      closures from 3 helper lines); all 16 source-verified true for
      the recorded config, 1 (`llvm-exegesis`) illustrating the
      documented B1 caveat.

Variable simplification:

- [x] **Single-use variable inlining** — `set(SRCS …)` with exactly one
      resolved read → inline at the read site. Dataflow proves the
      read count; trivial-to-verify codemod. Recommended second pick
      (with aliases below): plain SQL over existing tables.
      (this commit) Shipped report-only as the `single-use-variables`
      built-in (note severity). LLVM-validated: 17 raw → 5 final after
      adding the unexecuted-write-target guard discovered in validation
      (the conditional-default idiom, 12/17 raw FPs); of the 5, 4 are
      genuine set-once-read-once pairs (readability judgment calls) and
      1 is the documented template-consumer FP class
      (`PACKAGE_BUGREPORT`, read by `config.h.cmake`).
- [x] **Alias / pass-through variables** — `set(X ${Y})` where every
      read of `X` observed a value identical to `Y`'s at that moment →
      replace reads with `Y`. Includes **PARENT_SCOPE round-trips**: a
      child sets a parent variable the parent only feeds back into the
      same child.
      (this commit) Shipped report-only as the `alias-variables`
      built-in (note severity), both shapes: pure aliases plus the
      narrow `set(V ${V} PARENT_SCOPE)` no-op round-trip.
      LLVM-validated: 36 raw alias shapes → 1 finding
      (`HAVE_BACKTRACE` aliasing `Backtrace_FOUND`, genuine for every
      CMake-visible read, template-consumer caveat documented); all 3
      raw round-trip shapes correctly suppressed as load-bearing.
- [ ] **No-op writes** — a `set()` whose value equals the dominating
      value already in effect (copy-paste config blocks); report the
      redundant sites, keep one.
- [x] **Empty-expansion no-op calls** (this commit) — e.g.
      `target_link_libraries(x ${EXTRA_LIBS})` where the list expanded
      to nothing in every evaluation: dead code, or a list that was
      never populated (pairs with undefined-reads suggestions).
      *Result*: shipped as the report-only `noop-calls` built-in
      (note severity), arg-kind-aware (quoted `""` is a real argument)
      with never-set / B1 enrichment — LLVM: 130 candidate call sites →
      2 raw all-empty → 1 finding, source-verified TP
      (`${LLVM_COMPILE_DEFINITIONS}`, MSVC-only writer, correctly
      tagged B1), 0 FPs; the 1 exclusion is the documented
      quoted-expansion FN (`LLVMDebugInfoPDB`, DIA-SDK-only).

Structural factoring (report-only — the evidence is exact, the
suggested shape is stylistic):

- [ ] **Common-requirement factoring** — cluster targets by recorded
      requirement sets; when N targets repeat the same
      warnings/definitions block, propose the `project_warnings`
      INTERFACE-library idiom (or a helper function) and list the call
      sites that would collapse.
- [ ] **`find_package` consolidation** — the same package resolved
      repeatedly across subdirectories where one top-level call
      suffices; scope tree + events give call sites, `profile` gives
      the configure-time cost.
- [ ] **Move-closer-to-use** — a variable or `include()` consumed only
      inside one subdirectory → relocate down (the inverse of
      demotion, same scope-attribution evidence).

Suggested order: directory-command demotion → single-use inlining +
aliases → shadowed-requirement dedup; unused include-dir pruning once
dep-file ingestion is wired into the default workflow.

## Milestone status

All five design milestones (§5.3) are **complete**. The only work still
open is the hardening track's Windows path audit and the feature roadmap
above; neither is milestone-bound.

| Milestone | State | Commit | Summary |
|---|---|---|---|
| M1 Recorder + DB | done | `083c964` | record (trace+File API), §4.1 schema, scope replay + dataflow, query; LLVM-validated |
| M2 First value | done | `083c964` | why-links / why-value / dead options; text/JSON/SARIF |
| M3 Breadth | done | `083c964` | clobbers, scope-leaks, dead \*, overshare, diff, why-includes/flag, config, fail-on |
| M4 Modernize + LSP | done | see detail below | lossless printing, patch engine, codemods + §4.6 verification loop, LSP + quickfixes |
| M5 Extensibility | done, 2 caveats | see detail below | SQL user passes (WASM deferred), provenance LSP requests, VS Code extension |
| Hardening | **in progress** | see detail below | Windows/case-insensitive path audit open (§6.5); fuzzing, version-floor CI, perf gates done |

M1–M3 details live in the commits and README's validation section. M4,
M5, the hardening track, and the lint roadmap keep their full entries
below — several completed recently, and their validation numbers are the
FP documentation trail.

## Milestone detail

### M4 — Modernize + LSP (design §5.3, §4.6, §2.4)

M4 is **complete** (including the LSP follow-ups, ticked below).

- [x] **Lossless printing layer** in `cmakedb-syntax` (`reprint`, this
      commit): reconstructs source byte-identically from leaf spans + gaps,
      erroring on incoherent spans; property-tested over the fixture corpus
      and edge cases (CRLF, bracket args, error-tolerant parses) (§6.3).
- [x] **Patch engine integration** (this commit): modernize planning is a
      pure db function (recorded file content stored in `files.content`);
      `modernize --check` renders findings + unified diffs, `--fix`
      applies via the hash-guarded `cmakedb-patch` model.
- [x] **Codemods** (this commit, §4.6 steps 1–5): shared engine for
      `include_directories`, `add_definitions`, `add_compile_options`,
      `link_libraries` → target-scoped PRIVATE calls; affected targets
      from recorded scope/event order + File API final-state shadowing
      check; multi-evaluation and non-directory-scope calls skipped with
      notes; include dirs absolutized against the evaluating directory.
      Covered by fixtures/legacy e2e (comments/bytes preserved).
- [x] **Verification loop** (this commit, §4.6 step 6):
      `cmakedb-record::verify::apply_and_verify` re-records with the same
      build dir (cache preserves the configuration) and asserts
      target-set + resolved-requirement isomorphism
      (`diff::graph_isomorphism_diff`); on mismatch every edit rolls back
      and the differences are reported; on success the fresh recording
      replaces the database. Acceptance test: a graph-breaking patch is
      auto-reverted (`verification_loop_reverts_graph_breaking_patch`).
- [x] **LSP server** (this commit, `cmakedb-lsp`, §2.4): `cmakedb lsp`
      serves stdio via tower-lsp. Diagnostics from all lint passes,
      stale-marked when the open buffer's hash differs from the
      recording; hover (precise per-line read value + write history for
      variables, type/links/includes for targets); go-to-definition
      (dominating write for the read at the position, then function
      defs, target definition sites). Query helpers are pure db
      functions with tests. Still open: the custom `cmakedb/provenance`
      request and code actions (tracked below).
- [x] LSP follow-ups (this commit): custom requests
      `cmakedb/provenance` (full Explanation JSON, link or requirement
      kinds), `cmakedb/targets`, `cmakedb/edges` (tree-view data with
      origins); code actions offering modernize patches as quickfixes —
      byte-anchored edits convert to UTF-16 LSP positions against the
      *recorded* content and are withheld for stale buffers.
- [x] **Scope snapshots** (this commit, §3.2 channel 2):
      `record --capture-scopes` (or `[record] capture-scopes`) injects a
      `CMAKE_PROJECT_INCLUDE_BEFORE` hook that hex-dumps the visible
      variable table at each `project()` call and at top-directory end
      (`cmake_language(DEFER)`); ingestion pairs dumps with the hook's
      `file(APPEND)` trace events and cross-validates the replayed table —
      divergence is a hard ingestion error and the transaction rolls back
      (§6.2). The validator immediately earned its keep by catching two
      real replay gaps: json-v1 trace args are per-source-argument
      expansions (unquoted empty expansions appear as `''` but were never
      real arguments — disambiguated via the joined AST's argument kinds),
      and `include(... RESULT_VARIABLE)` writes. On LLVM it further
      exposed that try_compile scratch configures execute nested inside
      the trace but are an isolated variable world — now contained in
      opaque `try_compile` scopes. LLVM validates cleanly with
      `--capture-scopes` (313k events).

### M5 — Extensibility (design §5.3, §3.4)

M5 ships as **SQL-file user passes + the editor extension**, with one
recorded caveat: the WASM pass API is deferred (see below). The earlier
"never run in a live editor" caveat is closed — the extension now has an
integration suite that runs it inside a real VS Code instance, gated in
CI (`vscode-extension` job).

- [x] SQL-file user passes (this commit): `*.sql` files in
      `.cmakedb/passes/` (or `[passes] sql-dir`) run as lint passes; the
      SELECT must produce `file`, `line`, `message` (+ optional
      `severity`, `col`); rule id = file stem, first `--` comment is the
      description; statements run under SQLite `query_only`; malformed
      shapes fail loudly.
- [ ] WASM pass API — **deferred; implemented, then removed before the
      first release.** `cmakedb-wasm` hosted `*.wasm` passes on wasmtime
      with fuel + memory limits and a linear-memory ABI v0
      (`cmakedb_alloc`/`cmakedb_pass_run`/host `env.cmakedb_query`,
      read-only), ABI conformance-tested with a WAT guest including
      write-rejection. It was dropped because the cost/benefit did not
      survive review before going public:
      - wasmtime pulled **81 crates that existed for nothing else** —
        about a third of the dependency tree, including a full Cranelift
        JIT — to support 243 lines of host code with one call site.
      - `cargo deny check advisories` reported **16 open RUSTSEC
        advisories** against wasmtime 27, several of them sandbox
        escapes; RUSTSEC-2026-0096 (miscompiled guest heap access on
        aarch64 Cranelift) applies to a published release target, which
        undermined the containment the feature's whole security story
        rested on.
      - The ABI was always provisional — the component-model/WIT
        interface below was already the intended replacement — and it
        shipped with no example guest and one sentence of user docs.
      - A WASM pass's only host import was read-only SQL, so it offered
        SQL passes plus arbitrary computation; SQL passes deliver most of
        that value at zero dependency cost.

      Restoring it should target the WIT/component-model ABI directly
      (Track E) rather than reviving ABI v0. Removal commit also dropped
      the `[passes] wasm-dir` config key; a `.cmakedb.toml` still setting
      it now fails loudly (the section is `deny_unknown_fields`).
- [x] Editor extension (this commit, `editors/vscode/`): language
      client over `cmakedb lsp` plus the "CMake Provenance" explorer
      view — targets expand into link edges with visibility and origin
      `file:line` (click-to-jump), fed by `cmakedb/targets` +
      `cmakedb/edges`; the `cmakedb: Why …` command renders the full
      propagation tree from `cmakedb/provenance`. Exercised in a real
      VS Code instance by `editors/vscode/test/` via `@vscode/test-cli`:
      activation, contributed commands, a guard that the test workspace
      really holds a recording, a live language-client round trip
      asserting diagnostics come back, and the refresh command against a
      running server. The workspace is staged and recorded by
      `test/prepare-workspace.js`, which fails hard if the binary or the
      recording is missing so the suite cannot pass vacuously. Runs
      headless under xvfb in CI.
- [ ] **Marketplace publication — deferred by decision, not blocked.**
      The extension packages cleanly (`vsce package` → a complete .vsix
      with icon and license text), but `publisher` is the placeholder
      `"cmakedb"`, which is not a registered Marketplace publisher ID.
      Publishing needs a Microsoft/Azure DevOps account, a PAT scoped to
      *all accessible organizations* with Marketplace→Manage, and a
      publisher ID registered at marketplace.visualstudio.com/manage.
      Prefer a personal ID (e.g. `christopherbate`) over the product
      name: a publisher hosts every extension you ever ship, and the
      extension's unique id is `publisher.name`, so changing it after
      publication orphans existing installs. Nothing in the repo depends
      on the value — the test suite derives the id from package.json —
      so the rename is a one-line change whenever the account exists.
      Open VSX (`ovsx publish`) is the separate registry serving
      VSCodium/Cursor/Gitpod, should wider reach be wanted.

### Hardening (design §6.5–§6.6, not milestone-bound)

- [x] Fuzzing (this commit): seeded-mutator robustness tests run in every
      CI job (parser never-panics + reprints losslessly over 2000 mutated
      inputs; six malformed-trace shapes fail cleanly with full rollback;
      snapshot parsing never panics), plus coverage-guided `cargo-fuzz`
      targets under `fuzz/` run weekly by `fuzz.yml`. Caveat: the fuzz
      targets themselves are nightly-only and first compile in CI (no
      nightly toolchain on the dev machine). `record` now deletes the
      database file on ingestion failure so a failed run can't masquerade
      as a recording.
- [x] Base CI (this commit): GitHub Actions gating fmt/clippy(-D
      warnings)/tests on Linux x86_64 + arm64 + macOS, and a tag-driven
      release workflow publishing prebuilt, smoke-tested Linux binaries
      for both architectures (native runners, glibc 2.35 baseline).
- [x] CMake version floor CI (this commit): a pinned cmake 3.25.3 leg
      runs the full suite (verified locally on macOS first); the main test
      jobs cover current stable. **Deviation from the design's ≥3.17**:
      the effective floor is 3.25 because the fixtures use `block()`
      (3.25) and the scope replay requires `global_frame` in the json-v1
      trace. A cmake-master leg remains open (needs a nightly-binary
      source).
- [x] Performance (this commit): parallel AST parsing (rayon) closed the
      §6.6 gap — LLVM ingestion now ~50.5k events/s on the reference
      machine (was ~47k). CI enforces a floor via a portable release-mode
      throughput test (150k synthetic events, generous 10k/s floor to
      absorb runner variance) instead of the design's criterion
      benchmarks — recorded deviation; criterion remains attractive for
      local regression hunting.
- [~] Windows/case-insensitive-filesystem path handling audit (§6.5).
      Round 1: CMake's forward-slash spelling vs native backslashes broke
      every `in_source` join — fixed via `cmake_path_spelling`/
      `cmake_path_starts_with` at all boundaries. Round 2 (next CI run):
      the entire e2e + robustness suites went green on Windows, incl.
      recording under Visual Studio/MSVC; the two remaining failures were
      LSP helper *tests* querying with native paths where the production
      wire layer normalizes via `db_path_for` — tests now use the same
      call path. Notable: cmake preserves 8.3 aliases (RUNNER~1) as
      spelled, so exact-match lookups hold. Closing pends one more green
      Windows run; mixed 8.3/long-name spellings within one build remain
      the documented residual risk.

### Lint roadmap (proposed)

New detections, prioritized by (user pain × achievable confidence ÷
cost). Grounding rule: every entry names the **evidence already in the
database** it would run on — nothing here requires reimplementing CMake.
Per docs/documentation-policy.md, a rule ships only with its
false-positive analysis in the user guide; the FP sketch below is the
start of that analysis, not a substitute.

**Lifecycle**: prototype as a SQL pass (`.cmakedb/passes/*.sql`) →
validate FP rate on LLVM + the fixture corpus → promote to a built-in
with tuning knobs and guide entry. This is the design §5.1 payoff put to
work; several Tier 1 rules are one SELECT away from a working prototype.

#### Tier 1 — pure queries over the existing schema

- [x] **`option-after-use`** (this commit) — declaration-ordering bug,
      built-in pass validated on LLVM per the lifecycle (8 raw findings →
      3 excluded idioms discovered: DEFINED probes, guard-idiom
      containment, -D pre-seeds → 5 final: 2 genuine value-corruption
      warnings incl. LLVM's empty-triple gold-linker search, 3
      fragile-ordering notes). Side effect hardening: the recorder now
      passes -D/preset cacheVariables pre-seeds into ingestion so early
      reads of user-provided cache values resolve (replay fidelity
      improvement benefiting all passes).
- [x] **`undefined-reads`** (this commit) — built-in, note severity,
      LLVM-validated: 27 raw findings → 10 after excluding the
      conditional-set-then-use idiom (a set() for the name exists in
      unexecuted AST branches — intentional optional values), all 10 the
      legitimate empty-if-unset configuration surface. Typo subclass
      carries edit-distance ≤2 "did you mean" suggestions; names with any
      recorded write are option-after-use territory and excluded.
- [x] **`genex-in-wrong-context`** (this commit) — built-in: literal
      `$<...>` reaching commands that never evaluate genexes (`if`/
      `elseif`/`while` and `execute_process` at warning; `message`,
      `file(WRITE|APPEND|READ)`, `configure_file`, `string(COMPARE)`,
      `math` at note). The roadmap's flow-through FP sketch became the
      core design: storing commands (`set`/`list`/`string` building) are
      never flagged directly — a stored genex resurfaces in its sink's
      expanded args, so the misusing *read site* is flagged with the
      storing `set()` as related dataflow evidence, and genex-aware
      sinks/unread stores suppress naturally. LLVM-validated: 53 raw
      genex-bearing events in the curated command list (49 set + 4 list,
      0 direct non-evaluating sinks) → 0 findings; every stored flow
      spot-checked into genex-aware sinks (`$<TARGET_FILE:>` paths into
      add_custom_command dominant) or nested-genex building
      (HandleLLVMOptions). Escaped `\$<` regex detection and
      unterminated `$<` probe idioms excluded by construction.

- [x] **`duplicate-links`** (this commit) — built-in, LLVM-validated:
      16 raw GROUP BY (src,dst) count>1 groups → 16 final (nothing
      excluded on LLVM; the folding knobs — per-config
      `debug`/`optimized`/`general` slots recovered by re-walking the
      originating call's expanded args, since ingestion drops the
      keywords; genex-containing destinations; out-of-source-only groups
      — all validated on the fixture corpus instead). 5 conflicting-
      visibility warnings, all spot-checked genuine (LLVMJITLink ×4 and
      LLVMOrcJIT link libraries PRIVATE that their LINK_COMPONENTS
      already link PUBLIC — the PRIVATE calls are subsumed); 11
      same-call redundancy notes (LLVM's component resolver emits
      duplicate items in one expanded list — true but generated, hence
      note severity + ignore-patterns as the knob).
- [x] **`cyclic-links`** (this commit) — built-in, note severity, SCC
      (iterative Tarjan) over real-target edges with the shortest cycle
      rendered like a why-links path and per-hop origin locations;
      imported-member and out-of-source-only cycles skipped.
      LLVM-validated: 0 findings (the component graph is a DAG —
      cross-checked with a direct SQL probe for 2-cycles/self-loops);
      positive case covered by the fixtures/linkgraph 2-cycle e2e.
- [x] **`execute-process-unchecked`** (this commit) — built-in, note
      severity, `execute_process` whose `RESULT_VARIABLE` is absent or
      written-but-never-read (`COMMAND_ERROR_IS_FATAL` recognized as a
      check, `ERROR_QUIET` reported as aggravating context).
      LLVM-validated: 14 raw events → 11 source sites → 7 final findings
      (all the no-RESULT_VARIABLE shape); the 4 RESULT_VARIABLE sites
      (`TT_RV`, python-module `status`, `git_result`, relpath `result`)
      all correctly resolved as checked through later condition reads,
      including bare `if(status)` and per-invocation function scopes.
      Surviving findings spot-checked: 5 are the documented best-effort
      probing FP class (xcrun/-find, libtool -V, ninja --version,
      linker-detection stderr sniffing — outcome judged via
      OUTPUT_VARIABLE content, hence note severity), 2 genuine unchecked
      `cmake -E copy_if_different` housekeeping calls.

- [x] **`set-cache-force`** (this commit) — built-in, LLVM-validated:
      12 raw FORCE events → 4 declaration sites → `INTERNAL` exemption +
      default ignore-patterns (`.*_VERSION.*` absorbs the derived
      `NINJA_VERSION` cache) → 3 final: 1 warning (`LLVM_HAVE_OPENCSD`,
      a derived probe result that should be `INTERNAL`) and 2 notes
      (AddLLVM's empty-docstring "cache as global" idiom, demoted by
      the empty-doc heuristic). Guard idiom (`if(NOT DEFINED X)`)
      demotes to note via AST if-chain containment.
- [x] **`fetchcontent-pinning`** (this commit) — built-in, note
      severity per the FP sketch. LLVM's llvm-subset recording contains
      zero `FetchContent_Declare`/`ExternalProject_Add` events (0 raw →
      0 final); positive/negative variants (branch tag, full-SHA pin,
      URL±URL_HASH) validated on the `hygiene` fixture instead.
      Distinguishes no-GIT_TAG / unpinned-GIT_TAG / URL-without-hash;
      URL+URL_HASH is recognized as pinned.

- [x] **`constant-conditions`** (this commit) — built-in, note severity,
      scoped to the sound subset of the sketch: conditions evaluated >= 3
      times in one recording with identical expanded args *and* identical
      resolved values (loop-/call-invariant conditions). LLVM-validated:
      186 raw multi-evaluation identical-args nodes → 36 final after the
      exclusions (value-level invariance since bare `if(X)` args never
      vary textually; literal-only and never-written-name conditions;
      foreach loop-variable reads; ignore-patterns; try_compile; plus
      three discovered in validation: dynamic dereference of expanded
      variable names, platform pseudo-constants, and undefined-on-every-
      evaluation guard idioms).
      Single-evaluation dead-branch claims are deliberately out of scope:
      the `--also` intersection keys on (rule, file, line) without
      observed values, so they would intersect wrongly across recordings
      that took different branches — future work pends a value-carrying
      intersection key. Cross-recording intersection *raises* confidence
      for the shipped subset (invariant in every config).

#### Tier 2 — small ingestion additions first

- [x] **`configure-warnings`** — surface CMake's own configure warnings
      (dev warnings, policy warnings) as findings with locations, making
      them lintable/gateable/SARIF-visible. Needs: recorder captures
      stderr and ingests parsed warning blocks (new table). FP: none —
      they're CMake's words; value is routing + ratchet.
      *Done*: the recorder now captures configure stderr (forwarded live,
      saved as `configure-stderr.txt` beside the trace); ingestion parses
      CMake's warning/error block format — `at file:line (cmd)`,
      `at file:line`, `in file`, and no-location blocks, tolerant of
      interleaved non-block output — into the new
      `configure_diagnostics` table (schema v3), and the
      `configure-warnings` pass emits one finding per diagnostic with
      CMake's own severity mapping (Error→error, Warning/dev/
      Deprecation→warning, policy→note). Shapes validated against real
      CMake 4.2.1 output; see the user guide's detections reference.
- [x] **`policy-hygiene`** (this commit) — `cmake_policy(SET ... OLD)`
      as tracked tech debt, built-in warning pass with a curated ~15-entry
      policy metadata table (description + introducing version); pairs
      with the `configure-warnings` channel for unset-policy warnings and
      reads nothing from `configure_diagnostics` (pre-v3 recordings work
      unchanged). LLVM-validated: 395 raw OLD-pin events → 0 findings,
      all in CMake's own try_compile scratch configures (project files
      pin only NEW there).
- [x] **`slow-configure` profile** (this commit) — shipped as the
      `cmakedb profile` command (a report, not a lint pass, as
      planned): slowest events by self time, hottest scopes by true
      wall-clock inclusive time (opening→closing event timestamps,
      aggregated by name with call counts/means), and per-command
      self-time totals; text/JSON, `--top N`. The time-to-next-event
      approximation is stated in the output itself; validated on the
      LLVM recording (13.04s span, config-ix / check_symbol_exists /
      try_compile dominate, as expected).
- [x] **`env-dependence`** (this commit) — built-in, note severity.
      Ingestion records `read_kind='env'` rows from a separate
      `env_var_refs` extractor (static_var_refs untouched — its exact
      semantics carry other passes) plus `DEFINED ENV{X}` condition
      tokens, resolved only against prior `set(ENV{X} ...)` writes:
      resolving = project-internal, unresolved = ambient (the signal).
      LLVM-validated: 1082 env reads recorded (CMake's own modules'
      `CFLAGS`/`SDKROOT`/save-restore idioms, resolution verified) → 0
      project-file findings, confirmed correct by source inspection
      (llvm/'s `$ENV{}` uses are install/script-time only). Built-in
      expected-ambient allowlist (PATH/HOME/CC/CI/... + CMAKE_/CI-provider
      prefixes) extended via ignore-patterns; §6.2 snapshot validation
      unaffected.

#### Tier 3 — infrastructure that multiplies every rule

- [x] **Findings baseline / suppression file** (this commit) —
      `lint --write-baseline baseline.json` snapshots the current
      findings; `lint --baseline baseline.json` reports only findings
      absent from it and `fail-on` gates that surviving set
      (finding-level ratchet; the graph-level ratchet remains `diff`).
      Versioned, sorted JSON; keys reuse the `--also` intersection
      identity (rule + declaration file:line). CI flow in user guide §8.
- [x] **SARIF `partialFingerprints` + per-rule `helpUri`** (this
      commit) — every result carries `cmakedbFindingKey/v1` (stable
      FNV-1a over rule+file+line, golden-tested so the algorithm can't
      drift) for GitHub code-scanning dedup across runs; every rule
      links its user-guide FP analysis via the rule-id anchor.
- [x] **Per-rule severity overrides** (this commit) — `[lint.severity]
      set-cache-force = "error"` in `.cmakedb.toml` remaps a rule's
      severity after passes run; rendered output and `fail-on` both see
      the override, so teams tune the gate without forking passes.

## Known gaps / debt

- User passes are SQL-only: the WASM host was removed before the first
  release (see M5). Extensibility beyond a single read-only SELECT per
  pass has no supported path until the WIT ABI lands.
- The VS Code extension is not published: `publisher` is the placeholder
  `"cmakedb"`, deferred pending a Marketplace account (see M5). Install
  it by building a .vsix locally; `vsce publish` will fail until a real
  publisher id is registered.
- MSRV is 1.95, set by `libsqlite3-sys`'s `cfg_select!` use, not by
  anything cmakedb needs. That is a high floor for a CI tool; the
  prebuilt binaries are the mitigation. Revisit if rusqlite's dependency
  relaxes it. Note several dependencies declare a `rust-version` far
  below what they actually compile on, so the floor must be re-derived by
  testing, not read from metadata.
- `custom-commands` config (§2.5) is parsed but not yet consumed by the
  argument parser; wrapper projects rely on `cmake_parse_arguments`
  synthesis instead (works for the common pattern, see fixtures/wrapper).
- Genex-containing link items are stored raw; a `;` inside `$<...>` would
  split incorrectly (rare; noted in `dataflow.rs`).
- Codemod visibility is always PRIVATE (behavior-preserving); PUBLIC
  upgrades are left to `overshare` evidence rather than guessed (§4.6
  step 4 composition is manual for now).
- `elapsed_us` approximates duration as time-to-next-event, which
  includes child-command time for frame openers.
- One recording = one configuration by design (§3.2); multi-config
  analysis is `diff` across presets.
