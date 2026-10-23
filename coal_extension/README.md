# COAL for Visual Studio Code

Language support for [COAL](https://github.com/coal-config/coal) configuration projects. The extension connects VS Code to the COAL language server (`coal-lsp`) and provides:

* **Diagnostics** — the whole workspace is compiled and validated against the `schema/` directory on every change, and errors are shown on the line they originate from.
* **Hover** — preview the fully resolved value of `$inherit`, `$override`, and Jinja (`{{ ... }}`) references.
* **Completion** — path suggestions inside Jinja expressions and directive references.

The extension activates for YAML files, `.coal` files, and `.schema` files.

## Requirements

The `coal-lsp` executable must be installed. Either:

* put `coal-lsp` on your system `PATH` (the default lookup), or
* set `coal.languageServerPath` to the full path of the executable (`~` is supported).

## Settings

| Setting | Description |
| --- | --- |
| `coal.languageServerPath` | Path to the `coal-lsp` executable. Leave empty to resolve `coal-lsp` from the system `PATH`. Changing this restarts the server automatically. |
| `coalLanguageServer.trace.server` | Log the LSP traffic (`off`, `messages`, `verbose`) for troubleshooting. |

## Commands

| Command | Description |
| --- | --- |
| `COAL: Restart Language Server` | Stops and restarts `coal-lsp`. Use this after installing or updating the binary, or if the server stops responding. |
