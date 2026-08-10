// cmakedb VS Code extension (design §2.4, M5): thin client over
// `cmakedb lsp` plus a provenance tree view fed by the server's custom
// requests (cmakedb/targets, cmakedb/edges, cmakedb/provenance).

const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

let client;

/** Tree: targets at the root; expanding a node shows its link edges with
 *  visibility and the originating file:line; edge nodes expand further,
 *  rendering the propagation chain incrementally. */
class ProvenanceProvider {
  constructor() {
    this._onDidChange = new vscode.EventEmitter();
    this.onDidChangeTreeData = this._onDidChange.event;
  }

  refresh() {
    this._onDidChange.fire(undefined);
  }

  async getChildren(element) {
    if (!client) return [];
    if (!element) {
      const targets = await client.sendRequest("cmakedb/targets", {});
      return (targets || []).map((t) => ({
        kind: "target",
        name: t.name,
        detail: t.type,
        expandable: true,
      }));
    }
    if (!element.expandable) return [];
    const edges = await client.sendRequest("cmakedb/edges", {
      target: element.name,
    });
    return (edges || []).map((e) => ({
      kind: "edge",
      name: e.dst,
      detail: `${e.visibility}${e.origin ? "  " + e.origin : ""}`,
      origin: e.origin,
      expandable: e.isTarget,
    }));
  }

  getTreeItem(element) {
    const item = new vscode.TreeItem(
      element.name,
      element.expandable
        ? vscode.TreeItemCollapsibleState.Collapsed
        : vscode.TreeItemCollapsibleState.None
    );
    item.description = element.detail;
    item.iconPath = new vscode.ThemeIcon(
      element.kind === "target" ? "package" : "arrow-right"
    );
    if (element.origin) {
      const [file, line] = splitLocation(element.origin);
      if (file) {
        item.command = {
          command: "vscode.open",
          title: "open origin",
          arguments: [
            vscode.Uri.file(file),
            { selection: new vscode.Range(line - 1, 0, line - 1, 0) },
          ],
        };
        item.tooltip = `origin: ${element.origin}`;
      }
    }
    return item;
  }
}

function splitLocation(loc) {
  const i = loc.lastIndexOf(":");
  if (i <= 0) return [null, 0];
  const line = parseInt(loc.slice(i + 1), 10);
  return [loc.slice(0, i), isNaN(line) ? 1 : line];
}

async function whyCommand() {
  if (!client) return;
  const target = await vscode.window.showInputBox({
    prompt: "Target (e.g. app)",
  });
  if (!target) return;
  const dep = await vscode.window.showInputBox({
    prompt: "Dependency to explain (e.g. z)",
  });
  if (!dep) return;
  const ex = await client.sendRequest("cmakedb/provenance", { target, dep });
  const out = vscode.window.createOutputChannel("cmakedb");
  out.clear();
  if (!ex) {
    out.appendLine(`no link path from '${target}' to '${dep}' in this recording`);
  } else {
    out.appendLine(`${target} links ${dep} via:`);
    renderTree(out, ex.tree, "", true, 0);
    if (ex.final_evidence && ex.final_evidence.length) {
      out.appendLine("final resolved evidence (File API):");
      for (const e of ex.final_evidence) out.appendLine(`  ${e}`);
    }
  }
  out.show(true);
}

function renderTree(out, node, prefix, isLast, depth) {
  const connector = depth === 0 ? "" : prefix + (isLast ? "└─ " : "├─ ");
  const vis = node.visibility ? node.visibility + " " : "";
  const origin = node.origin ? `    [${node.origin}]` : "";
  out.appendLine(`${connector}${vis}${node.label}${origin}`);
  const childPrefix =
    depth === 0 ? "" : prefix + (isLast ? "   " : "│  ");
  (node.children || []).forEach((c, i) =>
    renderTree(out, c, childPrefix, i + 1 === (node.children || []).length, depth + 1)
  );
}

function activate(context) {
  const serverPath = vscode.workspace
    .getConfiguration("cmakedb")
    .get("serverPath", "cmakedb");
  client = new LanguageClient(
    "cmakedb",
    "cmakedb",
    { command: serverPath, args: ["lsp"] },
    {
      documentSelector: [
        { pattern: "**/CMakeLists.txt" },
        { pattern: "**/*.cmake" },
      ],
    }
  );
  client.start();

  const provider = new ProvenanceProvider();
  context.subscriptions.push(
    vscode.window.registerTreeDataProvider("cmakedbProvenance", provider),
    vscode.commands.registerCommand("cmakedb.refreshProvenance", () =>
      provider.refresh()
    ),
    vscode.commands.registerCommand("cmakedb.why", whyCommand)
  );
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
