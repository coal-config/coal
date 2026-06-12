# Change Log

All notable changes to the COAL extension will be documented in this file.

## [0.2.0]

* Clear error notifications (with a shortcut to settings) when the language server binary is missing or fails to start, instead of repeated cryptic spawn failures.
* New `COAL: Restart Language Server` command to recover from a crashed or outdated server.
* Changing `coal.languageServerPath` now restarts the server automatically; the setting also supports `~` for the home directory.
* The server is only started once a workspace folder is open (it validates the project rooted there), and starts automatically when a folder is added.
* All `.yaml`, `.yml`, and `.schema` files are watched so the server no longer validates stale content after files change outside the editor (e.g. a git checkout).
* `.schema` files are associated with the COAL language, and the language gains basic editing support (`#` comments, bracket and quote auto-closing).

## [0.1.1]

* Initial release.
