import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { ExtensionContext, FileSystemWatcher, commands, window, workspace } from 'vscode';
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
} from 'vscode-languageclient/node';

const RESTART_COMMAND = 'coal.restartLanguageServer';
const LANGUAGE_SERVER_PATH_SETTING = 'coal.languageServerPath';

let client: LanguageClient | undefined;
let configurationFileWatcher: FileSystemWatcher | undefined;

export async function activate(context: ExtensionContext): Promise<void> {
    context.subscriptions.push(
        commands.registerCommand(RESTART_COMMAND, restartLanguageServer)
    );

    // Pick up a changed binary path without requiring a window reload.
    context.subscriptions.push(
        workspace.onDidChangeConfiguration(async (event) => {
            if (event.affectsConfiguration(LANGUAGE_SERVER_PATH_SETTING)) {
                await restartLanguageServer();
            }
        })
    );

    // The language server validates the COAL project rooted at the first
    // workspace folder; without a folder there is nothing to validate, so
    // wait until one is added.
    context.subscriptions.push(
        workspace.onDidChangeWorkspaceFolders(async () => {
            if (!client && workspaceFolderIsOpen()) {
                await startLanguageServer();
            }
        })
    );

    if (workspaceFolderIsOpen()) {
        await startLanguageServer();
    }
}

export async function deactivate(): Promise<void> {
    configurationFileWatcher?.dispose();
    configurationFileWatcher = undefined;

    const runningClient = client;
    client = undefined;
    if (runningClient) {
        await runningClient.dispose();
    }
}

function workspaceFolderIsOpen(): boolean {
    return (workspace.workspaceFolders?.length ?? 0) > 0;
}

async function startLanguageServer(): Promise<void> {
    if (client) {
        return;
    }

    const serverCommand = resolveServerCommand();
    if (!serverCommand) {
        return;
    }

    const serverOptions: ServerOptions = {
        run: { command: serverCommand },
        debug: { command: serverCommand },
    };

    const clientOptions: LanguageClientOptions = {
        documentSelector: [
            { scheme: 'file', language: 'yaml' },
            { scheme: 'file', language: 'coal' },
            { scheme: 'file', pattern: '**/*.schema' },
        ],
        synchronize: {
            // The server caches every file it reads. It evicts entries only
            // when notified of watched-file changes, so watch every file type
            // a COAL build can read or the server keeps validating stale
            // content after files change outside the editor.
            fileEvents: getConfigurationFileWatcher(),
        },
    };

    const startingClient = new LanguageClient(
        'coalLanguageServer',
        'COAL Language Server',
        serverOptions,
        clientOptions
    );

    try {
        await startingClient.start();
        client = startingClient;
    } catch (error) {
        await startingClient.dispose().catch(() => undefined);
        await showServerStartFailure(serverCommand, error);
    }
}

async function restartLanguageServer(): Promise<void> {
    const runningClient = client;
    client = undefined;

    if (runningClient) {
        // The old server may already be dead; that must not block the restart.
        await runningClient.dispose().catch(() => undefined);
    }

    if (workspaceFolderIsOpen()) {
        await startLanguageServer();
    }
}

/** The watcher is created once and shared across server restarts. */
function getConfigurationFileWatcher(): FileSystemWatcher {
    if (!configurationFileWatcher) {
        configurationFileWatcher = workspace.createFileSystemWatcher('**/*.{yaml,yml,schema}');
    }
    return configurationFileWatcher;
}

/**
 * Resolves the server executable: the configured path when set (with "~"
 * expanded and existence verified), otherwise the platform's default command
 * name resolved through the system PATH.
 */
function resolveServerCommand(): string | undefined {
    const configuredPath =
        workspace.getConfiguration('coal').get<string>('languageServerPath')?.trim() ?? '';

    if (configuredPath.length === 0) {
        return process.platform === 'win32' ? 'coal-lsp.exe' : 'coal-lsp';
    }

    const expandedPath = expandHomeDirectory(configuredPath);
    if (looksLikeFilePath(expandedPath) && !fs.existsSync(expandedPath)) {
        void showMissingConfiguredBinary(expandedPath);
        return undefined;
    }

    return expandedPath;
}

function expandHomeDirectory(configuredPath: string): string {
    if (
        configuredPath === '~' ||
        configuredPath.startsWith('~/') ||
        configuredPath.startsWith('~\\')
    ) {
        return path.join(os.homedir(), configuredPath.slice(1));
    }
    return configuredPath;
}

/**
 * A configured value without any path separator is treated as a command name
 * to be resolved through the system PATH, so it cannot be checked for
 * existence up front.
 */
function looksLikeFilePath(configuredPath: string): boolean {
    return configuredPath.includes('/') || configuredPath.includes('\\');
}

async function showMissingConfiguredBinary(expandedPath: string): Promise<void> {
    await showServerProblem(
        `The configured COAL language server was not found at '${expandedPath}'. ` +
            `Fix the "${LANGUAGE_SERVER_PATH_SETTING}" setting, then run "COAL: Restart Language Server".`
    );
}

async function showServerStartFailure(serverCommand: string, error: unknown): Promise<void> {
    console.error(`Failed to start the COAL language server ('${serverCommand}'):`, error);

    await showServerProblem(
        `The COAL language server failed to start ('${serverCommand}'). ` +
            `Install coal-lsp on your PATH or point "${LANGUAGE_SERVER_PATH_SETTING}" at the executable, ` +
            'then run "COAL: Restart Language Server".'
    );
}

async function showServerProblem(message: string): Promise<void> {
    const openSettings = 'Open Settings';
    const choice = await window.showErrorMessage(message, openSettings);
    if (choice === openSettings) {
        void commands.executeCommand('workbench.action.openSettings', LANGUAGE_SERVER_PATH_SETTING);
    }
}
