use coal::type_aliases::VirtualFileSystemReference;
use coal::virtual_file_system::{VirtualFileSystem, normalize_path};
use coal_lsp::completion::complete;
use coal_lsp::diagnostics::collect_workspace_diagnostics;
use coal_lsp::editor_tree::snapshot_file_system;
use coal_lsp::hover::hover_markdown;
use coal_lsp::projects::{
    PublishedDiagnostics, can_change_project_roots, discover_project_roots, enclosing_project_root,
    is_within, nearest_project, projects_containing,
};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{Duration, timeout};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

/// How long the editor has to be quiet before a validation starts.
const VALIDATION_DEBOUNCE: Duration = Duration::from_millis(400);

/// The COAL projects in the workspace, and which of them need validating.
///
/// The server finds the projects itself (see `coal_lsp::projects`); the
/// workspace folder is only where it looks.
#[derive(Default)]
pub struct Projects {
    /// Every known project root, as a normalized path.
    pub roots: BTreeSet<String>,
    /// The roots whose files changed since they were last validated.
    pub dirty: BTreeSet<String>,
}

pub struct Backend {
    pub client: Client,
    pub vfs: VirtualFileSystemReference,
    /// The folders the editor has open, as normalized paths.
    pub workspace_folders: Arc<Mutex<Vec<String>>>,
    pub projects: Arc<Mutex<Projects>>,
    /// Held while the projects are looked for, so that an older search can
    /// never finish after a newer one and undo what it found.
    pub discovery: tokio::sync::Mutex<()>,
    /// Documents the editor has open, by normalized path, with the version of
    /// the text last taken from it. The editor is the authority on their
    /// content, so a change on disk does not replace it.
    pub open_documents: Arc<Mutex<HashMap<String, i32>>>,
    /// The file edited last: where a build that cannot finish is reported when
    /// its message names no file.
    pub last_edited: Arc<Mutex<Option<String>>>,
    /// Counts the edits. The validator watches it; see `run_validator`.
    pub edits: watch::Sender<u64>,
}

impl Backend {
    fn wake_validator(&self) {
        self.edits.send_modify(|count| *count = count.wrapping_add(1));
    }

    /// Marks every project `path` belongs to as needing validation.
    fn file_changed(&self, path: &str) {
        let marked = {
            let mut projects = self.projects.lock().unwrap();
            let containing = projects_containing(path, &projects.roots);
            let marked = !containing.is_empty();
            projects.dirty.extend(containing);
            marked
        };
        if marked {
            self.wake_validator();
        }
    }

    /// The project whose build answers hover and completion in `file_path`.
    fn project_of(&self, file_path: &str) -> Option<String> {
        let path = normalize_path(file_path);
        nearest_project(&path, &self.projects.lock().unwrap().roots)
            .or_else(|| enclosing_project_root(Path::new(&path)))
    }

    /// Looks for the projects again, and starts or stops validating the ones
    /// that appeared or went away.
    ///
    /// The projects are those below the workspace folders, plus the one each
    /// open document belongs to: a file opened from above or beside the
    /// workspace folders still gets checked.
    async fn discover_projects(&self) {
        let _one_search_at_a_time = self.discovery.lock().await;

        let folders = self.workspace_folders.lock().unwrap().clone();
        let open_documents: Vec<String> = self.open_documents.lock().unwrap().keys().cloned().collect();
        let found = tokio::task::spawn_blocking(move || {
            let mut roots: BTreeSet<String> = folders
                .iter()
                .flat_map(|folder| discover_project_roots(Path::new(folder)))
                .collect();
            for document in open_documents {
                if nearest_project(&document, &roots).is_none() {
                    roots.extend(enclosing_project_root(Path::new(&document)));
                }
            }
            roots
        })
        .await
        .unwrap_or_default();

        let (added, removed) = {
            let mut projects = self.projects.lock().unwrap();
            let added: Vec<String> = found.difference(&projects.roots).cloned().collect();
            let removed: Vec<String> = projects.roots.difference(&found).cloned().collect();
            projects.dirty.extend(added.iter().cloned());
            projects.roots = found;
            (added, removed)
        };

        if !added.is_empty() || !removed.is_empty() {
            self.report_projects(&added, &removed).await;
            self.wake_validator();
        }
    }

    /// Starts validating the project `path` belongs to, found by walking up
    /// from it, if there is one and it is not known yet.
    async fn adopt_project_of(&self, path: &str) {
        let _one_search_at_a_time = self.discovery.lock().await;

        let start = path.to_string();
        let Ok(Some(root)) = tokio::task::spawn_blocking(move || enclosing_project_root(Path::new(&start))).await
        else {
            return;
        };
        let added = {
            let mut projects = self.projects.lock().unwrap();
            let added = projects.roots.insert(root.clone());
            projects.dirty.insert(root.clone());
            added
        };
        if added {
            self.report_projects(&[root], &[]).await;
        }
        self.wake_validator();
    }

    async fn report_projects(&self, added: &[String], removed: &[String]) {
        for root in added {
            self.client.log_message(MessageType::INFO, format!("COAL project found: {}", root)).await;
        }
        for root in removed {
            self.client.log_message(MessageType::INFO, format!("COAL project gone: {}", root)).await;
        }
        if self.projects.lock().unwrap().roots.is_empty() {
            self.client
                .log_message(
                    MessageType::INFO,
                    "No COAL project in the workspace. A folder is a COAL project when it holds \
                     `_structure.yaml`, a `_schema/` folder, or a `.coal` file.",
                )
                .await;
        }
    }

    /// Takes the workspace folder list the editor sent.
    fn set_workspace_folders(&self, folders: Vec<String>) {
        let mut folders: Vec<String> = folders.into_iter().map(|folder| normalize_path(&folder)).collect();
        folders.sort();
        folders.dedup();
        *self.workspace_folders.lock().unwrap() = folders;
    }
}

/// Validates the projects whenever they change, one build at a time.
///
/// A build can take seconds, and the editor keeps sending changes while it
/// runs. There is exactly one of these loops, and it never abandons a build
/// halfway, so:
///
/// * results are published in the order the edits were made — an older build
///   can never finish after a newer one and paint its findings over them;
/// * what each project has reported is always updated together with what is
///   published, so a file whose findings went away is always cleared. (Cancelling
///   a build at its first `await` used to leave that record behind, and the
///   findings it forgot about stayed in the editor until a restart.)
///
/// Only the projects whose files changed are built again: an edit in one
/// project does not rebuild the others. A project that is no longer there has
/// its findings cleared here too, so this loop is the only thing that ever
/// publishes.
///
/// An edit made while a build runs is not lost: the watch channel remembers
/// it, and the loop goes round again as soon as the build is published.
async fn run_validator(
    client: Client,
    vfs: VirtualFileSystemReference,
    projects: Arc<Mutex<Projects>>,
    last_edited: Arc<Mutex<Option<String>>>,
    mut edits: watch::Receiver<u64>,
) {
    let mut published = PublishedDiagnostics::default();

    loop {
        if edits.changed().await.is_err() {
            return;
        }

        // Wait for the editor to go quiet.
        loop {
            match timeout(VALIDATION_DEBOUNCE, edits.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => return,
                Err(_) => break,
            }
        }
        edits.borrow_and_update();

        let (roots, dirty) = {
            let mut projects = projects.lock().unwrap();
            let dirty = std::mem::take(&mut projects.dirty);
            (projects.roots.clone(), dirty)
        };

        for gone in published.projects().difference(&roots) {
            publish(&client, published.remove(gone)).await;
        }

        for root in dirty.intersection(&roots) {
            let fallback_file = last_edited
                .lock()
                .unwrap()
                .clone()
                .filter(|file| is_within(file, root));
            let snapshot = snapshot_file_system(&vfs);
            let build_root = root.clone();

            let build = tokio::task::spawn_blocking(move || {
                collect_workspace_diagnostics(&build_root, &snapshot, fallback_file.as_deref())
            })
            .await;

            match build {
                Ok(diagnostics) => publish(&client, published.replace(root, diagnostics)).await,
                Err(panic) => {
                    client
                        .log_message(
                            MessageType::ERROR,
                            format!("COAL validation of {} crashed: {}", root, panic),
                        )
                        .await;
                }
            }
        }
    }
}

async fn publish(client: &Client, files: Vec<(Uri, Vec<Diagnostic>)>) {
    for (url, diagnostics) in files {
        client.publish_diagnostics(url, diagnostics, None).await;
    }
}

fn folder_paths(folders: &[WorkspaceFolder]) -> Vec<String> {
    folders
        .iter()
        .filter_map(|folder| folder.uri.to_file_path().map(|path| path.into_owned()))
        .map(|path| path.to_string_lossy().to_string())
        .collect()
}

impl LanguageServer for Backend {
    #[allow(deprecated)] // root_uri: fallback for clients that send no workspace folders
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let folders = match (params.workspace_folders, params.root_uri) {
            (Some(folders), _) => folder_paths(&folders),
            (None, Some(uri)) => uri.to_file_path().into_iter().map(|path| path.to_string_lossy().to_string()).collect(),
            (None, None) => Vec::new(),
        };
        self.set_workspace_folders(folders);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![
                        " ".to_string(), ":".to_string(), "-".to_string(),
                        "{".to_string(), ".".to_string()
                    ]),
                    ..Default::default()
                }),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client.log_message(MessageType::INFO, "COAL Language Server initialized!").await;
        self.discover_projects().await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        let removed: BTreeSet<String> = folder_paths(&params.event.removed)
            .iter()
            .map(|folder| normalize_path(folder))
            .collect();
        let mut folders: Vec<String> = self
            .workspace_folders
            .lock()
            .unwrap()
            .iter()
            .filter(|folder| !removed.contains(*folder))
            .cloned()
            .collect();
        folders.extend(folder_paths(&params.event.added));
        self.set_workspace_folders(folders);
        self.discover_projects().await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        if let Some(file_path) = params.text_document.uri.to_file_path() {
            let path = normalize_path(&file_path.to_string_lossy());
            self.open_documents.lock().unwrap().insert(path.clone(), params.text_document.version);
            self.vfs.lock().unwrap().set_file_content(&path, params.text_document.text);

            let known = nearest_project(&path, &self.projects.lock().unwrap().roots).is_some();
            match known {
                true => self.file_changed(&path),
                // Opened from outside every project found so far: look for
                // the one it belongs to.
                false => self.adopt_project_of(&path).await,
            }
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(file_path) = params.text_document.uri.to_file_path() else { return };
        let path = normalize_path(&file_path.to_string_lossy());
        let version = params.text_document.version;

        {
            // Changes carry the whole text, so only the newest one matters,
            // and one that arrives after a newer one must not undo it.
            let mut open_documents = self.open_documents.lock().unwrap();
            if open_documents.get(&path).is_some_and(|known| *known > version) {
                return;
            }
            open_documents.insert(path.clone(), version);

            if let Some(change) = params.content_changes.into_iter().last() {
                self.vfs.lock().unwrap().set_file_content(&path, change.text);
            }
        }

        *self.last_edited.lock().unwrap() = Some(path.clone());
        self.file_changed(&path);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        if let Some(file_path) = params.text_document.uri.to_file_path() {
            let path = normalize_path(&file_path.to_string_lossy());
            self.open_documents.lock().unwrap().remove(&path);
            // Unsaved edits are gone with the editor tab; the file on disk is
            // what the build reads from now on.
            self.vfs.lock().unwrap().files.remove(&path);
            self.file_changed(&path);
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let mut changed_paths = Vec::new();
        let mut roots_may_have_changed = false;
        {
            let open_documents = self.open_documents.lock().unwrap();
            let mut vfs = self.vfs.lock().unwrap();
            for change in params.changes {
                if let Some(file_path) = change.uri.to_file_path() {
                    let path = normalize_path(&file_path.to_string_lossy());
                    // Only a file or folder appearing or disappearing can
                    // change which directories are projects; editing one
                    // cannot.
                    if change.typ != FileChangeType::CHANGED && can_change_project_roots(&path) {
                        roots_may_have_changed = true;
                    }
                    changed_paths.push(path.clone());
                    if change.typ != FileChangeType::DELETED && open_documents.contains_key(&path) {
                        continue;
                    }
                    vfs.files.remove(&path);
                }
            }
        }

        if roots_may_have_changed {
            self.discover_projects().await;
        }
        for path in changed_paths {
            self.file_changed(&path);
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some(file_path) = uri.to_file_path() else { return Ok(None) };
        let file_path = file_path.to_string_lossy().to_string();
        let Some(root) = self.project_of(&file_path) else { return Ok(None) };

        let snapshot = snapshot_file_system(&self.vfs);
        let items = tokio::task::spawn_blocking(move || {
            complete(&root, &snapshot, &file_path, position.line, position.character)
        })
        .await
        .ok()
        .flatten();

        Ok(items.map(CompletionResponse::Array))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some(file_path) = uri.to_file_path() else { return Ok(None) };
        let file_path = file_path.to_string_lossy().to_string();
        let Some(root) = self.project_of(&file_path) else { return Ok(None) };

        let snapshot = snapshot_file_system(&self.vfs);
        let markdown = tokio::task::spawn_blocking(move || {
            hover_markdown(&root, &snapshot, &file_path, position.line, position.character)
        })
        .await
        .ok()
        .flatten();

        Ok(markdown.map(|value| Hover {
            contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }),
            range: None,
        }))
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let vfs = Arc::new(Mutex::new(VirtualFileSystem::new()));
    let projects = Arc::new(Mutex::new(Projects::default()));
    let last_edited = Arc::new(Mutex::new(None));
    let (edits, edits_receiver) = watch::channel(0u64);

    let (service, socket) = LspService::new(|client| {
        tokio::spawn(run_validator(
            client.clone(),
            Arc::clone(&vfs),
            Arc::clone(&projects),
            Arc::clone(&last_edited),
            edits_receiver,
        ));

        Backend {
            client,
            vfs,
            workspace_folders: Arc::new(Mutex::new(Vec::new())),
            projects,
            discovery: tokio::sync::Mutex::new(()),
            open_documents: Arc::new(Mutex::new(HashMap::new())),
            last_edited,
            edits,
        }
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}
