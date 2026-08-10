# cmakedb for VS Code

Semantic CMake analysis backed by an execution recording. Requires the
`cmakedb` binary on PATH (or set `cmakedb.serverPath`) and a recording:

```sh
cmakedb record -- -S . -B build -G Ninja
```

Features (all post-mortem — answers come from the recording, and
diagnostics are stale-marked once a file diverges from it):

- **Diagnostics** from every lint pass (dead options/functions/modules,
  clobbers, scope-leaks, …) plus your `.cmakedb/passes/*.sql` passes via
  `cmakedb lint` in CI.
- **Hover**: a variable's resolved value at that line + write history; a
  target's type, link edges, and resolved include count.
- **Go to definition**: the dominating write for the read under the
  cursor, function/macro definitions, target definition sites.
- **Quick fixes**: modernize patches (`include_directories` →
  `target_include_directories` etc.), offered only while the buffer
  matches the recording. Prefer `cmakedb modernize --fix` for the
  re-record verification loop.
- **CMake Provenance view** (Explorer sidebar): every target, expandable
  into its link edges with visibility and the originating `file:line` —
  click an edge to jump to the call that created it. The
  `cmakedb: Why does a target link a library?` command prints the full
  §2.2-style propagation tree.

## Development install

```sh
cd editors/vscode
npm install
# then in VS Code: Run > Start Debugging (extension host), or:
npx @vscode/vsce package   # produces cmakedb-0.1.0.vsix
code --install-extension cmakedb-0.1.0.vsix
```
