use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::virtual_file_system::{VirtualFileSystem, normalize_path};
use coal::coal_tree::{CoalTree, tree_to_yaml};
use coal::reference_resolution::{find_target_node_using_reference_path, resolve_jinja_reference_string};
use std::sync::{Arc, Mutex};
use std::collections::{HashMap, HashSet};
use std::cell::RefCell;
use std::rc::Rc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tokio::time::Duration;

pub struct Backend {
    pub client: Client,
    pub vfs: Arc<Mutex<VirtualFileSystem>>,
    pub document_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    pub published_diagnostics: Arc<Mutex<HashSet<Url>>>,
    pub workspace_root: Arc<Mutex<String>>,
}

fn get_byte_index(line: &str, utf16_pos: u32) -> usize {
    let mut current_utf16 = 0;
    for (byte_idx, char_val) in line.char_indices() {
        if current_utf16 >= utf16_pos {
            return byte_idx;
        }
        current_utf16 += char_val.len_utf16() as u32;
    }
    line.len()
}

fn get_line_text(vfs: &Arc<Mutex<VirtualFileSystem>>, file_path: &str, line_idx: usize) -> Option<String> {
    let mut lock = vfs.lock().unwrap();
    let content = lock.get_file_content(file_path).ok()?;
    content.lines().nth(line_idx).map(|s| s.to_string())
}

fn extract_jinja_path_at_cursor(line: &str, utf16_pos: u32) -> Option<String> {
    let byte_pos = get_byte_index(line, utf16_pos);

    let left_part = &line[..byte_pos];
    let right_part = &line[byte_pos..];

    if let Some(start_idx) = left_part.rfind("{{") {
        if left_part[start_idx..].contains("}}") {
            return None;
        }

        if let Some(end_offset) = right_part.find("}}") {
            let inner_content = &line[start_idx + 2 .. byte_pos + end_offset];
            return Some(inner_content.trim().to_string());
        }
    }
    None
}

impl Backend {
    async fn trigger_workspace_validation(&self) {
        let mut tasks = self.document_tasks.lock().unwrap();

        if let Some(existing_task) = tasks.get("global") {
            existing_task.abort();
        }

        let client = self.client.clone();
        let vfs = Arc::clone(&self.vfs);
        let published_diagnostics = Arc::clone(&self.published_diagnostics);

        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            Backend::validate_workspace(client, vfs, published_diagnostics).await;
        });

        tasks.insert("global".to_string(), task);
    }

    async fn validate_workspace(
        client: Client,
        vfs: Arc<Mutex<VirtualFileSystem>>,
        published_diagnostics: Arc<Mutex<HashSet<Url>>>,
    ) {
        let mut global_diagnostics: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
        let mut diags_to_add = Vec::new();

        {
            let pipeline_result = build_entire_coal_configuration_tree("main.yaml", "schema", Arc::clone(&vfs), true, &[]);

            match pipeline_result {
                Ok((_, coal_diagnostics)) => {
                    for diag in coal_diagnostics {
                        let (line, col) = if let Some(span) = &diag.span {
                            (span.line.saturating_sub(1) as u32, span.col.saturating_sub(1) as u32)
                        } else {
                            (0, 0)
                        };

                        diags_to_add.push((
                            diag.file_origin.clone(),
                            Diagnostic {
                                range: Range::new(Position::new(line, col), Position::new(line, col + 50)),
                                severity: Some(DiagnosticSeverity::ERROR),
                                code: Some(NumberOrString::String(diag.error_type.clone())),
                                source: Some("COAL".to_string()),
                                message: diag.message.clone(),
                                ..Default::default()
                            }
                        ));
                    }
                }
                Err(e) => {
                    if let Ok(dir) = std::env::current_dir() {
                        let main_yaml_path = dir.join("main.yaml").to_string_lossy().to_string();
                        diags_to_add.push((
                            main_yaml_path,
                            Diagnostic {
                                range: Range::new(Position::new(0, 0), Position::new(0, 100)),
                                severity: Some(DiagnosticSeverity::ERROR),
                                source: Some("COAL Parser".to_string()),
                                message: format!("Fatal Error: {}", e),
                                ..Default::default()
                            }
                        ));
                    }
                }
            }
        }

        for (origin_path, diag) in diags_to_add {
            if let Ok(target_url) = Url::from_file_path(std::path::Path::new(&normalize_path(&origin_path))) {
                global_diagnostics.entry(target_url).or_default().push(diag);
            }
        }

        let current_published = published_diagnostics.lock().unwrap().clone();
        let mut new_published = HashSet::new();

        for (url, mut diags) in global_diagnostics {
            diags.dedup_by(|a, b| a.message == b.message && a.range == b.range);
            client.publish_diagnostics(url.clone(), diags, None).await;
            new_published.insert(url);
        }

        for url in current_published.iter() {
            if !new_published.contains(url) {
                client.publish_diagnostics(url.clone(), vec![], None).await;
            }
        }

        *published_diagnostics.lock().unwrap() = new_published;
    }
}

fn find_node_at_line(tree: Rc<RefCell<CoalTree>>, target_line: usize, target_file: &str) -> Option<Rc<RefCell<CoalTree>>> {
    let node_borrowed = tree.borrow();

    for child in &node_borrowed.children {
        if let Some(found) = find_node_at_line(Rc::clone(child), target_line, target_file) {
            return Some(found);
        }
    }

    if let Some(span) = &node_borrowed.span {
        let node_origin = node_borrowed.origin.clone().unwrap_or_default();
        if span.line == target_line && normalize_path(&node_origin) == normalize_path(target_file) {
            return Some(Rc::clone(&tree));
        }
    }

    None
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let mut workspace_path = String::new();
        if let Some(folders) = params.workspace_folders {
            if let Some(folder) = folders.first() {
                if let Ok(path) = folder.uri.to_file_path() {
                    workspace_path = path.to_string_lossy().to_string();
                }
            }
        } else if let Some(uri) = params.root_uri {
            if let Ok(path) = uri.to_file_path() {
                workspace_path = path.to_string_lossy().to_string();
            }
        }

        if !workspace_path.is_empty() {
            *self.workspace_root.lock().unwrap() = workspace_path.clone();
            std::env::set_current_dir(&workspace_path).ok();
        }

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
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client.log_message(MessageType::INFO, "COAL Language Server initialized!").await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        if let Ok(file_path) = params.text_document.uri.to_file_path() {
            self.vfs.lock().unwrap().set_file_content(&file_path.to_string_lossy(), params.text_document.text);
            self.trigger_workspace_validation().await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Ok(file_path) = params.text_document.uri.to_file_path() {
            let path_string = file_path.to_string_lossy().to_string();
            for change in params.content_changes {
                self.vfs.lock().unwrap().set_file_content(&path_string, change.text);
            }
            self.trigger_workspace_validation().await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for change in params.changes {
            if let Ok(file_path) = change.uri.to_file_path() {
                let path_string = file_path.to_string_lossy().to_string();
                self.vfs.lock().unwrap().files.remove(&normalize_path(&path_string));
            }
        }
        self.trigger_workspace_validation().await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let file_path = match uri.to_file_path() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => return Ok(None),
        };

        let file_content = {
            let mut lock = self.vfs.lock().unwrap();
            lock.get_file_content(&file_path).ok()
        };

        let mut is_in_jinja = false;
        let mut jinja_base_path: Option<String> = None;
        let mut select_target_path: Option<String> = None;

        if let Some(content) = file_content {
            let lines: Vec<&str> = content.lines().collect();
            let current_line_idx = position.line as usize;

            if current_line_idx < lines.len() {
                let line_text = lines[current_line_idx];

                // 1. Check for Jinja Scope
                let byte_pos = get_byte_index(line_text, position.character);
                let left_part = &line_text[..byte_pos];

                if let Some(start_idx) = left_part.rfind("{{") {
                    if !left_part[start_idx..].contains("}}") {
                        is_in_jinja = true;
                        let typed_so_far = left_part[start_idx + 2..].trim_start();
                        if let Some(dot_idx) = typed_so_far.rfind('.') {
                            let is_all_dots = typed_so_far[..=dot_idx].chars().all(|c| c == '.');
                            if is_all_dots {
                                jinja_base_path = Some(typed_so_far[..=dot_idx].to_string());
                            } else {
                                jinja_base_path = Some(typed_so_far[..dot_idx].trim().to_string());
                            }
                        }
                    }
                }

                // 2. Check for $keys / $select Context
                if !is_in_jinja {
                    let is_list_item = line_text.trim_start().starts_with('-');
                    let is_empty_line = line_text.trim().is_empty();

                    if is_list_item || is_empty_line {
                        let current_indent = line_text.chars().take_while(|c| c.is_whitespace()).count();

                        let mut keys_indent = None;
                        let mut parent_line_idx = 0;

                        // Scan backwards to find the $keys: directive
                        for i in (0..current_line_idx).rev() {
                            let prev_line = lines[i];
                            if prev_line.trim().is_empty() { continue; }

                            let prev_indent = prev_line.chars().take_while(|c| c.is_whitespace()).count();

                            if prev_indent < current_indent {
                                if prev_line.trim_start().starts_with("$keys:") {
                                    keys_indent = Some(prev_indent);

                                    // Bounding the block: find the parent object mapping
                                    for j in (0..i).rev() {
                                        let p_line = lines[j];
                                        if p_line.trim().is_empty() { continue; }
                                        let p_indent = p_line.chars().take_while(|c| c.is_whitespace()).count();
                                        if p_indent < prev_indent {
                                            parent_line_idx = j;
                                            break;
                                        }
                                    }
                                }
                                break;
                            }
                        }

                        // If $keys: is found, scan the whole parent block for $select:
                        if let Some(k_indent) = keys_indent {
                            for j in (parent_line_idx + 1)..lines.len() {
                                let scan_line = lines[j];
                                if scan_line.trim().is_empty() { continue; }
                                let scan_indent = scan_line.chars().take_while(|c| c.is_whitespace()).count();

                                if scan_indent < k_indent {
                                    break; // Reached the end of the block
                                }

                                if scan_indent == k_indent && scan_line.trim_start().starts_with("$select:") {
                                    let path_str = scan_line.trim_start()["$select:".len()..].trim();
                                    select_target_path = Some(path_str.trim_matches(|c| c == '\'' || c == '"').to_string());
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        let pipeline_result = build_entire_coal_configuration_tree("main.yaml", "schema", Arc::clone(&self.vfs), true, &[]);

        if let Ok((root, _)) = pipeline_result {
            let mut items = Vec::new();

            let context_node = find_node_at_line(Rc::clone(&root), (position.line + 1) as usize, &file_path)
                .unwrap_or_else(|| Rc::clone(&root));

            // Provide $select Keys
            if let Some(target_raw) = select_target_path {
                let resolved_path = resolve_jinja_reference_string(&target_raw, Rc::clone(&context_node), Rc::clone(&root));

                if let Some(target_node) = find_target_node_using_reference_path(&resolved_path, Rc::clone(&context_node), Rc::clone(&root)) {
                    let borrowed = target_node.borrow();
                    for child in &borrowed.children {
                        if let Some(key) = &child.borrow().key {
                            // Never suggest system directives ('$'-prefixed keys):
                            // they may still be unprocessed while editing and are
                            // internal, not selectable data.
                            if key.starts_with('$') {
                                continue;
                            }
                            items.push(CompletionItem {
                                label: key.clone(),
                                kind: Some(CompletionItemKind::ENUM_MEMBER),
                                detail: Some(format!("Available in {}", resolved_path)),
                                ..Default::default()
                            });
                        }
                    }
                    return Ok(Some(CompletionResponse::Array(items)));
                }
            }

            // Provide Jinja Paths
            if is_in_jinja {
                match jinja_base_path {
                    None => {
                        items.push(CompletionItem {
                            label: "ROOT".to_string(),
                            kind: Some(CompletionItemKind::CLASS),
                            detail: Some("Absolute Path Root".to_string()),
                            ..Default::default()
                        });

                        let mut scope = Some(Rc::clone(&context_node));
                        let mut seen_keys = HashSet::new();
                        while let Some(node) = scope {
                            let borrowed = node.borrow();
                            for child in &borrowed.children {
                                if let Some(key) = &child.borrow().key {
                                    // Skip unprocessed system directives.
                                    if key.starts_with('$') {
                                        continue;
                                    }
                                    if seen_keys.insert(key.clone()) {
                                        items.push(CompletionItem {
                                            label: key.clone(),
                                            kind: Some(CompletionItemKind::VARIABLE),
                                            detail: Some("In Scope".to_string()),
                                            ..Default::default()
                                        });
                                    }
                                }
                            }
                            scope = borrowed.parent.as_ref().and_then(|w| w.upgrade());
                        }
                    }
                    Some(path) => {
                        if let Some(target_node) = find_target_node_using_reference_path(&path, Rc::clone(&context_node), Rc::clone(&root)) {
                            let borrowed = target_node.borrow();
                            for child in &borrowed.children {
                                if let Some(key) = &child.borrow().key {
                                    // Skip unprocessed system directives.
                                    if key.starts_with('$') {
                                        continue;
                                    }
                                    items.push(CompletionItem {
                                        label: key.clone(),
                                        kind: Some(CompletionItemKind::PROPERTY),
                                        detail: Some(format!("Child of {}", path)),
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                    }
                }

                items.sort_by(|a, b| a.label.cmp(&b.label));
                items.dedup_by(|a, b| a.label == b.label);

                return Ok(Some(CompletionResponse::Array(items)));
            }

            // Provide Schema Allowed Values
            let target_line = (position.line + 1) as usize;
            if let Some(node) = find_node_at_line(Rc::clone(&root), target_line, &file_path) {
                let node_borrowed = node.borrow();

                if let Some(allowed_values) = &node_borrowed.allowed_values {
                    for val in allowed_values {
                        let val_str = match val {
                            serde_yaml::Value::String(s) => s.clone(),
                            serde_yaml::Value::Number(n) => n.to_string(),
                            serde_yaml::Value::Bool(b) => b.to_string(),
                            _ => continue,
                        };

                        items.push(CompletionItem {
                            label: val_str,
                            kind: Some(CompletionItemKind::ENUM_MEMBER),
                            detail: Some("Allowed Value".to_string()),
                            ..Default::default()
                        });
                    }
                    return Ok(Some(CompletionResponse::Array(items)));
                }
            }
        }

        Ok(None)
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let file_path = match uri.to_file_path() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => return Ok(None),
        };

        let line_text = match get_line_text(&self.vfs, &file_path, position.line as usize) {
            Some(text) => text,
            None => return Ok(None),
        };

        let reference_path = match extract_jinja_path_at_cursor(&line_text, position.character) {
            Some(path) => path,
            None => return Ok(None),
        };

        let pipeline_result = build_entire_coal_configuration_tree("main.yaml", "schema", Arc::clone(&self.vfs), true, &[]);

        if let Ok((root, _)) = pipeline_result {
            let context_node = find_node_at_line(Rc::clone(&root), (position.line + 1) as usize, &file_path)
                .unwrap_or_else(|| Rc::clone(&root));

            if let Some(target_node) = find_target_node_using_reference_path(&reference_path, context_node, root) {

                let full_yaml = tree_to_yaml(target_node);
                let display_string = serde_yaml::to_string(&full_yaml).unwrap_or_else(|_| String::from("..."));

                let markup = MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("**Resolved Jinja Value:**\n```yaml\n{}\n```", display_string.trim()),
                };

                return Ok(Some(Hover {
                    contents: HoverContents::Markup(markup),
                    range: None,
                }));
            }
        }

        Ok(None)
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let vfs = Arc::new(Mutex::new(VirtualFileSystem::new()));
    let document_tasks = Arc::new(Mutex::new(HashMap::new()));
    let published_diagnostics = Arc::new(Mutex::new(HashSet::new()));
    let workspace_root = Arc::new(Mutex::new(String::new()));

    let (service, socket) = LspService::new(|client| Backend {
        client,
        vfs,
        document_tasks,
        published_diagnostics,
        workspace_root,
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}