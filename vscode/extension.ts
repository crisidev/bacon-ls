import * as vscode from "vscode";
import * as os from "os";

import {
  ExecuteCommandRequest,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  Executable,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

// Settings that change how the server itself is launched. These cannot be
// applied without a restart.
const RESTART_ON_CHANGE = [
  "bacon-ls.path",
  "bacon-ls.logLevel",
  // The server cannot switch backends at runtime — restart is the only path.
  "bacon_ls.backend",
];

export async function activate(
  context: vscode.ExtensionContext,
): Promise<void> {
  let name = "Bacon-ls";

  const outputChannel = vscode.window.createOutputChannel(name);

  context.subscriptions.push(outputChannel);

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration(
      async (e: vscode.ConfigurationChangeEvent) => {
        if (RESTART_ON_CHANGE.some((s) => e.affectsConfiguration(s))) {
          await vscode.commands.executeCommand("bacon-ls.restart");
        }
        // All other bacon_ls.* changes are picked up by the server through
        // workspace/didChangeConfiguration, sent automatically by
        // vscode-languageclient because of synchronize.configurationSection.
      },
    ),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("bacon-ls.restart", async () => {
      if (client && client.needsStop()) {
        await client.stop();
      }

      try {
        client = await createClient(context, name, outputChannel);
      } catch (err) {
        vscode.window.showErrorMessage(
          `${err instanceof Error ? err.message : err}`,
        );
        return;
      }

      await client.start();
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("bacon-ls.run", async () => {
      if (!client || !client.isRunning()) {
        vscode.window.showWarningMessage("bacon-ls is not running");
        return;
      }
      try {
        await client.sendRequest(ExecuteCommandRequest.type, {
          command: "bacon_ls.run",
        });
      } catch (err) {
        vscode.window.showErrorMessage(
          `bacon-ls.run failed: ${err instanceof Error ? err.message : err}`,
        );
      }
    }),
  );

  await vscode.commands.executeCommand("bacon-ls.restart");
}

async function createClient(
  context: vscode.ExtensionContext,
  name: string,
  outputChannel: vscode.OutputChannel,
): Promise<LanguageClient> {
  const env = { ...process.env };

  const extensionConfig = vscode.workspace.getConfiguration("bacon-ls");
  const path = await getServerPath(context, extensionConfig);

  outputChannel.appendLine("Using bacon-ls server " + path);

  env.RUST_LOG = extensionConfig.get("logLevel");

  const run: Executable = {
    command: path,
    options: { env: env },
  };

  const serverOptions: ServerOptions = {
    run: run,
    debug: run,
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "untitled" },
      { scheme: "file", pattern: "**" },
      { scheme: "vscode-scm" },
    ],
    outputChannel: outputChannel,
    traceOutputChannel: outputChannel,
    // Forward server-side settings through the standard
    // workspace/configuration pull (which vscode-languageclient handles by
    // mapping section -> getConfiguration(section)) and notify the server on
    // changes via workspace/didChangeConfiguration.
    synchronize: {
      configurationSection: "bacon_ls",
    },
  };

  return new LanguageClient(
    name.toLowerCase(),
    name,
    serverOptions,
    clientOptions,
  );
}

async function getServerPath(
  context: vscode.ExtensionContext,
  config: vscode.WorkspaceConfiguration,
): Promise<string> {
  let path = process.env.BACON_LS_PATH ?? config.get<null | string>("path");

  if (path) {
    if (path.startsWith("~/")) {
      path = os.homedir() + path.slice("~".length);
    }
    const pathUri = vscode.Uri.file(path);

    return await vscode.workspace.fs.stat(pathUri).then(
      () => pathUri.fsPath,
      () => {
        throw new Error(
          `${path} does not exist. Please check bacon-ls.path in Settings.`,
        );
      },
    );
  }

  const ext = process.platform === "win32" ? ".exe" : "";
  const bundled = vscode.Uri.joinPath(
    context.extensionUri,
    "bundled",
    `bacon-ls${ext}`,
  );

  return await vscode.workspace.fs.stat(bundled).then(
    () => bundled.fsPath,
    () => {
      throw new Error(
        "Unfortunately we don't ship binaries for your platform yet. " +
          "Try specifying bacon-ls.path in Settings. " +
          "Or raise an issue [here](https://github.com/crisidev/bacon-ls/issues) " +
          "to request a binary for your platform.",
      );
    },
  );
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }
  return client.stop();
}
