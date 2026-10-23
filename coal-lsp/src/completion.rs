//! What Ctrl+Space offers.
//!
//! Four places know what may be written in them, checked in this order:
//!
//! 1. inside `{{ ... }}`: the names in scope, or the children of the path typed
//!    so far;
//! 2. the value of `$include` / `$include_optional`, written inline or as list
//!    items: the names in the `_definitions` namespace at the level the typed
//!    path has reached;
//! 3. the items of a `$keys` block beside a `$select`: the keys of the selected
//!    mapping;
//! 4. a field — or an item of a list — whose schema lists its allowed values.

use crate::editor_tree::{
    EditorNode, build_editor_tree, file_lines, find_node_at_line, get_byte_index, indentation,
};
use coal::coal_tree::tree_to_yaml;
use coal::definition_scopes::directory_of_origin;
use coal::directory_loader::{LoadedConfiguration, load_configuration_directory};
use coal::reference_resolution::{
    find_target_node_using_reference_path, resolve_jinja_reference_string,
};
use coal::type_aliases::{VirtualFileSystemReference, new_virtual_file_system_reference};
use coal::virtual_file_system::{VirtualFileSystem, normalize_path};
use serde_yaml::Value;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, MarkupContent,
    MarkupKind, Position, Range, TextEdit,
};

/// Where the schema lives, relative to the configuration root.
const SCHEMA_DIRECTORY_NAME: &str = "_schema";

/// The completions for a position (0-based line, UTF-16 column), or `None`
/// where nothing is known about what belongs there.
pub fn complete(
    workspace_root: &str,
    vfs: &VirtualFileSystemReference,
    file_path: &str,
    line: u32,
    character: u32,
) -> Option<Vec<CompletionItem>> {
    let lines = file_lines(vfs, file_path)?;
    let line_index = line as usize;
    let line_text = lines.get(line_index).map(String::as_str).unwrap_or("");
    let byte_position = get_byte_index(line_text, character);
    let left_part = &line_text[..byte_position];

    if let Some(typed) = jinja_expression_before_cursor(left_part) {
        return complete_jinja_path(workspace_root, vfs, file_path, line, typed);
    }

    if let Some(typed) = include_path_before_cursor(&lines, line_index, left_part) {
        return complete_include_path(workspace_root, vfs, file_path, line, character, &typed);
    }

    if let Some(select_path) = select_target_for_keys_item(&lines, line_index)
        && let Some(items) = complete_select_keys(workspace_root, vfs, file_path, line, &select_path)
    {
        return Some(items);
    }

    complete_allowed_values(workspace_root, vfs, file_path, &lines, line_index)
}

// =====================================================================
// JINJA PATHS
// =====================================================================

/// What has been typed inside an unclosed `{{` before the cursor.
fn jinja_expression_before_cursor(left_part: &str) -> Option<&str> {
    let start = left_part.rfind("{{")?;
    if left_part[start..].contains("}}") {
        return None;
    }
    Some(left_part[start + 2..].trim_start())
}

fn complete_jinja_path(
    workspace_root: &str,
    vfs: &VirtualFileSystemReference,
    file_path: &str,
    line: u32,
    typed: &str,
) -> Option<Vec<CompletionItem>> {
    let base_path = typed.rfind('.').map(|dot| {
        match typed[..=dot].chars().all(|c| c == '.') {
            true => typed[..=dot].to_string(),
            false => typed[..dot].trim().to_string(),
        }
    });

    let root = build_editor_tree(workspace_root, vfs).ok()?;
    let context_node = find_node_at_line(Rc::clone(&root), (line + 1) as usize, file_path)
        .unwrap_or_else(|| Rc::clone(&root));

    let mut items = Vec::new();
    match base_path {
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
                for key in data_keys(&node) {
                    if seen_keys.insert(key.clone()) {
                        items.push(CompletionItem {
                            label: key,
                            kind: Some(CompletionItemKind::VARIABLE),
                            detail: Some("In Scope".to_string()),
                            ..Default::default()
                        });
                    }
                }
                scope = borrowed.parent.as_ref().and_then(|w| w.upgrade());
            }
        }
        Some(path) => {
            if let Some(target_node) =
                find_target_node_using_reference_path(&path, Rc::clone(&context_node), Rc::clone(&root))
            {
                for key in data_keys(&target_node) {
                    items.push(CompletionItem {
                        label: key,
                        kind: Some(CompletionItemKind::PROPERTY),
                        detail: Some(format!("Child of {}", path)),
                        ..Default::default()
                    });
                }
            }
        }
    }

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.dedup_by(|a, b| a.label == b.label);
    Some(items)
}

/// The keys of a node's children, leaving out system directives: they may still
/// be unprocessed while editing, and they are not data anyone can select.
fn data_keys(node: &EditorNode) -> Vec<String> {
    node.borrow()
        .children
        .iter()
        .filter_map(|child| child.borrow().key.clone())
        .filter(|key| !key.starts_with('$'))
        .collect()
}

// =====================================================================
// $include PATHS
// =====================================================================

const INCLUDE_DIRECTIVES: [&str; 2] = ["$include", "$include_optional"];

/// The path typed so far when the cursor is in the value of an include, either
/// inline (`$include: templates.ba|`) or as an item of the list beneath one.
fn include_path_before_cursor(lines: &[String], line_index: usize, left_part: &str) -> Option<String> {
    let trimmed = left_part.trim_start();
    let without_dash = trimmed.strip_prefix('-').map(str::trim_start).unwrap_or(trimmed);

    // Inline: `$include: <path>` (possibly as a list item itself).
    for directive in INCLUDE_DIRECTIVES {
        if let Some(rest) = without_dash.strip_prefix(directive)
            && let Some(value) = rest.strip_prefix(':')
        {
            return clean_include_value(value);
        }
    }

    // A list item under an include written on a line of its own.
    if !trimmed.starts_with('-') {
        return None;
    }
    let item_indent = indentation(left_part);
    let parent = parent_line(lines, line_index, item_indent)?;
    let parent_trimmed = parent.trim();
    let parent_is_include = INCLUDE_DIRECTIVES.iter().any(|directive| {
        parent_trimmed
            .strip_prefix(directive)
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|rest| strip_comment(rest).trim().is_empty())
    });
    match parent_is_include {
        true => clean_include_value(&trimmed[1..]),
        false => None,
    }
}

/// The path part of an include value, or `None` where it is not a plain path
/// (a flow list, or a template the namespace cannot answer for).
fn clean_include_value(value: &str) -> Option<String> {
    let value = value.trim_start();
    if value.starts_with('[') || value.contains("{{") || value.contains('#') {
        return None;
    }
    Some(value.trim_start_matches(['"', '\'']).to_string())
}

fn complete_include_path(
    workspace_root: &str,
    vfs: &VirtualFileSystemReference,
    file_path: &str,
    line: u32,
    character: u32,
    typed: &str,
) -> Option<Vec<CompletionItem>> {
    let (parent_path, segment) = match typed.rfind('.') {
        Some(dot) => (&typed[..dot], &typed[dot + 1..]),
        None => ("", typed),
    };

    let namespace = definitions_namespace_for(workspace_root, vfs, file_path)?;
    let level = walk_namespace(namespace, parent_path)?;

    // Only the segment after the last dot is replaced, so the editor filters
    // on it rather than on the whole dotted path.
    let segment_length: u32 = segment.chars().map(|c| c.len_utf16() as u32).sum();
    let range = Range::new(
        Position::new(line, character.saturating_sub(segment_length)),
        Position::new(line, character),
    );

    let level_name = match parent_path.is_empty() {
        true => "_definitions".to_string(),
        false => parent_path.to_string(),
    };

    let children = level.borrow().children.clone();
    let mut items = Vec::new();
    for child in children {
        let Some(key) = child.borrow().key.clone() else { continue };
        if key.starts_with('$') {
            continue;
        }
        let has_children = !child.borrow().children.is_empty();
        let preview = serde_yaml::to_string(&tree_to_yaml(Rc::clone(&child))).unwrap_or_default();
        let full_path = match parent_path.is_empty() {
            true => key.clone(),
            false => format!("{}.{}", parent_path, key),
        };

        items.push(CompletionItem {
            label: key.clone(),
            kind: Some(match has_children {
                true => CompletionItemKind::MODULE,
                false => CompletionItemKind::VALUE,
            }),
            detail: Some(format!("definition in {}", level_name)),
            documentation: Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("`{}`\n```yaml\n{}\n```", full_path, truncate_lines(&preview, 30)),
            })),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: key,
            })),
            ..Default::default()
        });
    }

    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

/// The definitions an include in `file_path` reads: those of the file's own
/// directory and every directory above it, the deeper ones overriding. A file
/// inside `_schema/` reads the schema's definitions, the others the
/// configuration's.
///
/// Only the directory is read — no build — so an include that does not
/// resolve yet (which is what one looks like while it is being typed) does
/// not stop its own completion.
fn definitions_namespace_for(
    workspace_root: &str,
    vfs: &VirtualFileSystemReference,
    file_path: &str,
) -> Option<EditorNode> {
    let root = normalize_path(workspace_root);
    let schema_directory = normalize_path(
        &Path::new(workspace_root).join(SCHEMA_DIRECTORY_NAME).to_string_lossy(),
    );
    let file = normalize_path(file_path);
    let file_directory = directory_of_origin(&file);

    let directory = match file.starts_with(&format!("{}/", schema_directory)) {
        true => schema_directory,
        false => root,
    };
    let visible = |loaded: LoadedConfiguration| {
        loaded.definition_scopes.visible_from_directory(&file_directory)
    };

    if let Ok(loaded) = load_configuration_directory(&directory, Arc::clone(vfs)) {
        return Some(visible(loaded));
    }

    // A file halfway through being typed often does not parse, and the file
    // being typed is the one that stops the directory from loading. The names
    // it can include come from elsewhere, so read the directory as if the file
    // said what it last said on disk, or nothing at all.
    let mut without_file = VirtualFileSystem::new();
    without_file.files = vfs.lock().unwrap().files.clone();
    without_file.files.remove(&file);
    let without_file = new_virtual_file_system_reference(without_file);
    if let Ok(loaded) = load_configuration_directory(&directory, Arc::clone(&without_file)) {
        return Some(visible(loaded));
    }

    without_file.lock().unwrap().files.insert(file, String::new());
    load_configuration_directory(&directory, without_file)
        .ok()
        .map(visible)
}

fn walk_namespace(namespace: EditorNode, dotted_path: &str) -> Option<EditorNode> {
    let mut current = namespace;
    for segment in dotted_path.split('.').filter(|segment| !segment.is_empty()) {
        let next = current
            .borrow()
            .children
            .iter()
            .find(|child| child.borrow().key.as_deref() == Some(segment))
            .cloned()?;
        current = next;
    }
    Some(current)
}

fn truncate_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    match lines.len() > max_lines {
        true => format!("{}\n...", lines[..max_lines].join("\n")),
        false => lines.join("\n"),
    }
}

// =====================================================================
// $keys / $select
// =====================================================================

/// The `$select` path of the block, when the cursor is on an item (or an empty
/// line) of a `$keys:` list.
fn select_target_for_keys_item(lines: &[String], line_index: usize) -> Option<String> {
    let line_text = lines.get(line_index)?;
    let is_list_item = line_text.trim_start().starts_with('-');
    let is_empty_line = line_text.trim().is_empty();
    if !is_list_item && !is_empty_line {
        return None;
    }

    let current_indent = indentation(line_text);

    let mut keys_line = None;
    for i in (0..line_index).rev() {
        let previous = &lines[i];
        if previous.trim().is_empty() {
            continue;
        }
        if indentation(previous) < current_indent {
            if previous.trim_start().starts_with("$keys:") {
                keys_line = Some(i);
            }
            break;
        }
    }
    let keys_line = keys_line?;
    let keys_indent = indentation(&lines[keys_line]);

    // The mapping holding `$keys` bounds where its `$select` may be.
    let mut block_start = 0;
    for j in (0..keys_line).rev() {
        if lines[j].trim().is_empty() {
            continue;
        }
        if indentation(&lines[j]) < keys_indent {
            block_start = j + 1;
            break;
        }
    }

    for scan_line in &lines[block_start..] {
        if scan_line.trim().is_empty() {
            continue;
        }
        let scan_indent = indentation(scan_line);
        if scan_indent < keys_indent {
            break;
        }
        if scan_indent == keys_indent
            && let Some(rest) = scan_line.trim_start().strip_prefix("$select:")
        {
            return Some(rest.trim().trim_matches(|c| c == '\'' || c == '"').to_string());
        }
    }
    None
}

fn complete_select_keys(
    workspace_root: &str,
    vfs: &VirtualFileSystemReference,
    file_path: &str,
    line: u32,
    select_path: &str,
) -> Option<Vec<CompletionItem>> {
    let root = build_editor_tree(workspace_root, vfs).ok()?;
    let context_node = find_node_at_line(Rc::clone(&root), (line + 1) as usize, file_path)
        .unwrap_or_else(|| Rc::clone(&root));

    let resolved_path =
        resolve_jinja_reference_string(select_path, Rc::clone(&context_node), Rc::clone(&root));
    let target_node =
        find_target_node_using_reference_path(&resolved_path, Rc::clone(&context_node), root)?;

    Some(
        data_keys(&target_node)
            .into_iter()
            .map(|key| CompletionItem {
                label: key,
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(format!("Available in {}", resolved_path)),
                ..Default::default()
            })
            .collect(),
    )
}

// =====================================================================
// ALLOWED VALUES
// =====================================================================

fn complete_allowed_values(
    workspace_root: &str,
    vfs: &VirtualFileSystemReference,
    file_path: &str,
    lines: &[String],
    line_index: usize,
) -> Option<Vec<CompletionItem>> {
    let root = build_editor_tree(workspace_root, vfs).ok()?;

    let allowed_values = find_node_at_line(Rc::clone(&root), line_index + 1, file_path)
        .and_then(|node| node.borrow().allowed_values.clone())
        .or_else(|| allowed_values_for_new_list_item(&root, file_path, lines, line_index))?;

    let items = allowed_values
        .iter()
        .filter_map(|value| match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
        .map(|label| CompletionItem {
            label,
            kind: Some(CompletionItemKind::ENUM_MEMBER),
            detail: Some("Allowed Value".to_string()),
            ..Default::default()
        })
        .collect();
    Some(items)
}

/// The allowed values of an item the tree does not have (yet): the cursor is on
/// a `-` line, and the list above it constrains every element. Any element it
/// already holds says what the others may be.
fn allowed_values_for_new_list_item(
    root: &EditorNode,
    file_path: &str,
    lines: &[String],
    line_index: usize,
) -> Option<Vec<Value>> {
    let line_text = lines.get(line_index)?;
    if !line_text.trim_start().starts_with('-') {
        return None;
    }

    let owner_index = parent_line_index(lines, line_index, indentation(line_text))?;
    let owner = find_node_at_line(Rc::clone(root), owner_index + 1, file_path)?;
    // The node found on the owner line is the deepest one written there; the
    // list is the one whose key the line declares, when that is a sibling.
    let key = lines[owner_index].trim().trim_end_matches(':').to_string();
    let parent = owner.borrow().parent.as_ref().and_then(|parent| parent.upgrade());
    let declared = parent.and_then(|parent| {
        parent
            .borrow()
            .children
            .iter()
            .find(|child| child.borrow().key.as_deref() == Some(key.as_str()))
            .cloned()
    });
    let list_node = declared.unwrap_or(owner);

    let children = list_node.borrow().children.clone();
    children
        .iter()
        .find_map(|child| child.borrow().allowed_values.clone())
}

// =====================================================================
// LINE STRUCTURE
// =====================================================================

/// The nearest non-blank line above `line_index` indented less than
/// `indent`. A list may also sit at its key's own indentation (`key:\n- a`),
/// so a line ending in `:` at the same indentation counts too.
fn parent_line_index(lines: &[String], line_index: usize, indent: usize) -> Option<usize> {
    for i in (0..line_index).rev() {
        let candidate = &lines[i];
        let content = strip_comment(candidate).trim_end();
        if content.trim().is_empty() {
            continue;
        }
        let candidate_indent = indentation(candidate);
        if candidate_indent < indent {
            return Some(i);
        }
        if candidate_indent == indent && !content.trim_start().starts_with('-') {
            return match content.ends_with(':') {
                true => Some(i),
                false => None,
            };
        }
    }
    None
}

fn parent_line(lines: &[String], line_index: usize, indent: usize) -> Option<&str> {
    parent_line_index(lines, line_index, indent).map(|i| lines[i].as_str())
}

fn strip_comment(line: &str) -> &str {
    let mut previous_is_space = true;
    for (byte_index, character) in line.char_indices() {
        if character == '#' && previous_is_space {
            return &line[..byte_index];
        }
        previous_is_space = character.is_whitespace();
    }
    line
}
