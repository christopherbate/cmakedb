import { defineConfig } from "@vscode/test-cli";

// The workspace is built by test/prepare-workspace.js (npm pretest): a
// staged fixture with a real recording, so the language client has
// something to serve.
export default defineConfig({
  files: "test/**/*.test.js",
  workspaceFolder: "./.vscode-test/workspace",
  mocha: {
    // Launching VS Code, activating, spawning the server and waiting for
    // the first diagnostics is well over Mocha's 2s default.
    timeout: 60000,
  },
});
