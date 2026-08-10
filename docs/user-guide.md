# cmakedb User Guide

Everything cmakedb can do, how each detection works, and — most
importantly — **when each detection can be wrong**. Sections here mirror
the rule ids you see in output, so `warning [dead-options] …` can be
looked up directly under [dead-options](#dead-options).

For design rationale see [design.md](../design.md); for one-screen
examples see the [README](../README.md).

---

## 1. The recording model — read this first

cmakedb never parses your intent; it records what one real `cmake`
configure **did** (command trace + File API build graph + your source
ASTs) and answers questions from that recording. This gives it precision
static tools cannot have, at the cost of five blind spots. Nearly every
false positive in this guide is one of these five in disguise:

| # | Blind spot | Consequence |
|---|---|---|
| B1 | **One recording = one configuration.** Branches not taken (other platforms, presets, cache values, `if()` arms) are invisible. | Anything "never read/called/included" may be alive in another configuration. Findings say so explicitly ("in this configuration"). |
| B2 | **Reads happen at trace granularity.** `configure_file`/`string(CONFIGURE)` template substitution (`@VAR@`), generator expressions (evaluated at generate time), and build-time scripts (`cmake -P`, `install(CODE)` with escaped `\${...}`, ExternalProject sub-configures, CTest/CPack) do not produce trace events. | Variables/options consumed *only* through those channels look unread. |
| B3 | **Dynamic variable names.** A read through `${${prefix}_LIBS}` records a read of `prefix` but cannot statically name the outer variable. | The dynamically-named variable's read is not recorded. |
| B4 | **Command results aren't in the trace.** `list(APPEND)`, `find_*`, `execute_process` outputs etc. record the *write* but with an unknown value. | Value-sensitive checks skip those writes rather than guess. |
| B5 | **Textual guards.** Some suppressions (presets mentioning an option, any command mentioning a module's filename) are substring checks. | These bias toward silence: they suppress real findings more often than they create false ones (false *negatives*, not false positives). |

Mitigations that apply across the board:

- **Record per preset and intersect with `--also`.** A finding present
  under *every* configuration you build is trustworthy in a way no
  single recording can be — see [Multi-configuration
  workflows](#multi-configuration-workflows) for the recipe.
  `cmakedb diff` additionally catches build-graph drift between
  recordings.
- **`--capture-scopes`** cross-validates cmakedb's variable-scope replay
  against tables dumped by CMake itself at `project()` calls and
  directory end; any divergence aborts ingestion rather than producing
  silently wrong provenance. Costs ~10–20% configure time.
- **`fail-on` thresholds and per-pass `ignore-patterns`**
  (`.cmakedb.toml`, §2.5 of the design) turn advisory rules into
  zero-noise CI gates at whatever confidence level you choose.

---

## 2. Recording

```sh
cmakedb record -- -S . -B build -G Ninja        # raw cmake args after --
cmakedb record --preset release                 # CMakePresets.json (inherits/macros resolved)
cmakedb record --capture-scopes -- ...          # + scope-replay validation
```

Requires cmake **>= 3.25 as the recording binary** — the version that
matters is the one `cmakedb record` invokes, because it produces the
trace; the project's own `cmake_minimum_required` can be far older (a
3.20-minimum or 2.8-minimum project records fine under cmake 4.x).
Recordings made by a binary too old to emit `global_frame` are rejected
with a clear error instead of producing a silently flat scope replay.

Produces `.cmakedb/trace.db` (SQLite; the whole product is queries over
this file). Recorded per run: every command evaluation with expanded
arguments and location, the runtime scope tree, variable dataflow with
dominating-write resolution, the File API's fully resolved build graph
(targets, link closures, include/define/option fragments per target),
and your parsed source ASTs. `cmake` version, preset, argv and platform
are stored as metadata; findings are always relative to that snapshot.

For `overshare` you additionally need compiler dep files from a **built**
tree: `ninja -C build && cmakedb overshare --deps-from build`.

---

## 3. Questions (provenance and history)

These are queries, not detections — they report recorded facts with a
source location at every hop. Their caveats are shared:

- Single configuration (B1): an edge behind `if(WIN32)` you didn't
  record does not exist here.
- Generator-expression items (`$<...>`) are shown as written; cmakedb
  does not evaluate them (the File API's already-evaluated results are
  shown alongside as "final resolved evidence").
- A `;` inside a `$<...>` in a link list can split incorrectly (rare).
- Path search is bounded (64 paths, depth 32) — pathological graphs get
  a truncated (but correct) answer.

| Command | Answers |
|---|---|
| `why-links <target> <lib>` | Every propagation path from target to library, with the `target_link_libraries`/`set_target_properties` call that created each hop — including hops inside third-party `Find*.cmake` modules. External-library matching is structural (`z` ~ `libz.so` ~ `-lz` ~ `Foo::z`), which can over-match very short names. |
| `why-includes <target> <dir>` / `why-flag <target> <flag>` | Which target declared the requirement, with what visibility, where — plus the propagation path if it arrived transitively, and the File API's resolved value as ground truth. |
| `why-value <var> [--at file:line]` | Full write history: value, location, scope kind, call chain, and how many reads resolved to each write. Values are unknown (B4) for results the trace doesn't expose. `PARENT_SCOPE` writes are shown at the parent scope timestamped at the writing event; a function that reads a variable *after* an inner `PARENT_SCOPE` write may show the new value where real CMake kept the entry-time copy (documented approximation — flagged by `--capture-scopes` validation if it ever matters in practice). |
| `why-not links/target/set <...>` | Why something did **not** happen — candidate sites classified as guard-failed (with condition-value provenance), executed-differently, uncalled function, or never-ran file. See below. |
| `query "<SQL>"` | Raw SQL over the schema (design §4.1). This is also the prototyping surface for user passes. |
| `diff <a.db> <b.db>` | Targets, link edges, and resolved requirement sets added/removed between two recordings. Exit 1 on any difference (ratchet workflows). |

### Explaining a line

`cmakedb explain <file:line>` is the inverse motion of the `why-*`
family: instead of starting from an outcome, start from a source
location and ask "what did this line actually do?"

```sh
cmakedb explain src/app/CMakeLists.txt:2
cmakedb explain cmake/modules/AddLLVM.cmake:1008 --format json
```

The file may be source-relative or absolute (matched with the
recording's own path spelling); a line inside a multi-line command is
remapped to the command's starting line. The report shows, for every
recorded evaluation of the command at that location:

- **Executed as** — the expanded argument values the command actually
  received, collapsed to distinct argument sets with counts (a line
  evaluated 500 times across loop iterations stays readable; long
  values are elided in text, complete in `--format json`).
- **Context** — the call chains (innermost first) the evaluations ran
  under, aggregated with counts.
- **Effects** — what the evaluations produced: variable writes (name,
  value, scope kind), link edges, usage requirements, target
  definitions, and target property writes whose origin is one of these
  evaluations.
- **Influence (one hop)** — for each write, how many reads resolved to
  it and the first few reading locations; for each created edge, the
  File API's final resolved link fragments as ground truth.

If the location has commands in the AST but zero recorded evaluations,
the report says the line **never executed** and shows static context
only — the raw command plus its enclosing `if`/`foreach`/`function`
headers (and whether the whole file went unexecuted) — clearly labeled
as AST evidence, not runtime fact. The section-1 caveats apply
unchanged: one recording is one configuration (B1), and values the
trace doesn't expose are `<not captured>` (B4).

### Asking why something did NOT happen (`why-not`)

The `why-*` family explains recorded facts; `why-not` explains their
absence — usually the question you actually have when a build breaks.

```sh
cmakedb why-not links app zlib        # why doesn't app link zlib?
cmakedb why-not target BrainF         # why was this target never created?
cmakedb why-not set SPECIAL_FLAG      # why was this variable never set?
```

If the thing *does* exist, the answer is a redirect to the matching
`why-*` command. Otherwise cmakedb finds every source site that could
have produced it — link/definition/set commands naming it, plus `add_*`
wrapper calls whose first argument is the target name
(`add_llvm_library`, `add_my_tool`, ...) — and classifies each one
against the recording:

- **guard failed** — the site sits in a branch whose nearest *executed*
  guard chose another way. The guard's observed condition values are
  shown with their provenance, including cache values pre-seeded from
  the command line (`-D`) or a preset:

  ```
  target 'BrainF' was never created. Candidate sites:
    examples/BrainF/CMakeLists.txt:11  add_llvm_example(BrainF ...)
      └─ examples/BrainF/CMakeLists.txt never executed — its directory was never added
         sites that would have pulled it in:
        ...
            CMakeLists.txt:1453  add_subdirectory(examples)
              └─ never executed: guard if(LLVM_INCLUDE_EXAMPLES) chose another branch
                   LLVM_INCLUDE_EXAMPLES = "OFF" (from the command line: -DLLVM_INCLUDE_EXAMPLES)
  ```

- **executed differently** — the site ran but its expanded arguments
  produced something else (a variable expanded to another name). When
  its edges actually went to a *different* target, that is called out
  explicitly ("this call links zlib into other_tool, not app").
- **inside an uncalled function/macro**.
- **file never ran** — with the `add_subdirectory`/`include` sites that
  would have pulled it in classified recursively (two levels deep).

Sites that merely mention the name in other commands are listed as
context, and when nothing mentions it at all you get an edit-distance
suggestion against known names (`did you mean 'zlib_stub'?`).

When the answer can mislead:

- **Site discovery is token-based.** A site that builds the name
  entirely from variables (`add_library(${prefix}_core ...)`) or a
  wrapper that constructs it internally (LLVM's `add_llvm_target`
  prefixes `LLVM`) contains no literal token and is missed — check the
  mentions list, and `cmakedb explain` the suspect line. Conversely,
  matching is word-boundary but not semantic: a same-named directory or
  unrelated string can appear as a candidate.
- **One recording, one configuration (B1).** "Never executed" means
  never executed *in this configure*; record the other preset before
  concluding code is dead.
- **Guard values are trace-resolved.** Values the trace doesn't expose
  are shown as `<undefined>` (B4), and condition provenance covers
  variables, not `EXISTS`/`TARGET`/compiler-probe predicates — the
  guard line itself is still correct.

---

## Exporting the dependency graph

`cmakedb graph` exports the recorded target graph as DOT (default),
Mermaid, or versioned JSON (`cmakedb_graph_version: 1`). Unlike
`cmake --graphviz`, every edge carries its **as-written visibility** and
the **`file:line` of the call that created it** (DOT/Mermaid: in the
edge tooltip / link text; JSON: an `origin` field):

```sh
cmakedb graph > deps.dot                    # dot -Tsvg deps.dot > deps.svg
cmakedb graph --format mermaid              # paste into anything Mermaid-aware
cmakedb graph --format json | jq .edges
cmakedb graph --target app                  # only app's forward closure
cmakedb graph --path src/net --no-external  # your subtree, targets only
```

Reading the output (the legend is repeated in comments at the top of
every DOT/Mermaid export):

- **Edges**: solid = PUBLIC, dashed = PRIVATE, dotted = INTERFACE
  (Mermaid lacks a third dash pattern, so INTERFACE is thick there; the
  visibility is always in the link text too).
- **Nodes**: executables and libraries are distinct shapes; imported and
  interface targets are styled distinctly; **external** destinations —
  names that never resolved to a target in this recording (`z`, `-lssl`,
  `/usr/lib/libm.dylib`) — are distinct nodes, dropped by
  `--no-external`. Alias targets are folded into their real target.
- `--target <name>` keeps the target's *forward* closure, followed
  through every visibility including PRIVATE — it answers "what does X
  depend on", not "what reaches X's link line" (that is `why-links`).
- `--path <prefix>` (repeatable) keeps targets whose *defining location*
  falls under the source-relative prefix, with the same component-wise
  matching as everywhere else (§5); externals have no defining location
  and are kept while an edge into them survives.

The usual recording caveats apply (§1): one configuration per
recording, and generator-expression link items appear as written.

---

## 4. Detections reference

Severity conventions: **warning** = actionable with high confidence in
this configuration; **note** = advisory, evidence attached, expected to
need human judgment. `lint` runs everything enabled and exits 1 at/above
`fail-on` (default `warning`).

For every rule: *what it detects*, *the evidence it uses*, *when it's a
false positive (FP)*, *when it misses (FN)*, and *tuning*.

### dead-options

**Detects**: `option()` and user-facing `set(... CACHE BOOL|STRING|PATH|FILEPATH ...)`
declarations in project files whose variable has **no recorded read**.

**Evidence**: variable dataflow (reads from `${X}` references and bare
identifiers in `if`/`elseif`/`while` conditions, resolved against the
scope replay). Suppressed if the name appears textually in
`CMakePresets.json`/`CMakeUserPresets.json`. If the only references sit
in code that never executed, the finding says so
("read only by code that never executed") and lists those locations.

**False positives**:
- **B1** — the option gates a branch this configuration didn't take
  (`ENABLE_CUDA` on a machine without CUDA). The qualifier catches the
  common shape; a read in a file that was never *included* at all is
  still reported (with the unexecuted-reference note when the reference
  is visible statically).
- **B2** — the option is consumed only by a `configure_file`/
  `string(CONFIGURE)` template (`#cmakedefine ENABLE_FOO`, `@ENABLE_FOO@`),
  a generator expression, `install(CODE "\${...}")`, a `cmake -P`
  build-time script, or an ExternalProject child configure. None of these
  produce trace reads.
- **B3** — read through a computed name (`${${feature}_ENABLED}`).
- The option exists as an interface for *humans or wrapper scripts*
  (read by a Python driver, CI matrix, or documentation) rather than by
  CMake code.

**False negatives**: any executed read counts, including a read that
ignores the value; presets mentioning the name suppress the finding even
if the preset is obsolete (B5).

**Tuning**: `[lint.dead-options] ignore-patterns`; record every preset
and trust only findings present in all of them.

### option-after-use

**Detects**: an `option()`/user-facing `set(... CACHE ...)` declaration
whose variable was already **referenced earlier in the same configure**
— the earlier code ran before the declaration (and its default) existed.
Event ordering in the trace proves the misorder outright.

**Evidence**: executed references to the name before the declaration
event, where the reference observed nothing. Three idioms are excluded
by construction (each validated against LLVM):
resolved references (the cache was pre-seeded via `-D`/preset
`cacheVariables` — the recorder knows these and seeds the replay);
`if(DEFINED N)` probes (they correctly observe *undefinedness*); and the
guard idiom `if(NOT N) <declare N> endif()` (the reference exists to
decide whether to declare). Severity is **warning** when an early
`${N}` expansion baked an empty string into a value (real corruption —
e.g. LLVM's gold-linker search using an empty target triple), **note**
for bare `if(N)` references (fragile ordering, benign while the default
is falsy).

**False positives**:
- A bare identifier in a condition that is really a *string operand*
  coincidentally equal to a later-declared option name
  (`if(x STREQUAL SOME_OPT)` before `option(SOME_OPT ...)`) — rare, and
  note-severity in that shape.
- Code that deliberately treats "empty until declared" as a feature
  (staged includes that tolerate the empty first pass).

**False negatives**:
- References through dynamic names (B3) and template-only consumption
  (B2) are invisible.
- Recording with the option pre-seeded (`-DOPT=ON`) suppresses the
  finding by design — the early read observed the value. Record a
  default (un-seeded) configuration to surface ordering issues.

### undefined-reads

**Detects** (note severity by design): `${X}` expansions in project
files that produced an **empty string because the variable is never
written anywhere in the recording** — with a "did you mean" suggestion
when a defined name sits within edit distance 2 (the typo subclass,
which is the actionable one: `${ENABLE_LOGING}` → `ENABLE_LOGGING`).

**Evidence**: unresolved expansion reads, minus three excluded classes
(each validated on LLVM): names with *any* recorded write (ordering
issues belong to `option-after-use`); the conditional-set-then-use idiom
(a `set(NAME ...)` exists in the AST but its branch didn't execute —
intentional optional values, not typos); and `ignore-patterns` names
(`CMAKE_.*` etc. by default).

**False positives**:
- The **empty-if-unset idiom** for externally-settable variables with no
  in-tree default (`${LLVM_DISTRIBUTION_COMPONENTS}`-style
  configuration surface). The trace cannot distinguish "meant to be
  empty" from "forgot to set" — hence note severity and no gate impact
  at the default `fail-on`.
- Variables set only by mechanisms with untraced writes that our replay
  doesn't model (rare after `cmake_parse_arguments`/parameter
  synthesis, but e.g. `variable_watch` handlers).
- A suggestion can be wrong when two legitimate names differ by ≤2
  edits — it's a hint, not a verdict.

**False negatives**: dynamic names (B3); bare `if(X)` references
(conditions of undefined names are indistinguishable from string
literals and are deliberately not reported here).

### genex-in-wrong-context

**Detects**: a literal `$<...>` generator expression reaching a command
that **never evaluates genexes** — the expression is compared, printed,
written, or executed as raw text at configure time.
`if("$<CONFIG:Debug>" STREQUAL ...)` is the canonical silent bug: the
condition compares the literal string, so the branch outcome is fixed
regardless of configuration. Severity is **warning** for
`if`/`elseif`/`while` conditions and `execute_process` (near-certainly
bugs), **note** for `message`, `file(WRITE|APPEND|READ)`,
`configure_file`, `string(COMPARE)`, and `math`.

**Evidence**: expanded trace args still containing a well-formed `$<...>`
in a curated non-evaluating command list, in project files, outside
try_compile scratch scopes. Genex-aware commands (`target_*`,
`add_custom_command`/`add_custom_target`, `install`, `add_test`,
`set_property`/`set_*_properties`, `file(GENERATE)`, link/include/
compile commands) are never candidates. Two textual idioms are excluded
by construction: escaped `\$<` (the `if(x MATCHES "\\$<")` regex idiom
for *detecting* genexes) and an unterminated `$<` probe string
(`string(FIND "${v}" "$<" ...)`).

**Flow-through exclusion** (the important one): a `set(X "$<...>")` —
or `list`/`string` building — is deliberate genex string-building and
is **never flagged directly**. If the stored value later reaches a
genex-aware sink (`target_include_directories(t PRIVATE ${X})`), that
is exactly what the idiom is for. If it instead reaches a non-evaluating
command, the sink's own expanded args contain the genex, so **the read
site is what gets flagged** (clearer: the finding points at the command
that misuses the text), with the storing `set()` attached as related
evidence via the dataflow tables.

**False positives**:
- Deliberately printing a genex in `message()` output (debug/status text
  that quotes a genex on purpose) — note severity by design.
- `file(WRITE)` of content for a consumer that itself understands
  `$<...>` (e.g. writing a CMake snippet later processed with
  `file(GENERATE)` or an external templater) — rare; note severity.
- Genexes fed through `ignore-patterns` variables (`CMAKE_.*` by
  default) are suppressed as someone else's string-building.

**False negatives**:
- A stored genex with **no resolving reads** in this recording is
  suppressed (fate unknown — it may be consumed by another
  configuration or inside another genex string); LLVM's
  `HandleLLVMOptions` builds exactly such nested-genex fragments.
- Genexes inside a `configure_file`/`file(WRITE)` **template's content**
  are invisible — only command arguments are traced.
- Genexes reaching a non-evaluating command only in an unexecuted branch
  (B1) are not seen.

**Tuning**: `[lint.genex-in-wrong-context] ignore-patterns` suppresses
findings whose genex arrived via a matching variable name.

### execute-process-unchecked

**Detects** (note severity by design): `execute_process()` calls whose
**failure would be silently ignored** at configure time. Two shapes,
with distinguishable messages: no `RESULT_VARIABLE` and no
`COMMAND_ERROR_IS_FATAL` at all ("failure is silently ignored"), and a
`RESULT_VARIABLE`/`RESULTS_VARIABLE` that is written but **never read
afterwards** in this configure ("captured but never checked").
`ERROR_QUIET` is mentioned as aggravating context when present (the
failure would not even print), but is not itself a trigger.

**Evidence**: `execute_process` event args (keyword presence) joined
with the dataflow tables — the ingester synthesizes a write for each
result variable, so "checked" means a read *resolving to that write*
exists at a later event (`if(NOT rv EQUAL 0)`-style conditions count).
`COMMAND_ERROR_IS_FATAL` is recognized as a check. Project files only,
outside try_compile scratch scopes. A source line invoked many times
(helper functions) is reported once, and is considered checked if *any*
invocation's result was read.

**False positives**:
- **Best-effort probing** — the documented FP class and the reason for
  note severity: optional-tool discovery and version sniffing whose
  outcome is judged via `OUTPUT_VARIABLE` content instead of the exit
  code (`xcrun -find`, `tool --version` into a variable that is then
  string-matched). The exit status genuinely goes unchecked, but an
  empty/garbage output usually steers the caller anyway; on LLVM this
  is every surviving finding.
- Commands that cannot meaningfully fail or whose failure is acceptable
  (`${CMAKE_COMMAND} -E`-style housekeeping).
- A result variable "read" only by an untraced consumer (a
  `configure_file` template, a `cmake -P` script) looks unread here.

**False negatives**: a result variable read by *any* later event counts
as checked, even if the read ignores the value or only logs it; calls in
unexecuted branches (B1) are invisible; `check_*` module wrappers run
`try_compile` internally and are out of scope by construction.

**Tuning**: `[lint.execute-process-unchecked] ignore-patterns` matched
against the result-variable name suppresses the captured-but-never-
checked shape.
### constant-conditions

**Detects** (note severity by design): `if`/`elseif`/`while` conditions
evaluated **at least three times in this recording** that never varied —
identical expanded arguments *and* identical resolved variable values on
every evaluation. That is a loop- or call-invariant condition: a check
inside a `foreach`/`while` body or an often-called function/macro that
could not take a different branch no matter how often it ran. The
finding reports the evaluation count and the invariant values observed.

**Evidence**: per-evaluation expanded trace arguments grouped by AST
node, cross-checked at the *value* level against the dataflow replay's
resolved reads — necessary because `if()` auto-dereferences bare
identifiers after argument expansion, so `if(ARG_TARGET)` has
byte-identical trace args on every call even when the branch taken
differs. Excluded by construction: conditions referencing no variables
at all (`if(TRUE)` is deliberate); conditions whose only variables are
never written anywhere in the recording (invariant by necessity — e.g.
string operands of `STREQUAL`) or match `ignore-patterns`; direct reads
of a `foreach` loop variable (its per-iteration values share one
synthesized write, so invariance cannot be proven); try_compile scratch
scopes. Three more exclusions came out of LLVM validation (186 raw
multi-evaluation identical-args nodes → 36 findings): **dynamic
dereference** — an expanded argument naming a variable that `if()`
dereferences again (`if(NOT ${return_var})`), whose second-order value
the replay cannot see and which may vary, is skipped for soundness;
**platform pseudo-constants** (`APPLE`, `WIN32`, `MSVC`, ...) —
invariant by necessity on one host, and written by CMake's platform
modules so the never-written exclusion does not catch them; and
**conditions undefined on every evaluation** — the
fetch-global-into-local idiom (`if(NOT X) get_property(X GLOBAL ...)`
inside a function: the local is *always* undefined at entry) and
assertion guards, and undefined-reads territory besides. A finding
therefore always carries at least one concretely *defined* invariant
value.

**Relationship to `--also`**: cross-recording intersection **raises
confidence** for this rule — a condition invariant across every
recorded configuration is invariant everywhere you build, not just
here. The intersection matches findings by (rule, file, line) without
comparing observed values, which is exactly why *single-evaluation*
dead-branch detection ("this `if()` ran once and took the false
branch") is deliberately out of scope: two recordings that took
*different* branches at the same site would still intersect, producing
a confident-looking but false "constant" claim. Extending the
intersection key with per-finding observed values (so single-evaluation
findings intersect soundly) is future work.

**False positives**:
- Values that *could* differ but happened not to in the recorded
  matrix: a helper always called with the same argument, an
  `if(NOT DEFINED X)` guard always seeded, an invariant `cmake_parse_arguments`
  output. These are true statements about the recording (that is the
  finding) but the condition is not structurally dead — hence note
  severity. Record more presets and intersect to firm them up.
- Assertion guards with a defined invariant value: a check kept as an
  error path (`if(result) message(FATAL_ERROR ...)`) is invariantly
  false in every *successful* configure — true, but intentional.
- A condition kept for future call sites of a shared helper (invariant
  today, load-bearing tomorrow).

**False negatives**: conditions evaluated fewer than three times
(including every single-evaluation dead branch — see above); dynamic
names (B3); conditions whose variables the replay could not resolve on
some evaluation.

**Tuning**: `[lint.constant-conditions] ignore-patterns` (a condition
whose variables all match is skipped; defaults exclude `CMAKE_.*` etc.).
### env-dependence

**Detects** (note severity by design): `$ENV{X}` reads in project files
whose **value came from the ambient environment** — no
`set(ENV{X} ...)` preceded them in this configure. Both expansion reads
and `if(DEFINED ENV{X})` condition probes count. Such reads make the
configure result depend on the machine/shell it ran on — a
non-reproducibility signal. The rule reports the read itself; it does
**not** claim to track what the value went on to influence.

**Evidence**: `read_kind='env'` rows recorded at ingestion from the raw
AST text of joined events, resolved *only* against prior
`set(ENV{X} ...)` writes in the same configure (a resolving read is
project-internal and reproducible, e.g. the save/set/restore idiom).
Reported per environment name: read count, first location, up to 4
further sites. Excluded: try_compile scratch scopes, non-project files
(CMake's own modules read `CFLAGS`, `SDKROOT`, ... constantly), and an
expected-ambient allowlist — `PATH`, `HOME`, `USER`, `TMPDIR`, `TEMP`,
`TMP`, `PWD`, `SHELL`, `HOSTNAME`, `NUMBER_OF_PROCESSORS`, the Windows
`ProgramFiles`/`SystemRoot`/`WINDIR` family, `CC`, `CXX`, `CFLAGS`,
`CXXFLAGS`, `LDFLAGS`, `MAKEFLAGS`, `VERBOSE`, `DESTDIR`, `CI`, and the
`CMAKE_`/`GITHUB_`/`GITLAB_`/`JENKINS_`/`TRAVIS_`/`BUILDKITE_`
prefixes. LLVM-validated: 1082 env reads recorded, all ambient ones in
CMake's own modules; the project files' few `$ENV{}` uses are
install/script-time only, so 0 findings — the correct answer there.

**False positives**:
- **Intentional env-driven CI configuration** — a project that
  deliberately keys behavior off `MY_PROJ_RELEASE_TAG` set by its
  pipeline. That is still a reproducibility hazard for local builds,
  hence note severity; extend the allowlist via
  `[lint.env-dependence] ignore-patterns` for names that are part of
  your contract.
- Probes that only *report* (`message()` diagnostics quoting an env
  var) — the read is real but harmless.

**False negatives**:
- Environment consumed by **child processes** (`execute_process`,
  compiler launchers, `ExternalProject` children inherit the whole
  environment) — only literal `$ENV{X}` reads in CMake code are seen.
- **Dynamic env names** (`$ENV{${v}}`) cannot be named statically (B3).
- Env reads in unexecuted branches (B1) emit no events.

**Tuning**: `[lint.env-dependence] ignore-patterns` extends the
built-in expected-ambient allowlist (full-name match).

### dead-functions

**Detects**: `function()`/`macro()` definitions in project files with
zero invocation events (direct calls, `cmake_language(CALL/DEFER)` are
all counted — if it ran under any spelling, it's live).

**Evidence**: trace events; wrapper indirection is handled for free. A
qualifier is added when calls exist only in never-executed code.

**False positives**:
- **B1** — called only under other configurations.
- **Exported API**: functions defined so *downstream* projects can call
  them after `install()`/`find_package` (the classic `MyProjHelpers.cmake`
  pattern). cmakedb guards installed *modules* but cannot know a
  function is a public API.
- **B2** — called from build-time script mode (`cmake -P`,
  `install(SCRIPT)`), CTest fixtures, or ExternalProject children.

**False negatives**: a function called only by another dead function is
still flagged (that's intended — with the qualifier), but a function
invoked via `cmake_language(EVAL CODE ...)` *is* traced and counted
alive even if the EVAL itself was pointless.

### dead-modules

**Detects**: `*.cmake` files and `CMakeLists.txt` under the source tree
with **zero executed events** ("never included" / "directory never
added").

**Evidence**: file table vs. events. Suppressed when any executed
command's arguments mention the file's basename (covers `install()`,
`configure_file`, list variables), or when presets mention it.
`*.h.cmake`-style configure templates are excluded from consideration
entirely.

**False positives**:
- **B1** — platform/feature-conditional includes (`FindDIASDK.cmake` on
  non-Windows is genuinely *dead here* but not deletable).
- **B2** — modules included only by build-time scripts or child
  configures.
- Modules addressed by a path composed at runtime in a configuration
  that didn't run (the basename guard only sees *executed* arguments).

**False negatives (B5)**: the basename guard is a substring match — a
module named `utils.cmake` mentioned in any argument anywhere is
suppressed, even if that mention is unrelated.

### dead-variables

**Detects**: variables `set()` in project files whose name has **no
recorded read anywhere** (scope-aware: a `PARENT_SCOPE` write read in
the parent counts as read).

**Evidence**: dataflow, name-level. Demoted to *note* when static
references exist in unexecuted code. Writes from `foreach` loop
variables, function-parameter synthesis, and non-`set` commands are not
reported; default `ignore-patterns` hide well-known write-only names
(`CMAKE_.*`, `.*_FOUND`, ...).

**False positives**: the union of dead-options' B1/B2/B3 cases — most
commonly a variable consumed only by `configure_file` templates or
generator expressions, or read under a dynamic name. This is the
noisiest pass by nature; treat *warning*-level results as candidates,
not verdicts.

**False negatives**: a variable that is read once anywhere is never
reported, even if nine of its ten writes are pointless (that pattern is
`clobbers`' job).

### directory-commands

**Detects**: executed directory-scope requirement commands in project
files — `include_directories`, `link_libraries`, `add_definitions`,
`add_compile_definitions`, `add_compile_options` — reported with the
exact targets the recording proves they affect, proposing the
target-scoped equivalent (`target_include_directories(<tgt> PRIVATE …)`
and friends). A call that affects no target at all is called out
separately as a **dead directory command**. Report-only: the mechanical
rewrite (with re-record verification) is `modernize`'s job; this pass
also reports the shapes modernize skips (multi-evaluation, non-`-D`
definitions), so nothing legacy stays invisible.

**Evidence**: events × scope tree × `targets.defined_event`, using
affected-set semantics validated against real cmake (4.2.1):
`include_directories`, `add_definitions` and `add_compile_definitions`
are **retroactive** — they also apply to targets *already created* in
the same directory (LLVM's MCTargetDesc "grab private headers" hack
depends on this) — while `add_compile_options` and `link_libraries`
affect only targets created after the call. Subdirectories snapshot the
parent's directory properties at `add_subdirectory` time, so subtree
targets count only when defined after the command. Excluded from
affected sets: imported/alias/`INTERFACE`/`UNKNOWN` targets, custom
(`UTILITY`) targets (they neither compile nor link), and anything inside
`try_compile` scratch configures. When a subdirectory also creates
targets under the inherited scope, they are counted and listed
explicitly ("N of these are defined in subdirectories") — the proposal
never silently narrows past an inheriting target. Re-evaluations of the
same call site in the same directory (loops, re-included files) fold
into one finding with an evaluation count; for the order-sensitive
commands the set is computed from the first evaluation, which is the
union over all of them.

**False positives**:
- **B1 — one recording, one configuration.** The affected-target set is
  proven only for the recorded configuration: a directory command may
  affect targets that only exist under other presets/platforms, and a
  "dead directory command" may be alive elsewhere. Before acting on
  either claim, record every preset and keep only findings that survive
  the intersection (`cmakedb matrix`, or `lint --also` across
  recordings).
- **The proposal's visibility is a default, not evidence.** `PRIVATE` is
  the behavior-preserving translation (directory commands never
  propagated to consumers); whether some requirement *should* be
  `PUBLIC` is an `overshare`-informed decision.
- **Generated/vendored targets** you don't control can dominate an
  affected list; `ignore-patterns` (matched against affected-target
  names) drops them, and a finding whose every affected target is
  ignored is dropped entirely.

**False negatives**: calls executed inside *function* bodies are not
reported (the site is not a directory-scope call; helper functions
wrapping directory commands — LLVM's `add_llvm_target` style — are
reported only when the enclosing evaluation is a macro/include at
directory scope). A directory command whose only would-be beneficiaries
are excluded target kinds (e.g. only a `UTILITY` target) reports as
dead — true for compilation, but check custom-command usage before
deleting.

**Tuning**: `[lint.directory-commands] ignore-patterns = [...]` on
affected-target names.

**LLVM validation**: 126 raw events of the five commands in the
recording → 13 project-file directory-scope call sites → 13 findings,
all spot-checked genuine: 4 root-level calls affecting all 215 targets
(the `__STDC_*` definition trio and the top-level
`include_directories`), and 9 narrow per-directory calls, including two
retroactive cases (MCTargetDesc's private-header hack, llvm-config's
`CMAKE_CFG_INTDIR` define) that a naive "targets defined after" model
had mislabeled as dead. Validation drove two guard fixes: the
retroactivity semantics themselves, and inheritance classification
across function scopes (targets defined via `add_llvm_component_library`
were misreported as subdirectory-inherited).

### clobbers

**Detects**: two consecutive writes to the same variable in the same
scope where the first write's value was **observed by no read** before
being replaced with a different value. Both locations are reported.

**Evidence**: dataflow with dominating-write resolution. Suppressed when
either write sits inside a `foreach`/`while` body, when both events come
from the same AST node (re-execution, not a second site), when either
value is unknown (B4), or when values are equal.

**False positives**:
- **The default-then-override idiom** — `set(X 0)` followed by a probe
  or platform branch that assigns the real value. The default is
  genuinely unread, but the code is idiomatic, not buggy. This is the
  dominant FP class (LLVM's compiler-flag probes are full of it).
- **Unrecorded reads (B2/B3)** — if the only read of the first value went
  through a template or dynamic name, the first write looks unobserved.
- **Semantically-equal values** — `ON` vs `TRUE` vs `1` count as
  "different".

**False negatives**: real clobbers inside loops are suppressed by the
loop guard; clobbers across scopes (shadowing) are out of scope here
(that's `scope-leaks` / normal scoping).

### policy-hygiene

**Detects** (warning severity): explicit `cmake_policy(SET CMPxxxx OLD)`
pins in project files. Every OLD behavior is deprecated by definition
and carries a removal clock — later CMake releases turn the pin into a
hard configure error (CMake 4.0 removed OLD for every policy introduced
before 3.5). Each pin is tracked tech debt whether or not it currently
works.

**Evidence**: `cmake_policy` events in the trace with exactly the
`SET <policy> OLD` shape, restricted to project files and excluding
CMake's own try_compile scratch configures (which pin policies OLD
constantly, by design). A curated, additive metadata table for ~15
well-known policies (CMP0048, CMP0054, CMP0063, CMP0074, CMP0077,
CMP0091, CMP0135, ...) adds the NEW-behavior description and
introducing CMake version to the message; unknown policy numbers get a
generic message. `cmake_policy(VERSION ...)` interplay is deliberately
not analyzed.

**Division of labor with `configure-warnings`**: `policy-hygiene` owns
explicit OLD pins proven by the trace; `configure-warnings` carries
CMake's own advice about policies that are *not set* ("Policy CMPnnnn
is not set"). The two never overlap — a pinned policy is set, so CMake
emits no unset-policy warning for it — and this rule reads nothing from
`configure_diagnostics`, so it works on recordings made before that
table existed.

**False positives**: essentially none in the mechanical sense — the pin
is real and its behavior is deprecated. A deliberate, comment-justified
OLD pin (e.g. `CMP0135 OLD` to keep reproducible download timestamps)
is still reported on purpose: the justification does not stop the
removal clock, and the finding is the tracking mechanism. Silence a
policy you have consciously accepted with an ignore-pattern.

**False negatives**: OLD behavior reached *implicitly* by a low
`cmake_minimum_required`/`cmake_policy(VERSION)` (unset policies) is
not reported here — CMake's own unset-policy warnings arrive via
`configure-warnings`. Pins in code that did not execute in this
configuration are invisible (trace-based, like every rule).

**Tuning**: `[lint.policy-hygiene] ignore-patterns` matches policy
numbers (e.g. `CMP0135` to accept a specific pin).

### scope-leaks

**Detects**: `set()` executed inside a **macro** body (macros don't
create scopes) landing in the caller's scope. *Note* severity normally;
escalated to *warning* when other code in that caller scope also writes
the same name (a real collision).

**Evidence**: the runtime scope tree — the write's event sits in a macro
scope while its effective scope is the caller's.

**False positives**:
- **Macros whose purpose is writing caller variables** — the
  "return value" pattern (`macro(get_foo OUT)` setting `${OUT}` or a
  fixed name). The leak is the API. The note wording ("consider a
  function() or unset()") assumes hygiene was intended; for deliberate
  APIs, ignore-pattern the variable or the finding.
- The *warning* escalation triggers on any other write of the same name
  in the caller scope, including deliberate sequential reuse.

**False negatives**: leaks via nested macro→macro chains attribute to
the innermost macro only; `PARENT_SCOPE` abuse inside functions is not
this rule's concern.

### overshare

**Detects** (advisory, *note* severity by design): `PUBLIC`/`INTERFACE`
include directories no consumer's translation units actually use —
"consider PRIVATE" when only the owning target's TUs use them,
"consider removing" when nobody does.

**Evidence**: reverse propagation-aware consumer closure over the link
graph, cross-checked against compiler dep files (`ninja -t deps`) —
which headers each TU *actually included* in the built tree. Requires a
completed Ninja build; without dep data the pass emits a single note
saying so instead of guessing.

**False positives** (why it is advisory):
- **Incomplete build** — TUs never compiled (config excluded them, build
  was partial, or a consumer target wasn't built) contribute no header
  evidence and look like non-users. Build everything first.
- **Header need ≠ full need** — a consumer may require the directory at
  *link* time or for ODR reasons without including anything from it, or
  may be about to (the dir is deliberate public API surface for external
  consumers who aren't in this build at all).
- **Path spelling** — matching is by lexical prefix using cmake's own
  path spelling. Generated headers included from the build tree while
  the requirement names the source tree (or unusual symlink layouts)
  break the prefix match and read as "unused".
- **Other configurations (B1)** — consumers that exist only under other
  presets.

**False negatives**: a single header include by any consumer TU
validates the whole directory; per-header granularity is not attempted.

### configure-warnings

**Detects**: CMake's *own* configure-time diagnostics — author/dev
warnings (`message(AUTHOR_WARNING)`, `-Wdev` output), deprecation and
policy warnings, and configure errors — re-surfaced as findings with
locations. Severity follows CMake's label: `CMake Error` → error,
`CMake Warning` / `CMake Warning (dev)` / `CMake Deprecation Warning` →
warning. Unset-policy advice ("Policy CMPxxxx is not set", which CMake
prints under `(dev)`) → note: it is advice about future behavior, not a
present problem, and note severity keeps it visible without tripping
`--fail-on warning` gates.

**Evidence**: the recorder captures the configure's stderr (also saved
verbatim as `configure-stderr.txt` next to the trace), ingestion parses
the warning blocks into the `configure_diagnostics` table, and the pass
reads that table. Blocks with a `file:line` header anchor there; blocks
with no location (e.g. "Manually-specified variables were not used")
report at the `<configure>` pseudo-location.

**False positives**: essentially none — the messages are CMake's own
words, unedited. The value of the rule is routing and ratcheting:
warnings that scrolled past in CI logs become lintable, gateable
(`--fail-on`), and SARIF-visible in code scanning.

**False negatives**: the residual risk is parser coverage of exotic
block shapes. The parser is validated against real CMake 4.x output
(`at file:line (cmd)`, `at file:line`, `in file`, no-location blocks,
`Call Stack` sections, the dev-warning trailer) and skips anything it
does not recognize rather than guessing — so an unrecognized shape drops
that one diagnostic, never the run. Only the first few body lines of a
block are kept as the finding message; read `configure-stderr.txt` for
the full text.
### duplicate-links

**Detects**: the same destination linked to the same target more than
once. Two classes: **conflicting visibilities** (warning — e.g. `PUBLIC`
once and `PRIVATE` once: CMake merges the declarations to their union,
so the dependency is both linked and propagated and the stricter call is
silently subsumed, almost always unintended) and **same-visibility
repeats** (note — pure redundancy, either across calls or duplicate
items inside one expanded argument list). Both origin calls are
reported (primary + related).

**Evidence**: `tgt_edges` grouped by (target, destination). Because
ingestion drops the `debug`/`optimized`/`general` keywords, the pass
re-walks each originating `target_link_libraries` call's recorded
expanded arguments with the same splitting rules to assign every
occurrence a per-config slot — a legitimate `debug X optimized X` pair
occupies two different slots and is **not** a duplicate. Destinations
containing `$<` are skipped (a `;` inside a genex splits incorrectly at
ingest — known gap), as are groups where no participating call is in
project source, plus `ignore-patterns` names.

**False positives**:
- Duplicates deliberately produced by component/dependency-computation
  machinery that tolerates them (LLVM's component resolver emits
  duplicate items in one call — reported as the redundancy *note*, and
  factually true, but nobody is going to "fix" generated lists;
  `ignore-patterns` on the target or library name is the knob).
- A conflicting-visibility pair can be intentional when a build wants
  the dependency on the link line *and* in the interface for different
  reasons — expressing that as a single `PUBLIC` is equivalent, so the
  warning still points at real cleanup.

**False negatives**: duplicates expressed through different spellings of
the same library (`z` vs `libz.so` vs `ZLIB::ZLIB`) are distinct
destinations here; keyword pairs that repeat *within* one config slot in
a call our re-walk could not reproduce (mismatch falls back to treating
rows as all-config, which can under-count per-config repeats).

### redundant-links

**Detects** (note severity, **report-only by design**): a direct link
edge `T -> B` written in project files that is transitively redundant —
another direct dependency `A` of `T` already provides `B` through an
unbroken PUBLIC/INTERFACE chain (`A -> ... -> B`, every hop resolving
to a real target and propagating). The classic shape is
`target_link_libraries(app PRIVATE a b)` where `a` PUBLIC-links `b`.
The message renders the providing chain; related evidence points at the
`T -> A` call and every chain hop's originating call.

**Evidence**: `tgt_edges` — direct candidate edges must resolve to a
real target (`dst_target` non-null) and originate in project files;
the chain search is a breadth-first walk over PUBLIC/INTERFACE edges
with a **depth cap of 8 hops** and a visited-set cycle guard (real
projects have link cycles — see cyclic-links). Skipped by
construction: external/string destinations (never flagged — name
matching over-matches short names like `m`), imported/alias/interface
`T`, object libraries anywhere in the chain (`$<TARGET_OBJECTS>`
semantics differ), genex-bearing (`$<`) as-written destinations,
try_compile scopes, and — LLVM-validated — origin call sites that link
more than one distinct target across the recording: those are
wrapper/loop machinery (`llvm_add_library`-style helpers), where the
flagged line is not where the edit belongs.

**False positives** — why this rule never auto-fixes:
- **Static-archive link ordering / ODR (read this first)**: with static
  libraries the linker resolves symbols in command-line order, and an
  explicit direct edge can be load-bearing — it pins `B`'s position on
  the link line and which archive satisfies a symbol in
  ODR-sensitive setups. Removing a "redundant" edge can turn into
  undefined-symbol or wrong-definition surprises. Prefer keeping
  explicit edges for static link ordering unless you know better; the
  finding message carries the same caveat.
- **Config-dependent genex edges**: an involved edge guarded by a
  generator expression can exist in one config and not another;
  genex-bearing destinations are skipped, but a chain hop whose
  *presence* is config-dependent still makes the recorded chain
  config-specific.
- **Proven for the recorded configuration only (B1)**: the providing
  chain may not exist under another preset — e.g. LLVM's
  `llvm-exegesis` links `LLVMExegesis` directly plus per-target
  `LLVMExegesis<T>` libs that PUBLIC-provide it, but that list is empty
  when no exegesis targets are enabled, and only the direct edge keeps
  the build linking. Intersect with `--also`/`matrix` across every
  recorded preset before acting.
- **Deliberate explicitness**: many teams intentionally declare every
  directly-used dependency instead of relying on a transitive route
  ("link what you use") — depending on `A` to keep providing `B` is a
  refactoring hazard. LLVM's JITLink/OrcJIT explicit link blocks exist
  for exactly this reason (shared-lib configs resolve components
  differently). The rule reports maintenance information, not an order.

**False negatives**: edges applied through shared wrapper helpers or
loops (the multi-target origin-site guard above — on LLVM this is the
component-closure machinery, which links deliberate full static-archive
closures: 1003 of 1019 raw candidates); chains longer than 8 hops;
chains through external or imported providers; duplicate direct edges
beyond the first per (target, dependency) pair (duplicate-links owns
those).

### cyclic-links

**Detects** (note severity by design): circular link dependencies —
strongly-connected components of size ≥ 2 (or self-loops) in the target
link graph, restricted to edges where **both endpoints are real targets
in this build**. CMake permits cycles between static libraries (it
repeats them on the link line), so this is an architecture smell and a
link-time cost, not an error — the message says exactly that. Each
cycle group is reported once, with one concrete shortest cycle rendered
like a `why-links` path and every hop's originating
`target_link_libraries` call as related evidence.

**Evidence**: SCC (Tarjan) over `tgt_edges` rows with a resolved
`dst_target`, per-edge origins from the trace join.

**False positives**:
- Intentional cycles: some codebases genuinely accept mutually-dependent
  static libraries — that is why the severity is note and never gates at
  the default `fail-on`. Use `ignore-patterns` on member target names to
  silence an accepted cycle.
- A cycle can be config-illusory: two edges that never coexist under one
  real configuration (B1) still coexist in one recording's graph if both
  calls executed.

**False negatives**: cycles through external/imported libraries
(unresolved `dst_target`) or through targets that only exist under other
presets (B1) are invisible; components whose members are imported
targets or whose edges all originate outside project source are
deliberately skipped.
### set-cache-force

**Detects**: `set(<var> ... CACHE <type> <doc> FORCE)` in project files
— the write re-runs on **every configure** and overwrites whatever the
user configured (`-D`, ccmake/cmake-gui edits). `INTERNAL` entries are
exempt by construction: they are not user-facing configuration and
FORCE is idiomatic bookkeeping there.

**Evidence**: event args of executed `set` commands (one finding per
declaration site, however many times it re-executed), outside
try_compile scratch configures (their cache is an isolated world).

**Severity**: **warning** for the plain stomp. Two shapes are demoted
to **note** (each present in real code, validated on LLVM):
- the **guard idiom** — the write sits inside an `if`/`elseif` chain
  whose condition mentions the variable (`if(NOT DEFINED X)` /
  `if(NOT X)`), so it only runs while the entry is absent and a `-D`
  provided up front survives (the guard also makes FORCE redundant);
- an **empty docstring** — the "cache entry as global variable" idiom
  (LLVM's `get_host_tool_path` does this deliberately); not user-facing
  in practice, and the fix is to declare it `CACHE INTERNAL`.

**False positives**:
- **Derived values cached for visibility** (a probed version or tool
  path re-cached each configure, e.g. LLVM's `NINJA_VERSION`): nobody
  should be setting these by hand, but the shape is identical to a
  stomp. The rule's advice (use `INTERNAL`) still applies; use
  `ignore-patterns` for ones you want to keep as-is.
- Deliberate migration/sanitization code that *must* override a stale
  user value once (a rename shim). The trace cannot see intent.
- The guard demotion checks the enclosing `if`-chain conditions
  textually; a guard expressed through an intermediate variable
  (`if(NOT probed) set(X ... FORCE)`) stays at warning.

**False negatives**: `set(CACHE)` executed through dynamic dispatch the
trace didn't run (B1: other configurations); a guard on an unrelated
variable that happens to name-mention the target demotes incorrectly
(rare — requires the variable name as a whole token in the condition).

**Tuning**: `[lint.set-cache-force] ignore-patterns` matched against
the **variable name**. Note the default ignore list applies when no
per-pass table is given — it includes `CMAKE_.*` and `.*_VERSION.*`, so
stomps of `CMAKE_BUILD_TYPE`-style variables are only reported if you
configure a narrower pattern list for this pass.

### shadowed-requirements

**Detects** (report-only, *note* severity): the **same** include
directory or compile definition reaching one target through **two or
more routes**, making the narrower declaration redundant maintenance
noise — CMake dedups the compile line, so the build is identical either
way. Routes modeled, per target `T` and value `V`:

1. **Direct** — a `target_include_directories` /
   `target_compile_definitions` call on `T` (any visibility) in a
   project file.
2. **Directory scope** — an executed `include_directories` /
   `add_definitions` / `add_compile_definitions` whose directory scope
   contains `T`, using the affected-set semantics validated for
   [`directory-commands`](#directory-commands) (all three commands are
   retroactive: they also apply to targets already created in the same
   directory; subdirectory targets count only when defined after the
   command).
3. **Transitive** — a PUBLIC/INTERFACE requirement declared on a
   dependency `D` that `T` links via a project-file edge. **One hop
   only** in v1: requirements `D` itself inherits from *its*
   dependencies are not propagated onward, so deeper chains are a
   false-negative class, never a false-positive one.

The primary span is the **most specific** redundant declaration — the
direct `target_*` call when a directory/transitive route also supplies
the value, or the directory command when a transitive PUBLIC route
supplies it — with the other route(s) as "also supplied by …" evidence
spans. Includes are compared in the recording's own path spelling
(never canonicalized) with trailing slashes trimmed; definitions
compare the full `FOO=BAR` token exactly.

**Evidence**: trace `usage_reqs` (as-written rows with visibility and
origin) × the directory-scope attribution model × project-file
`tgt_edges`.

**False positives** (each guarded or documented):

- **Include ORDER matters — the big caveat.** Two directories can
  contain same-named headers, and cmake's dedup keeps the *first*
  occurrence of a repeated dir on the compile line. Removing the
  "redundant" declaration can therefore **reorder includes** and change
  which header wins. The finding message carries this caution; treat
  every include finding as "verify no same-named headers" before
  acting. Relatedly, `SYSTEM`-ness changes warning semantics and search
  order: routes whose declaring calls differ in a literal `SYSTEM`
  keyword are never treated as shadowing (guard implemented); SYSTEM
  applied through other mechanisms
  (`CMAKE_SYSTEM_INCLUDE_PATH`-adjacent property edits) is not
  detected.
- **The standalone-build heuristic (precision/recall tradeoff).**
  Subprojects routinely re-declare parent-supplied requirements so they
  also build standalone, when the parent route doesn't exist. The pass
  therefore only reports when the redundant declaration and the
  shadowing route live in the **same directory's files** (declaration
  in a subdir + directory command in the parent = likely
  standalone-build intent, skipped; same file/directory = likely true
  redundancy). Cross-directory true redundancy is deliberately not
  reported — recall is traded for precision.
- **Generator expressions.** Any value containing `$<` is skipped:
  the value is config-dependent, so redundancy cannot be proven from
  one recording.
- **B1 — one recording, one configuration.** Both routes are proven
  only for the recorded configuration; under another preset one route
  may not exist (and then the "redundant" declaration is load-bearing).
  Only act on findings that survive a `cmakedb matrix` / `lint --also`
  intersection across every recorded preset.
- try_compile scratch configures are excluded on both routes, and
  INTERFACE-only/imported/alias/utility targets are never `T`.

**False negatives**: transitive chains deeper than one hop (above);
cross-directory redundancy skipped by the standalone-build heuristic;
requirements applied via `set_property(TARGET … INCLUDE_DIRECTORIES)`
on the *consumer* side appear only when the trace recorded them as
requirement rows.

**Tuning**: `[lint.shadowed-requirements] ignore-patterns` matched
against the **target name**.

**LLVM validation**: 113 project-file requirement declarations × 13
directory-scope requirement commands × 1406 project link edges → **0
findings**, and an independent SQL sweep confirms 0 raw value
collisions even before any guard: LLVM keeps directory-supplied values
(the root/component `include_directories`, the `__STDC_*` define trio)
disjoint from its per-target declarations (target-local dirs like
`${CMAKE_CURRENT_BINARY_DIR}`, feature defines like `HAS_LOGF128`), and
its interface requirements are genex-wrapped (`$<BUILD_INTERFACE:…>`).
A synthetic redundancy planted in a scratch copy (a direct declaration
of MCTargetDesc's directory-scope include dir) is detected through the
retroactive same-directory attribution, confirming the machinery at
scale.

### single-use-variables

**Detects** (report-only, *note* severity by design): a plain
`set(NAME value)` in project code whose variable is written **exactly
once** and read **exactly once** (via `${}` expansion) in the whole
recording — a candidate for inlining the value at the sole use site.
The message names the read location; the related span points at it.

**Evidence**: dataflow — the name has one write of *any* kind (so
append/build-up patterns are out by construction) and one read, and
that read resolves to that write — cross-checked statically two ways:

- **unexecuted reads**: if `ast_var_refs` holds more distinct
  project-file reference nodes for the name than the executed read
  touched, some branch/configuration this recording didn't take also
  references it — skipped (the B1 guard);
- **unexecuted writes**: an unexecuted project-file `set`/`option`/
  `list`/`string`/... naming the variable as a write target — the
  **conditional-default idiom** (`set(X_default OFF)` +
  `set(X_default ON)` in an untaken branch) — skipped. Bare write
  targets are not variable references, so the first guard cannot see
  them; this was the dominant raw-FP class on LLVM (12 of 17 raw
  findings, all suppressed by this guard).

Writes inside `foreach`/`while` bodies or try_compile scratch scopes
are never candidates, and neither end of the pair may sit inside a
function/macro body — parameters, locals, and set-before-call dynamic
scoping are how CMake functions communicate, not simplification
targets. A read inside a non-project file (e.g. a find module
consuming an input variable) disqualifies too: that value cannot be
inlined. `CMAKE_*`/`_CMAKE_*` names are skipped unconditionally, plus
the ignore-patterns.

**False positives**:
- **Readability-intent variables** — a named source list or documented
  constant (`set(ALLOWED_BUILD_TYPES ...)`) that exists to *explain*
  the value, not to be reused. The rule cannot judge intent; that is
  why it is a note and report-only.
- **Other configurations (B1)** — a branch or preset this recording
  didn't take reads or rewrites the variable. Mitigated by both static
  guards and, across recordings, by the `matrix`/`--also`
  intersection; a reference spelled through a dynamic name
  (`${${prefix}_SRCS}`) is invisible to both and stays a residual
  risk.
- **Generated/template consumers** — values read by code the trace
  cannot see: `configure_file` templates (`*.h.cmake` files are
  configure inputs, not CMake code, and are not parsed), `*.in`
  package-config files, or scripts run at build time. LLVM's
  `PACKAGE_BUGREPORT` is the canonical case: set once, read once in
  CMake, but also substituted into `config.h.cmake` — inlining it
  would break the generated header.

**False negatives**: genuine single-use pairs inside function bodies
or loops (excluded wholesale by the guards above); a pair whose second
*potential* write merely shares a token with an unexecuted
`list`/`string` command (the write-target check matches
conservatively, so such names are suppressed).

**Tuning**: `[lint.single-use-variables] ignore-patterns` matched
against the **variable name**; the default ignore list applies when no
per-pass table is given.

### alias-variables

**Detects** (report-only, *note* severity by design) two shapes of
pass-through variable:

- **Pure alias**: a project-file `set(X ${Y})` whose raw argument text
  is exactly the name plus a single `${Y}` reference, where X is
  written nowhere else in the recording, **every** read of X resolved
  to that write, and Y was never rewritten after the alias event —
  every read of X observed exactly Y's value, so the reads can use Y
  directly. The message names Y and the alias site; related spans list
  the read sites to rewrite.
- **PARENT_SCOPE round-trip**: a `set(V ${V} PARENT_SCOPE)`
  self-assignment inside a function body whose `${V}` read resolved to
  a write *outside* that function invocation — the call wrote back the
  unchanged inherited value, a no-op that can be removed. The related
  span points at the inherited definition.

**Evidence**: dataflow plus a static AST shape check. The alias shape
is verified against the *raw* argument text (`commands` +
`ast_var_refs`): exactly one variable reference and nothing else — so
`set(X ${Y}extra)`, multi-part values, and `CACHE`/`PARENT_SCOPE`
tails never qualify as shape 1. For the round-trip, a `${V}` that
resolved to a write *inside* the function (a computed local, a
synthesized parameter) is a real export and is never flagged; if V was
undefined in the parent, the write *defines* it there and is not a
no-op either. Guards shared with `single-use-variables`: writes inside
loops or try_compile scratch scopes are skipped, a pure alias may not
sit in (or be read from) a function/macro body, read sites must be
project code, and both B1 cross-checks apply — **unexecuted
references** (more static `ast_var_refs` nodes than executed reads)
and **unexecuted write targets** (the conditional-default idiom,
checked for X, for Y, *and* for a round-tripped V, where an untaken
`if(cond) set(V changed)` branch before the export would make the
round-trip load-bearing). A pure alias with zero reads is skipped —
that is `dead-variables` territory, not reported twice.

**False positives**:
- **Renamed-for-API-clarity aliases** — a documented public name
  aliasing an internal one (LLVM's `LLVM_LIBRARY_DIR` for
  `LLVM_LIBRARY_OUTPUT_INTDIR`): the alias *is* the interface, and
  "use Y directly" would leak the internal name. The default ignore
  patterns (`.*_DIR`, `.*_VERSION.*`, ...) absorb the common cases; add
  project-specific public names to `ignore-patterns` for the rest.
- **Other configurations (B1)** — a preset this recording didn't take
  rewrites Y between the alias and a read (or takes the untaken branch
  guarding a round-trip). The static guards catch in-file branches; a
  divergence that lives in an entirely different preset does not exist
  in this recording's AST at all, so only re-recording sees it — trust
  the finding for *this* configuration, and require survival of a
  `matrix`/`--also` intersection before acting across configs.
- **Template/generated-file consumers** — values read by code the
  trace cannot see: `configure_file` templates (`*.h.cmake` inputs are
  not CMake code and are not parsed), `*.in` package-config files, or
  build-time scripts. The single LLVM-validation finding is the
  canonical case: `HAVE_BACKTRACE` is a pure alias of
  `Backtrace_FOUND` for every CMake-visible read, but
  `config.h.cmake` also substitutes it — the reads can switch to
  `Backtrace_FOUND`, the variable itself must stay.

**False negatives**: aliases written or read inside function/macro
bodies (whether Y is visible at the read site is a dynamic-scoping
question this pass doesn't answer); an alias whose source is rewritten
*after the last read* (the divergence guard is deliberately
conservative: any later write of Y disqualifies); directory-scope
`set(V ${V} PARENT_SCOPE)` exports (only function bodies are
considered — propagating a value up a directory is usually
load-bearing).

**Tuning**: `[lint.alias-variables] ignore-patterns` matched against
the **alias name** (X / V); the default ignore list applies when no
per-pass table is given.
### noop-calls

**Detects** (report-only, *note* severity by design): executed
effect-bearing commands that were no-ops in **every** recorded
evaluation because everything beyond their fixed head expanded to
nothing — `target_link_libraries(x ${EXTRA_LIBS})` where the list was
empty each time — plus literally argument-less calls
(`add_definitions()`, `list(APPEND V)` with no items). Covered
commands: the target requirement family (`target_link_libraries`,
`target_include_directories`, `target_compile_definitions`,
`target_compile_options`, `target_sources`, `target_link_options` —
head: the target name plus `PRIVATE`/`PUBLIC`/`INTERFACE` and the
`BEFORE`/`SYSTEM` modifiers), the directory-scope family
(`include_directories` with its `AFTER`/`BEFORE`/`SYSTEM` keywords,
`add_definitions`, `add_compile_definitions`, `add_compile_options`,
`link_libraries` — no head), and `list(APPEND|PREPEND <var>)` (head:
subcommand + list name). Two message shapes: *"… was a no-op in all N
evaluation(s) — ${X} expanded to nothing"* when the call carries `${}`
references, and *"… has no arguments — remove it"* for the literal
case.

**Evidence**: json-v1 trace args are *per-source-argument* expansions —
an unquoted argument that expanded to nothing appears as `''` but the
real command never received it, and the joined AST's argument kinds
disambiguate that from a real quoted `""`. Every evaluation of the node
must be payload-free: a call inside a loop where *any* iteration had
real arguments never flags, while all-iterations-empty still does (the
count strengthens it — LLVM's finding was empty in all 213
evaluations). Belt-and-braces, no `tgt_edges`/`usage_reqs`/`tgt_props`
row may point back at the call's events (skipped for `list()`: an empty
`APPEND` still records a value-preserving write of the list variable —
the empty-items check is the authority there). try_compile scratch
scopes are skipped. When the recording can tell *why* the variable was
empty, the message says so: never written anywhere → *"is never set —
see undefined-reads"*; written only by code that never executed →
*"populated only in code that never ran (B1)"*.

**The quoted-argument choice**: a quoted empty argument —
`target_compile_definitions(t PRIVATE "")`, or `"${X}"` where `X` is
empty — is a **real** (empty-string) argument by the trace format's own
rules and is never flagged; writing the quotes is read as deliberate.
Consequence observed on LLVM: `target_link_libraries(LLVMDebugInfoPDB
INTERFACE "${LIBPDB_ADDITIONAL_LIBRARIES}")` (populated only with the
Windows DIA SDK) is a documented FN in non-Windows recordings because
of the quotes.

**False positives**:
- **Other configurations/presets (B1)** — *the* dominant class: the
  list is populated only under a preset/platform this recording didn't
  take (sanitizer flags, MSVC-only definitions). The enrichment tags
  the visible cases (an unexecuted writer in the tree ⇒ the "(B1)"
  note), but a variable populated by an untraced toolchain file or
  cache preset looks identical to dead code. Before acting, intersect
  across recordings: `matrix` or `lint --also` keeps only findings
  empty in **every** preset.
- **Intentional extension-point variables** — a documented
  empty-by-default hook (`${PROJECT_EXTRA_LIBS}` meant to be filled via
  `-D` or a vendor overlay). The recording cannot see the convention;
  suppress by name with `ignore-patterns`.
- **Genex-only arguments** — generator expressions are opaque
  non-empty text at configure time, so a call whose argument is
  `$<...>` never flags here even when the genex evaluates to nothing at
  generate time. This keeps genex-conditional calls from being
  mislabeled as no-ops (the missed truly-empty genex is an FN, not an
  FP — see `genex-in-wrong-context` for genex placement issues).

**False negatives**: the quoted-empty class above; calls whose
remaining arguments are only non-head specifiers this pass doesn't
model (`debug`/`optimized` link prefixes); `list()` subcommands other
than `APPEND`/`PREPEND`. An empty directory-scope call also surfaces
via `directory-commands` as a dead directory command — the two rules
agree, from different evidence.

**Tuning**: `[lint.noop-calls] ignore-patterns` matched against the
**expanded-to-nothing variable names** (and the list variable name for
`list()`); the default ignore list applies, so `${CMAKE_*}` expansions
never flag without opting in.

### fetchcontent-pinning

**Detects** (advisory, *note* severity by design — tracking a branch is
a legitimate dev-mode choice): `FetchContent_Declare` /
`ExternalProject_Add` whose source is not reproducibly pinned. Three
variants, distinguished in the message:
- `GIT_REPOSITORY` with **no `GIT_TAG` at all** — tracks the remote's
  default branch;
- `GIT_TAG <x>` that is **not a full 40-hex commit hash** — branches,
  short SHAs, and tags can move or be rewritten (supply-chain surface);
- `URL <archive>` with **no `URL_HASH`/`URL_MD5`** — contents are not
  integrity-checked. A `URL` + `URL_HASH` flow **is** pinned and never
  flags.

**Evidence**: event args of executed declarations in project files (one
finding per declaration site), outside try_compile scopes. Declarations
using neither `GIT_REPOSITORY` nor `URL` (`SOURCE_DIR`, SVN/CVS/hg,
custom download commands) are out of scope.

**False positives**:
- Intentional tracking during development (nightly bots pointed at
  `main`) — that's why it's a note.
- A repo whose tags are treated as immutable by policy: CMake cannot
  verify that; the rule still asks for the commit hash.
- `GIT_TAG ${VAR}` resolved from a pinned manifest elsewhere — the rule
  judges the *expanded* value, so this only flags when the value really
  isn't a hash.

**False negatives**: declarations in code that didn't execute this
configure (B1); `FetchContent_MakeAvailable` of a name declared by a
dependency outside the project tree; a 40-hex `GIT_TAG` that is not
actually a commit in the repository (shape check only).

**Tuning**: `[lint.fetchcontent-pinning] ignore-patterns` matched
against the **dependency name** (first argument).

### modernize-* (modernize --check / --fix)

**Detects**: directory-scope legacy commands — `include_directories`,
`add_definitions`, `add_compile_options`, `link_libraries` — and plans
their replacement with target-scoped `PRIVATE` calls on exactly the
targets the recording proves were affected (File API cross-check filters
shadowed/no-op cases). Calls that ran multiple times with different
arguments, ran inside functions/macros, or carry non-`-D` definition
arguments are *skipped with a note* rather than rewritten.

**The safety story is different from every other rule**: `--fix` is not
trusted statically. It applies the patch, **re-configures, and asserts
the build graph is isomorphic** (same targets, same resolved requirement
sets); any difference rolls back every edit and reports the delta. What
survives `--fix` has been mechanically verified — *for this
configuration*.

**Residual false-positive/risk surface**:
- **B1** — a directory command may affect targets that only exist under
  other configurations; the rewrite covers the recorded ones, and the
  verification loop can only vouch for the recorded configuration.
  Re-record your other presets after fixing.
- "No observable effect — candidate for manual removal" notes inherit
  all of B1: the command may matter elsewhere. That's why they're notes
  with no auto-fix.
- The isomorphism check covers targets and resolved compile/link
  requirement sets; it does not model custom commands, install rules, or
  test properties (the shipped codemods cannot affect those, but keep it
  in mind when judging verification output).
- Rewrites always use `PRIVATE` (the behavior-preserving translation —
  directory commands never propagated). If a consumer *should* see the
  requirement, that's a separate `overshare`-informed decision.

### User passes (SQL)

`.cmakedb/passes/*.sql` files run as lint passes with your rule semantics
— their false-positive profile is whatever your query encodes. Three
properties cmakedb enforces: each pass is a **single** statement, it runs
under SQLite's `query_only` pragma so it cannot modify the recording, and
malformed result shapes fail the lint run loudly rather than being
dropped.

#### Trust: whose code runs when

User passes are supplied by the tree being analyzed, so it is worth being
precise about what executes at each stage.

| Step | Executes project code? |
|---|---|
| `cmakedb record` | **Yes** — it runs real `cmake`, which runs `CMakeLists.txt`, modules, and any `execute_process`/`FetchContent` they invoke |
| `lint`, `why-*`, `dead`, `graph`, `deps`, `explain`, `diff`, `profile`, `query`, `lsp` | No — these read only the recorded database |
| `.cmakedb/passes/*.sql` | Runs SQL from the analyzed tree, single-statement and read-only |
| `modernize --fix` | Re-runs `cmake` to verify its own patches |

Recording an untrusted repository is exactly as dangerous as configuring
it by hand — cmakedb adds no sandbox and claims none. Do it in a
container or throwaway VM, the same way you would treat
`./configure && make` on unfamiliar code.

Analysis is the safe part: passes are pure functions over the database
with no filesystem or process access, so analyzing a recording of code
you do not trust is fine. The one thing a hostile repository can still do
is ship a user pass that makes your lint output say whatever it likes, or
that burns CPU. To exclude them:

```sh
cmakedb lint --no-user-passes        # built-in passes only
cmakedb matrix --no-user-passes
```

This is a command-line flag rather than a `.cmakedb.toml` setting on
purpose: the config file lives in the repository under analysis, so a
setting there could never be trusted to turn anything off.

WASM user passes are **not currently supported**. An experimental host
existed but was removed before the first release: it carried a full JIT
runtime (about a third of the dependency tree) and an ABI that was always
going to be replaced by the component model. The capability is expected
to return against a WIT interface — see ROADMAP.md.

---

## 5. Focusing on a subdirectory

Large projects produce large finding lists (LLVM: 450+). Three ways to
narrow to the part you own, from simplest to most flexible:

1. **`--path` (recommended)** — every finding-producing command accepts
   repeatable source-relative prefixes; `lint`'s `fail-on` exit code then
   applies to the filtered set only:

   ```sh
   cmakedb dead options --path src/net
   cmakedb lint --path src/net --path cmake/modules --fail-on warning
   cmakedb clobbers --path src/net
   cmakedb modernize --check --path src/net   # --fix only touches legacy
                                              # commands under the prefix
   ```

   Matching is by path component (`src/net` matches `src/net/…` but not
   `src/nettle/…`). Two things to know:
   - Filtering is by where the finding is **reported** (the declaration
     site). A dead option that gates only your subdirectory but is
     *declared* in a top-level `cmake/options.cmake` will not appear
     under `--path src/net` — declaration sites are where you would act,
     so this is usually what you want, but check the top-level once.
   - Related/evidence locations attached to a kept finding may point
     outside the filter; they are context, not findings.
   - For `modernize --fix`, insertions can land outside the prefix when a
     filtered directory command affects targets defined in child
     directories — the verification loop covers the whole graph either
     way.

2. **SQL, ad hoc** — arbitrary predicates over the schema:

   ```sh
   cmakedb query "SELECT f.path, e.line, json_extract(e.args_json,'\$[0]')
                  FROM events e JOIN files f ON f.id=e.file_id
                  WHERE e.cmd_lower='option' AND f.path LIKE '%/src/net/%'"
   ```

3. **A scoped SQL pass** — encode the team's ownership boundary once in
   `.cmakedb/passes/net-dead-options.sql` and it runs (and gates) in
   every lint, with its own rule id.

---

## Profiling the configure

`cmakedb profile` reports configure-time hotspots from the timings
already in the recording — no re-run needed, and deliberately a report
rather than a lint pass (timing findings would be noise in a gate):

```sh
cmakedb profile                       # top 15 rows per section
cmakedb profile --top 30 --format json
```

Three sections: the slowest individual commands by self time (leaf
outliers like `execute_process`, `file(GLOB_RECURSE)`, `find_path`), the
hottest scopes by **inclusive wall time** — functions, macros, includes,
directories — aggregated by name with call counts and means (a function
called 243 times reports one row), and total self time per command name
(surfacing "14,000 cheap `string()` calls" patterns). The header states
the total configure span.

Accuracy caveat (also printed in the output): per-event self time is the
trace's time-to-next-event. For a frame-opening command — a function
call, `include`, `add_subdirectory` — that covers only the latency until
the first body command, not the body itself. Use the scopes section for
real function/include/directory costs: those are computed from the
scope's opening and closing event timestamps, so they are true
wall-clock spans. Relatedly, the last command before a `try_compile`
returns (often a `target_link_libraries` in a scratch `CMakeLists.txt`)
absorbs the nested compiler invocation's wait into its self time.

### Flamegraph export

The recorded scope tree is a call tree with wall-clock spans, so it
exports directly as folded stacks — the format consumed by
flamegraph.pl, [inferno], and [speedscope]:

```sh
cmakedb profile --flamegraph > configure.folded
inferno-flamegraph configure.folded > configure.svg   # or flamegraph.pl
# or drag configure.folded into https://www.speedscope.app
```

Each line is `frame;frame;frame N`: the stack is the scope's ancestor
chain from `root`, each frame is `kind:name` (`function:add_library_x`,
`include:config-ix`, `directory:src/net`), and the value is that scope's
**exclusive** time in integer microseconds — its inclusive span minus
its children's spans — because flamegraph tools sum inclusive time up
the stack themselves. The root line carries the residue (top-level leaf
commands), so the values sum to the total configure span. Names are
source-relative where possible and build-dir paths render as
`<build>/...`; scopes with a missing endpoint are skipped, as in the
report. `--flamegraph` is text-only (`--format json` is rejected).

[inferno]: https://github.com/jonhoo/inferno
[speedscope]: https://www.speedscope.app

### Comparing recordings

`profile --compare` turns two recordings into a configure-time
regression report — record before and after a change (or in CI against
the main branch's recording) and diff the scope costs:

```sh
cmakedb --db new.db profile --compare old.db
cmakedb --db new.db profile --compare old.db --top 30 --format json
```

Scopes are aggregated by `(kind, name)` and aligned by name — function,
macro, and include names plus source-relative directory paths are stable
across recordings of the same tree (build-dir paths normalize to
`<build>/...`), so different build or checkout directories don't
misalign. Rows are sorted by absolute inclusive-time delta (`primary -
other`, positive = the `--db` recording is slower), show both totals and
call-count changes, and mark scopes present in only one recording as
`appeared`/`disappeared`. The header carries the total-span delta;
`--top` limits the row count and `--format json` emits the versioned
report for scripting a CI floor (e.g. fail when `span_delta_us` exceeds
a budget).

---

## 6. Multi-configuration workflows

Blind spot B1 (one recording = one configuration) is the dominant source
of dead-code false positives: an option is "unread" here but gates the
Windows build, the CUDA build, the sanitizer preset. The systematic
answer is to record every configuration you actually build and only
trust findings that survive **all** of them.

```sh
# 1. One recording per preset, at distinct paths (same checkout!):
cmakedb record --preset linux-release --db .cmakedb/linux.db
cmakedb record --preset macos-debug   --db .cmakedb/macos.db
cmakedb record --preset full-featured --db .cmakedb/full.db

# 2. Intersect: report only findings present in EVERY recording.
cmakedb --db .cmakedb/linux.db dead options         --also .cmakedb/macos.db --also .cmakedb/full.db
cmakedb --db .cmakedb/linux.db lint         --also .cmakedb/macos.db --also .cmakedb/full.db         --fail-on warning              # gate applies to the intersection
```

Surviving findings are annotated `[present in all N recordings]`. An
option dead across a well-chosen preset matrix has had every `if()` arm
you ship actually executed against it — the single-configuration caveat
is discharged by construction, not by hope. `--also` composes with
`--path`, works for every finding command (`dead *`, `clobbers`,
`scope-leaks`, `overshare`, `lint`), and intersects by (rule,
declaration site).

**`cmakedb matrix` automates exactly this recipe:**

```sh
cmakedb matrix                          # every non-hidden configure preset
cmakedb matrix --preset linux-release --preset full-featured
cmakedb matrix --fail-on warning --path src/net
```

With no `--preset`, every non-hidden configure preset from
`CMakePresets.json` / `CMakeUserPresets.json` is recorded (hidden
presets are inheritance bases, not configurations). Presets record
sequentially — parallel configures of one source tree can collide — into
`.cmakedb/matrix/<preset>.db`, building in each preset's own `binaryDir`
(a preset that declares none gets `.cmakedb/matrix-build/<preset>`). A
preset that fails to configure is reported in the per-preset summary
table (on stderr) and the rest still record; `--fail-fast` stops at the
first failure instead. Then all enabled lint passes run on the first
successful recording intersected with every other successful one — the
same identity and `[present in all N recordings]` annotation as `--also`
above. `--fail-on` applies to that intersection, and the exit code is
non-zero if any preset failed to record. For `--format json|sarif`, pass
`--output <file>`: stdout also carries cmake's own configure output.

What it does and doesn't do:

- **Same checkout required.** Findings intersect by file + line; record
  all presets from the same commit or lines won't align.
- **It cannot discharge B2/B3** (template-only reads, dynamic names): a
  `#cmakedefine`-only option is "dead" in every recording. The
  intersection removes *configuration* false positives specifically.
- **Include the maximal preset.** The matrix is only as good as its
  coverage — an `-DEVERYTHING=ON` preset is the cheapest way to execute
  the branches your CI never builds.
- The un-intersected per-config view stays useful for "dead *in this
  configuration*" cleanup (e.g. pruning a platform-specific tree).

For graph-level drift between two recordings (targets, edges, resolved
requirements) use `cmakedb diff a.db b.db` — exit 1 on any difference
makes it a CI ratchet.

---

## 7. In-editor (LSP and VS Code)

`cmakedb lsp` serves the recording to any LSP client; `editors/vscode/`
adds the "CMake Provenance" tree view. Everything is **post-mortem**:

- Diagnostics are the lint findings above (same FP analysis applies) and
  are prefixed `[stale recording]` the moment your buffer diverges from
  the recorded content — re-run `cmakedb record` after CMake edits.
- Hover/definition answer from the recording; a symbol added since the
  recording has no answers yet.
- Modernize quickfixes are offered **only** while the buffer matches the
  recording byte-for-byte, so stale offsets can never be applied. The
  code-action path applies edits without the re-record verification loop
  — prefer `cmakedb modernize --fix` when you want the guarantee.

---

## Dependency inventory (SBOM)

`cmakedb deps` prints the complete external-dependency inventory of the
recorded configure — every `find_package`, `FetchContent_Declare`, and
`ExternalProject_Add` the configure actually evaluated in project files
(try_compile scratch probes excluded), with how each one resolved. The
configure trace is the only place this inventory is complete *and*
exact: static scanners miss conditionally-executed declarations and
can't see resolution results.

```sh
cmakedb deps                                     # table grouped by mechanism
cmakedb deps --format json                       # versioned cmakedb schema
cmakedb deps --format cyclonedx > sbom.cdx.json  # CycloneDX 1.5
```

- **find_package** entries carry the request (version, components,
  REQUIRED/QUIET — merged across calls) and the resolution read back
  from the recording: a truthy `<Pkg>_FOUND` variable write after the
  call marks the package found (find modules and FPHSA write it in both
  original and all-uppercase spellings; matched case-insensitively),
  the resolved version comes from `<Pkg>_VERSION` /
  `<Pkg>_VERSION_STRING` writes, and a location hint from `<Pkg>_DIR` /
  `<Pkg>_CONFIG` cache writes. Caveat: config-mode packages get
  `<Pkg>_FOUND` set inside the cmake binary rather than by traced CMake
  code, so a found config package whose config file writes no variables
  of its own can be misreported as not found ("no `<Pkg>_FOUND` write"
  is stated as the evidence in that case); in practice config files
  write version/target variables and resolve correctly.
- **FetchContent / ExternalProject** entries carry the declared source
  and a pinning class — the same classification as the
  [fetchcontent-pinning](#fetchcontent-pinning) lint: `pinned-commit`
  (full 40-hex `GIT_TAG`), `mutable-ref` (branch/tag), `default-branch`
  (no `GIT_TAG`), `hashed-archive` (`URL` + `URL_HASH`/`URL_MD5`),
  `unpinned-url`, or `unknown-source` (SVN/CVS/`SOURCE_DIR` flows).
- **Imported targets** (`Foo::bar`) are grouped as resolution evidence
  under the find_package matching their namespace (case-insensitive);
  the rest are listed separately for manual mapping.

The CycloneDX export is a valid 1.5 BOM: the project is
`metadata.component`, each dependency a `library` component, with the
cmakedb-specific facts (mechanism, found status, pinning, declared
`file:line`) carried as `properties`. A `purl` is emitted only when it
can be derived without guessing — a `github.com` `GIT_REPOSITORY`
pinned to a full commit SHA becomes `pkg:github/<owner>/<repo>@<sha>`;
nothing else gets one. Not-found packages are included (with
`cmakedb:found = false`) because the misses are exactly what CI review
wants to see; filter them out (e.g. with `jq`) if a strict
"what-shipped" SBOM is required.

---

## 8. CI recipes

```yaml
- run: cmakedb record --preset ci
- run: cmakedb lint --format sarif --output cmakedb.sarif   # exit 1 at fail-on
- uses: github/codeql-action/upload-sarif@v3
  with: { sarif_file: cmakedb.sarif }
```

SARIF results carry `partialFingerprints` keyed on (rule, file, line),
so GitHub code scanning tracks a finding as *one* alert across runs even
when its message wording changes, and each rule's `helpUri` links to its
entry (and FP analysis) in this guide.

**Finding-level ratchet** (fail PRs only on *new* findings): commit a
baseline from main, gate PRs on findings absent from it.

```sh
# On main (on merge, or nightly): snapshot today's findings and commit.
cmakedb record --preset ci
cmakedb lint --write-baseline cmakedb-baseline.json
git add cmakedb-baseline.json

# On PRs: only findings NOT in the baseline are reported and gated.
cmakedb record --preset ci
cmakedb lint --baseline cmakedb-baseline.json --fail-on warning
```

The baseline stores finding identities as (rule, file, line) — the same
key as the `--also` intersection (§6) — in versioned, sorted JSON, so
regenerating it produces stable diffs and message rewording never
resurrects a suppressed finding. Two consequences of keying on lines:
record PR and baseline from the same file states (a CI job that records
the PR checkout does this naturally), and unrelated edits that shift
lines can surface old findings as "new" — refresh the baseline from main
when that happens. `--baseline` filters every output format, so to keep
the pre-existing debt visible upload SARIF from a lint run *without*
`--baseline` (as above) and add a second, gating run with it.
`--write-baseline` snapshots the post-`--path`/`--also` set, so a scoped
baseline ratchets just that scope.

**Graph-level ratchet**: commit a baseline `trace.db` from main, then
`cmakedb diff baseline.db current.db` — exit 1 on any
target/edge/requirement change.

**Tuning the gate**: set `fail-on = "error"` to keep advisory rules
(`overshare`, notes) out of the gate while still surfacing them in
SARIF, and remap individual rules in `.cmakedb.toml` — overrides change
both the rendered severity and what `fail-on` sees:

```toml
[lint.severity]
overshare = "error"      # promote an advisory rule into the gate
clobbers  = "note"       # demote a rule your codebase can't fix yet
```

Per-preset recording is the systematic answer to B1: run the matrix and
gate on the `--also` intersection (§6).
