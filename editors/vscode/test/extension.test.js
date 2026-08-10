// Integration tests: these run inside a real VS Code instance with the
// extension loaded against a real recording (see prepare-workspace.js).
//
// The extension is deliberately thin — the analysis lives in cmakedb-lsp,
// which is tested in Rust. What can only be verified here is the wiring:
// that activation succeeds, that the contributed surface exists, and that
// the client actually reaches the server and gets results back.

const assert = require("assert");
const path = require("path");
const vscode = require("vscode");

const EXT_ID = "cmakedb.cmakedb";

/** Poll until `f()` returns something truthy, or fail after `ms`. */
async function waitFor(what, ms, f) {
  const deadline = Date.now() + ms;
  for (;;) {
    const v = await f();
    if (v !== undefined && v !== null && (!Array.isArray(v) || v.length)) {
      return v;
    }
    if (Date.now() > deadline) {
      assert.fail(`timed out after ${ms}ms waiting for ${what}`);
    }
    await new Promise((r) => setTimeout(r, 250));
  }
}

suite("cmakedb extension", () => {
  suiteSetup(async function () {
    const ext = vscode.extensions.getExtension(EXT_ID);
    assert.ok(ext, `extension ${EXT_ID} not found`);
    await ext.activate();
  });

  test("activates without throwing", () => {
    assert.strictEqual(vscode.extensions.getExtension(EXT_ID).isActive, true);
  });

  test("contributes its commands", async () => {
    const all = await vscode.commands.getCommands(true);
    for (const c of ["cmakedb.why", "cmakedb.refreshProvenance"]) {
      assert.ok(all.includes(c), `command not registered: ${c}`);
    }
  });

  test("the test workspace actually has a recording", async () => {
    const folder = vscode.workspace.workspaceFolders[0].uri;
    const db = vscode.Uri.joinPath(folder, ".cmakedb", "trace.db");
    // Guards the diagnostics test below from passing vacuously.
    await vscode.workspace.fs.stat(db);
  });

  test("language client reports diagnostics from the recording", async () => {
    const folder = vscode.workspace.workspaceFolders[0].uri;
    const uri = vscode.Uri.joinPath(folder, "CMakeLists.txt");
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);

    // fixtures/provenance is built to trigger dead-code findings, so an
    // empty result here means the client never reached the server.
    const diags = await waitFor("diagnostics on CMakeLists.txt", 45000, () =>
      vscode.languages.getDiagnostics(uri),
    );
    assert.ok(diags.length > 0, "expected at least one diagnostic");
    for (const d of diags) {
      assert.ok(d.message && d.message.length, "diagnostic without a message");
      assert.ok(d.range, "diagnostic without a range");
    }
  });

  test("refresh command runs against a live server", async () => {
    // Exercises the tree provider's data path; throws propagate as a
    // rejected promise and fail the test.
    await vscode.commands.executeCommand("cmakedb.refreshProvenance");
  });
});
