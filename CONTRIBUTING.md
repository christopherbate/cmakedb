# Contributing to cmakedb

Thanks for looking. This is a personal project, so response times vary —
but issues and pull requests are welcome.

## Before you start

For anything larger than a bug fix, open an issue first. cmakedb has a
written design (`design.md`) and a status document (`ROADMAP.md`) that is
the single source of truth for what is done and what is not; a feature
that contradicts the design is a design revision, and it is cheaper to
discuss that before the code exists.

## Building and testing

```sh
cargo build --workspace
cargo test  --workspace     # needs cmake >= 3.25 on PATH; one test also needs ninja
```

The test suite records **real** `cmake` configure runs — fixtures under
`fixtures/` are staged into temporary directories and configured for
real, so tests take a few seconds each and cmake must be installed.

Run the same gates CI does before pushing:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo deny check                                   # licenses + advisories
```

`cargo deny` needs `cargo install cargo-deny --locked`.

## Ground rules from the architecture

These are the constraints most likely to trip up a first patch. `AGENTS.md`
has the full set plus hard-won empirical facts about CMake's trace format.

- **Never reimplement CMake.** Every semantic fact comes from a real
  configure run. If an analysis needs information the trace and File API
  do not contain, the answer is to record more, not to interpret
  `CMakeLists.txt` statically.
- **Analysis passes are pure functions over the database.** No filesystem
  or process access inside a `Pass::run`. Anything needing the filesystem
  happens at ingest time or in the CLI.
- **Layering is strict.** Lower layers must not know about analyses.
- **Every detection ships with its false-positive analysis.** A lint pass
  is not done until `docs/user-guide.md` documents when it can be wrong.
  This is the project's main quality bar — findings a user cannot trust
  are worse than no findings.
- **Docs update in the same commit as the change.** See
  `docs/documentation-policy.md`: every fact has exactly one home, and a
  doc that can drift is a doc that will lie.

## Adding a lint pass

The cheapest path is to prototype as a SQL pass (`.cmakedb/passes/*.sql`)
against a real recording, validate the false-positive rate on a large
project, and only then promote it to a built-in. Existing passes in
`crates/cmakedb-passes/` are the template, and each has a fixture under
`fixtures/` encoding the phenomenon it detects.

## Commits and pull requests

Write commit messages that explain *why*, not just what. The existing
history is the style guide: a summary line, then prose covering the
reasoning and any validation performed.

Keep PRs focused on one change. Green CI is required.

## Licensing of contributions

cmakedb is dual licensed under MIT and Apache-2.0. Per Apache-2.0
section 5, any contribution you intentionally submit for inclusion is
dual licensed the same way, with no additional terms. There is no CLA to
sign.
