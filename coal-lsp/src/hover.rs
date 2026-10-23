//! What hovering over a `{{ reference }}` shows.

use crate::editor_tree::{build_editor_tree, file_lines, find_node_at_line, get_byte_index};
use coal::coal_tree::tree_to_yaml;
use coal::reference_resolution::find_target_node_using_reference_path;
use coal::type_aliases::VirtualFileSystemReference;
use std::rc::Rc;

fn extract_jinja_path_at_cursor(line: &str, utf16_pos: u32) -> Option<String> {
    let byte_pos = get_byte_index(line, utf16_pos);

    let left_part = &line[..byte_pos];
    let right_part = &line[byte_pos..];

    if let Some(start_idx) = left_part.rfind("{{") {
        if left_part[start_idx..].contains("}}") {
            return None;
        }

        if let Some(end_offset) = right_part.find("}}") {
            let inner_content = &line[start_idx + 2..byte_pos + end_offset];
            return Some(inner_content.trim().to_string());
        }
    }
    None
}

/// The markdown shown when hovering at `line`/`character` (0-based, UTF-16).
pub fn hover_markdown(
    workspace_root: &str,
    vfs: &VirtualFileSystemReference,
    file_path: &str,
    line: u32,
    character: u32,
) -> Option<String> {
    let lines = file_lines(vfs, file_path)?;
    let line_text = lines.get(line as usize)?;
    let reference_path = extract_jinja_path_at_cursor(line_text, character)?;

    let root = build_editor_tree(workspace_root, vfs).ok()?;
    let context_node = find_node_at_line(Rc::clone(&root), (line + 1) as usize, file_path)
        .unwrap_or_else(|| Rc::clone(&root));
    let target_node = find_target_node_using_reference_path(&reference_path, context_node, root)?;

    let full_yaml = tree_to_yaml(target_node);
    let display_string = serde_yaml::to_string(&full_yaml).unwrap_or_else(|_| String::from("..."));
    Some(format!("**Resolved Jinja Value:**\n```yaml\n{}\n```", display_string.trim()))
}
