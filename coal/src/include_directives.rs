use crate::coal_tree::CoalTree;
use crate::reference_resolution::resolve_jinja_reference_string;
use crate::tree_operations::{add_or_replace_child_node_in_parent, build_tree_from_marked_node};
use crate::type_aliases::{TreeNodeReference, VirtualFileSystemReference, new_tree_node_reference};
use crate::virtual_file_system::normalize_path;
use crate::yaml_values::{convert_marked_yaml_node_to_value, extract_string_list_from_value};
use std::error::Error;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

/// Traverses the tree depth-first, expanding every file inclusion directive
/// ($include, $include_directory, $include_subdirectories, and their
/// _optional variants) by parsing the referenced files and splicing their
/// contents into the tree.
pub fn process_include_directives_depth_first(
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
    virtual_file_system: VirtualFileSystemReference,
) -> Result<(), Box<dyn Error>> {
    let children = current_node.borrow().children.clone();
    let mut processed_directive_nodes = Vec::new();

    for child in children {
        let was_include_directive = process_single_include_directive(
            Rc::clone(&child),
            Rc::clone(&current_node),
            Rc::clone(&root_node),
            Arc::clone(&virtual_file_system),
        )?;

        if was_include_directive {
            processed_directive_nodes.push(Rc::clone(&child));
        } else {
            process_include_directives_depth_first(
                Rc::clone(&child),
                Rc::clone(&root_node),
                Arc::clone(&virtual_file_system),
            )?;
        }
    }

    remove_processed_directive_nodes(current_node, processed_directive_nodes);
    Ok(())
}

/// Routes an include directive node to its handler. Returns true when the
/// node was an include directive and has been processed.
fn process_single_include_directive(
    node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    virtual_file_system: VirtualFileSystemReference,
) -> Result<bool, Box<dyn Error>> {
    let node_key = match node.borrow().key.clone() {
        Some(key) => key,
        None => return Ok(false),
    };

    match node_key.as_str() {
        "$include" => {
            handle_file_include(node, parent_node, root_node, virtual_file_system, false)?
        }
        "$include_optional" => {
            handle_file_include(node, parent_node, root_node, virtual_file_system, true)?
        }
        "$include_directory" => {
            handle_directory_include(node, parent_node, root_node, virtual_file_system, false)?
        }
        "$include_directory_optional" => {
            handle_directory_include(node, parent_node, root_node, virtual_file_system, true)?
        }
        "$include_subdirectories" => {
            handle_subdirectories_include(node, parent_node, root_node, virtual_file_system, false)?
        }
        "$include_subdirectories_optional" => {
            handle_subdirectories_include(node, parent_node, root_node, virtual_file_system, true)?
        }
        _ => return Ok(false),
    };

    Ok(true)
}

// =====================================================================
// DIRECTIVE HANDLERS
// =====================================================================

fn handle_file_include(
    include_node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    virtual_file_system: VirtualFileSystemReference,
    is_optional: bool,
) -> Result<(), Box<dyn Error>> {
    for unresolved_path in include_target_paths(&include_node) {
        let resolved_path = resolve_include_path(&unresolved_path, &include_node, &root_node);

        if let Some(file_content) =
            read_file_if_present(&virtual_file_system, &resolved_path, is_optional)?
        {
            include_yaml_file_into_parent(
                &file_content,
                &resolved_path,
                Rc::clone(&parent_node),
                Rc::clone(&root_node),
                Arc::clone(&virtual_file_system),
            )?;
        }
    }
    Ok(())
}

fn handle_directory_include(
    include_node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    virtual_file_system: VirtualFileSystemReference,
    is_optional: bool,
) -> Result<(), Box<dyn Error>> {
    for unresolved_path in include_target_paths(&include_node) {
        let directory_path = resolve_include_path(&unresolved_path, &include_node, &root_node);

        let directory_entries = match read_directory_if_present(
            &directory_path,
            is_optional,
            "Could not read include directory",
        )? {
            Some(entries) => entries,
            None => continue,
        };

        for entry in directory_entries {
            let entry_path = entry?.path();
            if is_yaml_file(&entry_path) {
                let file_stem = entry_path
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                include_file_as_named_child(
                    &entry_path,
                    file_stem,
                    Rc::clone(&parent_node),
                    Rc::clone(&root_node),
                    Arc::clone(&virtual_file_system),
                )?;
            }
        }
    }
    Ok(())
}

fn handle_subdirectories_include(
    include_node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    virtual_file_system: VirtualFileSystemReference,
    is_optional: bool,
) -> Result<(), Box<dyn Error>> {
    for unresolved_path in include_target_paths(&include_node) {
        let base_directory = resolve_include_path(&unresolved_path, &include_node, &root_node);

        let directory_entries = match read_directory_if_present(
            &base_directory,
            is_optional,
            "Could not read subdirectories in",
        )? {
            Some(entries) => entries,
            None => continue,
        };

        for entry in directory_entries {
            let subdirectory_path = entry?.path();
            if !subdirectory_path.is_dir() {
                continue;
            }

            let directory_name = subdirectory_path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let yaml_file_path = subdirectory_path.join(format!("{}.yaml", directory_name));

            if yaml_file_path.is_file() {
                include_file_as_named_child(
                    &yaml_file_path,
                    directory_name,
                    Rc::clone(&parent_node),
                    Rc::clone(&root_node),
                    Arc::clone(&virtual_file_system),
                )?;
            }
        }
    }
    Ok(())
}

// =====================================================================
// FILE AND DIRECTORY ACCESS
// =====================================================================

fn read_file_if_present(
    virtual_file_system: &VirtualFileSystemReference,
    path: &str,
    is_optional: bool,
) -> Result<Option<String>, Box<dyn Error>> {
    match virtual_file_system.lock().unwrap().get_file_content(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) => {
            if is_optional {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

fn read_directory_if_present(
    path: &str,
    is_optional: bool,
    error_prefix: &str,
) -> Result<Option<fs::ReadDir>, Box<dyn Error>> {
    match fs::read_dir(Path::new(path)) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) => {
            if is_optional {
                Ok(None)
            } else {
                Err(format!("{} '{}': {}", error_prefix, path, error).into())
            }
        }
    }
}

// =====================================================================
// TREE INTEGRATION
// =====================================================================

/// Parses a YAML file into a temporary tree, expands any nested includes,
/// and merges the result directly into the parent node.
fn include_yaml_file_into_parent(
    content: &str,
    file_path: &str,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    virtual_file_system: VirtualFileSystemReference,
) -> Result<(), Box<dyn Error>> {
    let temporary_root = parse_yaml_into_tree_node(
        content,
        file_path,
        "TEMPORARY_ROOT".to_string(),
        Rc::clone(&parent_node),
    )?;

    process_include_directives_depth_first(
        Rc::clone(&temporary_root),
        root_node,
        virtual_file_system,
    )?;

    merge_temporary_tree_into_parent(temporary_root, parent_node);
    Ok(())
}

fn merge_temporary_tree_into_parent(
    temporary_root: TreeNodeReference,
    parent_node: TreeNodeReference,
) {
    let temporary_children = temporary_root.borrow().children.clone();

    if temporary_children.is_empty() {
        parent_node.borrow_mut().value = temporary_root.borrow().value.clone();
        return;
    }

    let included_file_is_list = temporary_root.borrow().is_list_type();
    let mut next_list_index = find_next_list_index(&parent_node);

    for child in temporary_children {
        child.borrow_mut().parent = Some(Rc::downgrade(&parent_node));

        if included_file_is_list {
            child.borrow_mut().key = Some(next_list_index.to_string());
            parent_node.borrow_mut().children.push(Rc::clone(&child));
            next_list_index += 1;
        } else {
            add_or_replace_child_node_in_parent(Rc::clone(&parent_node), child, false);
        }
    }
}

fn find_next_list_index(node: &TreeNodeReference) -> usize {
    node.borrow()
        .children
        .iter()
        .filter_map(|child| {
            child
                .borrow()
                .key
                .as_deref()
                .unwrap_or("")
                .parse::<usize>()
                .ok()
        })
        .max()
        .map(|highest_index| highest_index + 1)
        .unwrap_or(0)
}

/// Parses a YAML file and attaches it to the parent as a child named after
/// the file (or subdirectory) it came from.
fn include_file_as_named_child(
    path: &Path,
    child_key: String,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    virtual_file_system: VirtualFileSystemReference,
) -> Result<(), Box<dyn Error>> {
    let file_path = normalize_path(path.to_str().unwrap());
    let content = virtual_file_system
        .lock()
        .unwrap()
        .get_file_content(&file_path)?;

    let child_node =
        parse_yaml_into_tree_node(&content, &file_path, child_key, Rc::clone(&parent_node))?;

    process_include_directives_depth_first(Rc::clone(&child_node), root_node, virtual_file_system)?;
    add_or_replace_child_node_in_parent(parent_node, child_node, false);

    Ok(())
}

fn parse_yaml_into_tree_node(
    content: &str,
    file_path: &str,
    key: String,
    parent_node: TreeNodeReference,
) -> Result<TreeNodeReference, Box<dyn Error>> {
    let marked_node = marked_yaml::parse_yaml(0, content)
        .map_err(|error| format!("YAML Parse Error in '{}': {}", file_path, error))?;

    let node = new_tree_node_reference(CoalTree {
        key: Some(key),
        value: Some(convert_marked_yaml_node_to_value(&marked_node)),
        parent: Some(Rc::downgrade(&parent_node)),
        origin: Some(file_path.to_string()),
        ..Default::default()
    });

    build_tree_from_marked_node(&marked_node, Rc::clone(&node), file_path);
    Ok(node)
}

// =====================================================================
// PATH RESOLUTION
// =====================================================================

/// Resolves an include path: Jinja expressions are substituted first, then
/// "./" and "../" prefixes are resolved relative to the file the directive
/// was declared in.
fn resolve_include_path(
    unresolved_path: &str,
    include_node: &TreeNodeReference,
    root_node: &TreeNodeReference,
) -> String {
    let jinja_resolved_path = resolve_jinja_reference_string(
        unresolved_path,
        Rc::clone(include_node),
        Rc::clone(root_node),
    );
    let origin_path = include_node.borrow().origin.clone();
    resolve_path_relative_to_origin(&jinja_resolved_path, &origin_path)
}

fn resolve_path_relative_to_origin(unresolved_path: &str, origin_path: &Option<String>) -> String {
    if (unresolved_path.starts_with("./") || unresolved_path.starts_with("../"))
        && let Some(origin_path) = origin_path
        && let Some(origin_directory) = Path::new(origin_path).parent()
    {
        let combined_path = origin_directory.join(Path::new(unresolved_path));
        return normalize_path(combined_path.to_str().unwrap());
    }
    normalize_path(unresolved_path)
}

fn include_target_paths(include_node: &TreeNodeReference) -> Vec<String> {
    include_node
        .borrow()
        .value
        .as_ref()
        .map(extract_string_list_from_value)
        .unwrap_or_default()
}

fn remove_processed_directive_nodes(
    parent_node: TreeNodeReference,
    processed_nodes: Vec<TreeNodeReference>,
) {
    if !processed_nodes.is_empty() {
        parent_node.borrow_mut().children.retain(|child| {
            !processed_nodes
                .iter()
                .any(|processed| Rc::ptr_eq(child, processed))
        });
    }
}

fn is_yaml_file(path: &Path) -> bool {
    path.is_file() && path.extension().and_then(|extension| extension.to_str()) == Some("yaml")
}
