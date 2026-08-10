# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/).

`ROADMAP.md` remains the single source of truth for milestone status and
known gaps; this file records what changed between releases.

## [Unreleased]

First public release preparation. cmakedb was developed privately before
this point, so there is no prior public release to compare against — the
list below describes the state at first publication rather than a delta.

### Added

- Execution-trace recorder (`cmakedb record`): wraps a real `cmake`
  configure, capturing the json-v1 `--trace-expand` stream and the File
  API, with optional scope snapshots (`--capture-scopes`) that
  cross-validate the replay.
- Semantic database joining the trace with lossless tree-sitter ASTs and
  a replayed scope/dataflow model, stored as a single SQLite file.
- Provenance: `why-links`, `why-includes`, `why-flag`, `why-value`, and
  negative provenance via `why-not`.
- Dead-code detection (`dead options|functions|modules|variables`),
  `clobbers`, `scope-leaks`, and `overshare`.
- 24 built-in lint passes, with a documented false-positive profile for
  every detection in `docs/user-guide.md`.
- Reporting and CI integration: text/JSON/SARIF output, `--fail-on`,
  `--path` filtering, multi-configuration intersection via `--also`,
  finding-level baselines, and per-rule severity overrides.
- Additional commands: `explain`, `graph`, `deps` (with CycloneDX
  export), `profile` (including flamegraph export and recording
  comparison), `matrix`, `diff`, and `query`.
- Verified codemods: `modernize --check|--fix`, where `--fix` re-records
  and auto-reverts if the build graph changes.
- Post-mortem language server (`cmakedb lsp`) and a VS Code extension
  under `editors/vscode/`.
- User passes: `.cmakedb/passes/*.sql` run as lint passes, constrained to
  a single read-only statement.
- `--no-user-passes` on `lint` and `matrix`, to run only built-in passes
  when analyzing a repository you do not trust.
- Dual MIT/Apache-2.0 licensing, and a `deny.toml` license and advisory
  policy enforced in CI.

### Removed

- The experimental WASM user-pass host. It required a full JIT runtime
  (about a third of the dependency tree) and carried 16 open RUSTSEC
  advisories against wasmtime 27, including sandbox escapes, in exchange
  for a capability close to what SQL passes already provide. WASM passes
  are expected to return against a WIT/component-model interface; see
  `ROADMAP.md`.

### Known limitations

- Windows support is experimental: its CI leg is non-blocking and
  prebuilt binaries ship for Linux only.
- One recording describes one configuration by design; multi-config
  analysis is the `--also` intersection or `cmakedb matrix`.
- See `ROADMAP.md` for the full known gaps and debt list.
