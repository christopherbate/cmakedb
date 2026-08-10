# Security Policy

## Reporting a vulnerability

Please report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/christopherbate/cmakedb/security/advisories/new)
rather than opening a public issue.

Include the cmakedb version (`cmakedb --version`), the CMake version used
for the recording, and a reproducer if you have one. Expect an initial
response within a week. This is a personal project maintained in spare
time; there is no paid support and no bounty.

## What cmakedb executes

Understanding the trust model matters more than most tools, because
cmakedb's whole design is to run a real build configuration.

**`cmakedb record` runs `cmake`, which runs arbitrary code.** CMake
executes the project's `CMakeLists.txt`, its modules, and any
`execute_process`/`FetchContent` those invoke. Recording an untrusted
repository is exactly as dangerous as configuring it by hand — cmakedb
adds no sandbox and claims none. Treat `cmakedb record` on unfamiliar
code the way you would treat `./configure && make`: do it in a container
or a throwaway VM.

**Analysis commands do not execute the project.** Everything else
(`lint`, `why-*`, `dead`, `graph`, `deps`, `explain`, `diff`, `profile`,
`query`, `lsp`) reads only the recorded SQLite database. Analysis passes
are pure functions over that database with no filesystem or process
access, so they are safe to run against a recording of code you do not
trust. The exception is `overshare --deps-from`, which reads dependency
files from an already-built tree, and `modernize --fix`, which re-runs
`cmake` to verify its own patches.

**User passes are code supplied by the analyzed tree.** `.cmakedb/passes/*.sql`
files are discovered automatically and run as lint passes. They are
constrained — each pass is a *single* statement, executed under SQLite's
`query_only` pragma, so it can neither modify the recording nor chain an
`ATTACH` onto a `SELECT` — but a hostile repository can still make your
lint output say whatever it likes, and can spend your CPU. When analyzing
a repository you do not trust, pass **`--no-user-passes`** to `lint` or
`matrix` to run only the built-ins.

This decision is deliberately a command-line flag and not a
`.cmakedb.toml` setting: the config file lives in the repository under
analysis, so a setting there could not be trusted to disable anything.

WASM user passes are not supported; the experimental host was removed
before the first release (see `ROADMAP.md`).

## Dependency policy

`deny.toml` pins the license and advisory policy for the dependency tree.
`cargo deny check` runs in CI and fails on any unignored RUSTSEC
advisory; the `ignore` list is deliberately empty, so silencing an
advisory requires an argued change to that file.

## Scope

Findings are advisory. cmakedb reports what a configuration *did* — it is
an analysis tool, not a policy enforcement boundary, and its output should
not be the only control gating a build.
