use crate::coal_tree::CoalTree;
use crate::shared_logic::{
    add_or_replace_child_node_in_parent, build_tree_from_marked_node,
    extract_string_array_from_yaml_value, marked_node_to_serde_value, resolve_jinja_reference_string,
};
use crate::vfs::{normalize_path, VirtualFileSystem};
use std::cell::RefCell;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Traverses the CoalTree depth-first and processes special include directives.
pub fn process_all_include_functions_depth_first(
    current_tree_node: Rc<RefCell<CoalTree>>,
    root_tree_node: Rc<RefCell<CoalTree>>,
    vfs: Arc<Mutex<VirtualFileSystem>>,
) -> Result<(), Box<dyn Error>> {
    let children_references = current_tree_node.borrow().children.clone();
    let mut children_marked_for_removal = Vec::new();

    for child_reference in children_references {
        let was_processed = process_single_node_directive(
            Rc::clone(&child_reference),
            Rc::clone(&current_tree_node),
            Rc::clone(&root_tree_node),
            Arc::clone(&vfs),
        )?;

        if was_processed {
            children_marked_for_removal.push(Rc::clone(&child_reference));
        } else {
            // If it wasn't an include directive, continue recursion
            process_all_include_functions_depth_first(
                Rc::clone(&child_reference),
                Rc::clone(&root_tree_node),
                Arc::clone(&vfs),
            )?;
        }
    }

    remove_processed_children(current_tree_node, children_marked_for_removal);
    Ok(())
}

/// Identifies if a node is a directive and routes it to the appropriate handler.
fn process_single_node_directive(
    node: Rc<RefCell<CoalTree>>,
    parent: Rc<RefCell<CoalTree>>,
    root: Rc<RefCell<CoalTree>>,
    vfs: Arc<Mutex<VirtualFileSystem>>,
) -> Result<bool, Box<dyn Error>> {
    let node_key = node.borrow().key.clone();
    let key_string = match node_key {
        Some(key) => key,
        None => return Ok(false),
    };

    match key_string.as_str() {
        "$include" => handle_standard_file_include(node, parent, root, vfs, false)?,
        "$include_optional" => handle_standard_file_include(node, parent, root, vfs, true)?,
        "$include_directory" => handle_directory_include(node, parent, root, vfs, false)?,
        "$include_directory_optional" => handle_directory_include(node, parent, root, vfs, true)?,
        "$include_subdirectories" => handle_subdirectories_include(node, parent, root, vfs, false)?,
        "$include_subdirectories_optional" => {
            handle_subdirectories_include(node, parent, root, vfs, true)?
        }
        _ => return Ok(false),
    };

    Ok(true)
}

// --- Specialized Handlers ---

fn handle_standard_file_include(
    include_node: Rc<RefCell<CoalTree>>,
    parent_node: Rc<RefCell<CoalTree>>,
    root_node: Rc<RefCell<CoalTree>>,
    vfs: Arc<Mutex<VirtualFileSystem>>,
    is_optional: bool,
) -> Result<(), Box<dyn Error>> {
    let target_paths = get_paths_from_node(&include_node);

    for unresolved_path in target_paths {
        let fully_resolved_path =
            resolve_full_path(&unresolved_path, &include_node, &root_node);

        let file_content = match vfs.lock().unwrap().get_file_content(&fully_resolved_path) {
            Ok(content) => content,
            Err(error) => {
                if is_optional { continue; }
                return Err(error);
            }
        };

        process_and_integrate_yaml_file(
            &file_content,
            &fully_resolved_path,
            Rc::clone(&parent_node),
            Rc::clone(&root_node),
            Arc::clone(&vfs),
        )?;
    }
    Ok(())
}

fn handle_directory_include(
    include_node: Rc<RefCell<CoalTree>>,
    parent_node: Rc<RefCell<CoalTree>>,
    root_node: Rc<RefCell<CoalTree>>,
    vfs: Arc<Mutex<VirtualFileSystem>>,
    is_optional: bool,
) -> Result<(), Box<dyn Error>> {
    let target_paths = get_paths_from_node(&include_node);

    for unresolved_path in target_paths {
        let directory_path = resolve_full_path(&unresolved_path, &include_node, &root_node);

        let directory_entries = match fs::read_dir(Path::new(&directory_path)) {
            Ok(iter) => iter,
            Err(error) => {
                if is_optional { continue; }
                return Err(format!("Could not read include directory '{}': {}", directory_path, error).into());
            }
        };

        for entry in directory_entries {
            let path = entry?.path();
            if is_yaml_file(&path) {
                let file_stem = path.file_stem().unwrap().to_str().unwrap().to_string();
                process_named_child_file(
                    &path,
                    file_stem,
                    Rc::clone(&parent_node),
                    Rc::clone(&root_node),
                    Arc::clone(&vfs),
                )?;
            }
        }
    }
    Ok(())
}

fn handle_subdirectories_include(
    include_node: Rc<RefCell<CoalTree>>,
    parent_node: Rc<RefCell<CoalTree>>,
    root_node: Rc<RefCell<CoalTree>>,
    vfs: Arc<Mutex<VirtualFileSystem>>,
    is_optional: bool,
) -> Result<(), Box<dyn Error>> {
    let target_paths = get_paths_from_node(&include_node);

    for unresolved_path in target_paths {
        let base_directory = resolve_full_path(&unresolved_path, &include_node, &root_node);

        let entries = match fs::read_dir(Path::new(&base_directory)) {
            Ok(iter) => iter,
            Err(error) => {
                if is_optional { continue; }
                return Err(format!("Could not read subdirectories in '{}': {}", base_directory, error).into());
            }
        };

        for entry in entries {
            let subdir_path = entry?.path();
            if subdir_path.is_dir() {
                let dir_name = subdir_path.file_name().unwrap().to_str().unwrap().to_string();
                let yaml_file_path = subdir_path.join(format!("{}.yaml", dir_name));

                if yaml_file_path.exists() && yaml_file_path.is_file() {
                    process_named_child_file(
                        &yaml_file_path,
                        dir_name,
                        Rc::clone(&parent_node),
                        Rc::clone(&root_node),
                        Arc::clone(&vfs),
                    )?;
                }
            }
        }
    }
    Ok(())
}

// --- Helper Logic Functions ---

fn process_and_integrate_yaml_file(
    content: &str,
    file_path: &str,
    parent: Rc<RefCell<CoalTree>>,
    root: Rc<RefCell<CoalTree>>,
    vfs: Arc<Mutex<VirtualFileSystem>>,
) -> Result<(), Box<dyn Error>> {
    let marked_node = marked_yaml::parse_yaml(0, content)
        .map_err(|e| format!("YAML Parse Error in file '{}': {}", file_path, e))?;

    let temporary_root = Rc::new(RefCell::new(CoalTree {
        key: Some(String::from("TEMPORARY_ROOT")),
        value: Some(marked_node_to_serde_value(&marked_node)),
        origin: Some(file_path.to_string()),
        parent: Some(Rc::downgrade(&parent)),
        ..Default::default()
    }));

    build_tree_from_marked_node(&marked_node, Rc::clone(&temporary_root), file_path);
    process_all_include_functions_depth_first(Rc::clone(&temporary_root), root, vfs)?;

    let temp_children = temporary_root.borrow().children.clone();

    if temp_children.is_empty() {
        parent.borrow_mut().value = temporary_root.borrow().value.clone();
    } else {
        // NEW: Detect if the incoming file is a list
        let is_temp_list = temporary_root.borrow().is_list_type();

        // Find the highest numeric index currently in the parent to avoid overwriting
        let mut current_idx = parent.borrow().children.iter()
            .filter_map(|c| c.borrow().key.as_deref().unwrap_or("").parse::<usize>().ok())
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);

        for child in temp_children {
            child.borrow_mut().parent = Some(Rc::downgrade(&parent));

            if is_temp_list {
                // If it's a list, assign the next sequential index and append
                child.borrow_mut().key = Some(current_idx.to_string());
                parent.borrow_mut().children.push(Rc::clone(&child));
                current_idx += 1;
            } else {
                // If it's an object, merge it normally
                add_or_replace_child_node_in_parent(Rc::clone(&parent), child, false);
            }
        }
    }
    Ok(())
}

fn process_named_child_file(
    path: &Path,
    key_name: String,
    parent: Rc<RefCell<CoalTree>>,
    root: Rc<RefCell<CoalTree>>,
    vfs: Arc<Mutex<VirtualFileSystem>>,
) -> Result<(), Box<dyn Error>> {
    let file_path_string = normalize_path(path.to_str().unwrap());
    let content = vfs.lock().unwrap().get_file_content(&file_path_string)?;

    let marked_node = marked_yaml::parse_yaml(0, &content)
        .map_err(|e| format!("YAML Parse Error in '{}': {}", file_path_string, e))?;

    let child_node = Rc::new(RefCell::new(CoalTree {
        key: Some(key_name),
        value: Some(marked_node_to_serde_value(&marked_node)),
        parent: Some(Rc::downgrade(&parent)),
        origin: Some(file_path_string.clone()),
        ..Default::default()
    }));

    build_tree_from_marked_node(&marked_node, Rc::clone(&child_node), &file_path_string);
    process_all_include_functions_depth_first(Rc::clone(&child_node), root, vfs)?;
    add_or_replace_child_node_in_parent(parent, child_node, false);

    Ok(())
}

fn resolve_full_path(
    unresolved_path: &str,
    node: &Rc<RefCell<CoalTree>>,
    root: &Rc<RefCell<CoalTree>>,
) -> String {
    let jinja_resolved = resolve_jinja_reference_string(unresolved_path, Rc::clone(node), Rc::clone(root));
    let origin_path = node.borrow().origin.clone();
    resolve_relative_file_path_system_agnostic(&jinja_resolved, &origin_path)
}

fn resolve_relative_file_path_system_agnostic(
    unresolved_path: &str,
    origin_path_option: &Option<String>,
) -> String {
    let target_path = Path::new(unresolved_path);

    if unresolved_path.starts_with("./") || unresolved_path.starts_with("../") {
        if let Some(origin_path_string) = origin_path_option {
            if let Some(parent_directory) = Path::new(origin_path_string).parent() {
                let combined_path = parent_directory.join(target_path);
                return normalize_path(combined_path.to_str().unwrap());
            }
        }
    }
    normalize_path(unresolved_path)
}

fn get_paths_from_node(node: &Rc<RefCell<CoalTree>>) -> Vec<String> {
    node.borrow()
        .value
        .as_ref()
        .map(extract_string_array_from_yaml_value)
        .unwrap_or_default()
}

fn remove_processed_children(
    parent: Rc<RefCell<CoalTree>>,
    to_remove: Vec<Rc<RefCell<CoalTree>>>,
) {
    if !to_remove.is_empty() {
        parent.borrow_mut().children.retain(|existing_child| {
            !to_remove.iter().any(|removable| Rc::ptr_eq(existing_child, removable))
        });
    }
}

fn is_yaml_file(path: &Path) -> bool {
    path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("yaml")
}