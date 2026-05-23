import * as path from 'path';
import { workspace, ExtensionContext } from 'vscode';
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
} from 'vscode-languageclient/node';

let client: LanguageClient;

export function activate(context: ExtensionContext) {
    // 1. Check VS Code settings for a custom path
    const config = workspace.getConfiguration('coal');
    const configuredPath = config.get<string>('languageServerPath');

    // 2. Define the default fallback command (resolved via system PATH)
    const defaultCommand = process.platform === 'win32' ? 'coal-lsp.exe' : 'coal-lsp';

    // 3. Use the configured path if it exists and isn't empty, otherwise fall back
    const command = configuredPath ? configuredPath : defaultCommand;

    const serverOptions: ServerOptions = {
        run: { command: command },
        debug: { command: command }
    };

    // Options to control the language client
    const clientOptions: LanguageClientOptions = {
        documentSelector: [
            { scheme: 'file', language: 'yaml' },
            { scheme: 'file', language: 'coal' },
            { scheme: 'file', pattern: '**/*.schema' }
        ],
        synchronize: {
            // Notify the server about file changes to '.schema' files contained in the workspace
            fileEvents: workspace.createFileSystemWatcher('**/*.schema')
        }
    };

    // Create the language client and start the client.
    client = new LanguageClient(
        'coalLanguageServer',
        'COAL Language Server',
        serverOptions,
        clientOptions
    );

    // Start the client. This will also launch the Rust server process.
    client.start();
}

export function deactivate(): Thenable<void> | undefined {
    if (!client) {
        return undefined;
    }
    return client.stop();
}