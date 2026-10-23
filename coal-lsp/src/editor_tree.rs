//! The configuration tree the editor answers questions from, and how a line of
//! a file is found in it.

use coal::coal_tree::CoalTree;
use coal::tree_builder::{BuildScope, CompiledConfiguration, compile_configurations};
use coal::type_aliases::VirtualFileSystemReference;
use coal::virtual_file_system::{VirtualFileSystem, normalize_path};
use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

pub type EditorNode = Rc<RefCell<CoalTree>>;

/// A copy of the file system as it stands now.
///
/// A build reads files one at a time over several seconds. Reading them from
/// the live file system would let an edit made halfway through land in the
/// second half of the build and not the first, and the result would describe
/// a directory that never existed. A copy is cheap next to a build, and a build
/// on a copy describes exactly one moment.
pub fn snapshot_file_system(vfs: &VirtualFileSystemReference) -> VirtualFileSystemReference {
    let files = vfs.lock().unwrap().files.clone();
    let mut snapshot = VirtualFileSystem::new();
    snapshot.files = files;
    Arc::new(Mutex::new(snapshot))
}

/// The tree hovering and completion read.
///
/// A structure means the directory describes several configurations, and the
/// answers only make sense in one of them: a value is what its layers settled
/// on, and where nothing has been selected there is no such value at all. The
/// first configuration is a real one, so it is the one the editor answers from.
pub fn build_editor_tree(
    workspace_root: &str,
    vfs: &VirtualFileSystemReference,
) -> Result<EditorNode, Box<dyn Error>> {
    let configurations = compile_configurations(
        workspace_root,
        Arc::clone(vfs),
        BuildScope::FirstCombination,
        true,
        &[],
        &[],
    )?;

    configurations
        .into_iter()
        .next()
        .map(|configuration: CompiledConfiguration| configuration.root_node)
        .ok_or_else(|| "the build produced no configuration".into())
}

/// The deepest node written on `target_line` (1-based) of `target_file`.
pub fn find_node_at_line(tree: EditorNode, target_line: usize, target_file: &str) -> Option<EditorNode> {
    let target_file = normalize_path(target_file);
    find_node_at_normalized_line(tree, target_line, &target_file)
}

fn find_node_at_normalized_line(tree: EditorNode, target_line: usize, target_file: &str) -> Option<EditorNode> {
    let node_borrowed = tree.borrow();

    for child in &node_borrowed.children {
        if let Some(found) = find_node_at_normalized_line(Rc::clone(child), target_line, target_file) {
            return Some(found);
        }
    }

    if let Some(span) = &node_borrowed.span {
        let node_origin = node_borrowed.origin.clone().unwrap_or_default();
        if span.line == target_line && normalize_path(&node_origin) == target_file {
            return Some(Rc::clone(&tree));
        }
    }

    None
}

/// Converts an LSP (UTF-16) column into a byte index into `line`.
pub fn get_byte_index(line: &str, utf16_pos: u32) -> usize {
    let mut current_utf16 = 0;
    for (byte_idx, char_val) in line.char_indices() {
        if current_utf16 >= utf16_pos {
            return byte_idx;
        }
        current_utf16 += char_val.len_utf16() as u32;
    }
    line.len()
}

/// How far a line is indented, in characters.
pub fn indentation(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

/// The text of one line of a file, as the editor currently has it.
pub fn file_lines(vfs: &VirtualFileSystemReference, file_path: &str) -> Option<Vec<String>> {
    let content = vfs.lock().unwrap().get_file_content(file_path).ok()?;
    Some(content.lines().map(str::to_string).collect())
}
