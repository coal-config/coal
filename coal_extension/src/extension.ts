import * as path from 'path';
import * as os from 'os';
import * as fs from 'fs';
import { workspace, ExtensionContext } from 'vscode';
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
} from 'vscode-languageclient/node';

let client: LanguageClient;

export function activate(context: ExtensionContext) {
    // 1. Determine the current operating system
    const platform = os.platform();
    let osString = '';
    let ext = '';

    if (platform === 'win32') {
        osString = 'windows';
        ext = '.exe';
    } else if (platform === 'linux') {
        osString = 'linux';
        ext = '';
    } else {
        osString = platform === 'darwin' ? 'macos' : platform;
        ext = '';
    }

    // 2. Look for the release binary bundled by GitHub Actions
    const extensionRoot = context.extensionPath;
    const files = fs.readdirSync(extensionRoot);

    // Find a file like "coal-lsp-v1.0.0-windows.exe" or "coal-lsp-v1.0.0-linux"
    const releaseBinary = files.find(file =>
        file.startsWith('coal-lsp-') &&
        file.includes(`-${osString}`) &&
        file.endsWith(ext)
    );

    let serverPath = '';

    if (releaseBinary) {
        // Production: Use the bundled binary
        serverPath = context.asAbsolutePath(releaseBinary);
    } else {
        // Local Development Fallback: Point directly to the cargo target directory
        // Assumes your workspace root has 'coal-lsp' and 'coal_extension' side-by-side
        const localBin = platform === 'win32' ? 'coal-lsp.exe' : 'coal-lsp';
        serverPath = context.asAbsolutePath(path.join('..', 'coal-lsp', 'target', 'release', localBin));

        if (!fs.existsSync(serverPath)) {
            throw new Error(`COAL LSP binary not found. \nLooked for release binary in: ${extensionRoot} \nLooked for local binary at: ${serverPath}`);
        }
    }

    const serverOptions: ServerOptions = {
        run: { command: serverPath },
        debug: { command: serverPath }
    };

    // Options to control the language client
    const clientOptions: LanguageClientOptions = {
        // Register the server for YAML and COAL files
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