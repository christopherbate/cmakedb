// Builds the workspace the integration tests open.
//
// `cmakedb lsp` is a post-mortem server: it answers from a recording and
// has nothing to say without one. So this stages a real fixture, records
// a real cmake configure into it, and points the extension at the binary
// under test. Every failure here is fatal on purpose — a missing binary
// or a failed recording must not degrade into a test run that passes
// because the server had nothing to report.

const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const extRoot = path.resolve(__dirname, "..");
const repoRoot = path.resolve(extRoot, "..", "..");
const ws = path.join(extRoot, ".vscode-test", "workspace");

function die(msg) {
  console.error(`prepare-workspace: ${msg}`);
  process.exit(1);
}

const binary = ["release", "debug"]
  .map((p) => path.join(repoRoot, "target", p, "cmakedb"))
  .find((p) => fs.existsSync(p));
if (!binary) {
  die("no cmakedb binary found; run `cargo build` (or `--release`) first");
}

// Fresh every run: a stale recording would silently test the wrong thing.
fs.rmSync(ws, { recursive: true, force: true });
fs.mkdirSync(ws, { recursive: true });
fs.cpSync(path.join(repoRoot, "fixtures", "provenance"), ws, {
  recursive: true,
});

try {
  execFileSync(
    binary,
    [
      "record",
      "--source-dir",
      ws,
      "--build-dir",
      path.join(ws, "build"),
      "--db",
      path.join(ws, ".cmakedb", "trace.db"),
    ],
    { stdio: "pipe" },
  );
} catch (e) {
  die(
    `recording the fixture failed (is cmake on PATH?):\n${e.stderr || e.message}`,
  );
}

const db = path.join(ws, ".cmakedb", "trace.db");
if (!fs.existsSync(db)) die("record reported success but produced no database");

// The extension resolves the server via this setting; without it the test
// would depend on whatever `cmakedb` happens to be on PATH.
fs.mkdirSync(path.join(ws, ".vscode"), { recursive: true });
fs.writeFileSync(
  path.join(ws, ".vscode", "settings.json"),
  JSON.stringify({ "cmakedb.serverPath": binary }, null, 2),
);

console.log(`prepare-workspace: ready at ${ws} (server: ${binary})`);
