use crate::coal_tree::{CoalTree, Span};
use marked_yaml::Node;
use regex::Regex;
use serde_yaml::Value;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::OnceLock;

static REGEX_JINJA_PARTIAL: OnceLock<Regex> = OnceLock::new();
static REGEX_JINJA_EXACT: OnceLock<Regex> = OnceLock::new();

fn get_partial_regex() -> &'static Regex {
    REGEX_JINJA_PARTIAL.get_or_init(|| Regex::new(r"\{\{\s*([^}]+)\s*\}\}").unwrap())
}

fn get_exact_regex() -> &'static Regex {
    REGEX_JINJA_EXACT.get_or_init(|| Regex::new(r"^\{\{\s*([^}]+)\s*\}\}$").unwrap())
}

// =====================================================================
// YAML CONVERSION AND VARIABLE REPLACEMENT
// =====================================================================

pub fn marked_node_to_serde_value(node: &Node) -> Value {
    match node {
        Node::Scalar(scalar) => {
            let text = scalar.as_str();
            if let Ok(boolean) = text.parse::<bool>() {
                Value::Bool(boolean)
            } else if let Ok(integer) = text.parse::<i64>() {
                Value::Number(integer.into())
            } else if let Ok(float) = text.parse::<f64>() {
                Value::Number(float.into())
            } else {
                Value::String(text.to_string())
            }
        }
        Node::Sequence(sequence) => {
            Value::Sequence(sequence.iter().map(marked_node_to_serde_value).collect())
        }
        Node::Mapping(mapping) => {
            let mut mapping_object = serde_yaml::Mapping::new();
            for (key, value) in mapping.iter() {
                mapping_object.insert(
                    Value::String(key.as_str().to_string()),
                    marked_node_to_serde_value(value),
                );
            }
            Value::Mapping(mapping_object)
        }
    }
}

pub fn replace_variables_in_value(value: &mut Value, stem: &str) {
    match value {
        Value::String(string_content) => {
            if !stem.is_empty() {
                *string_content = string_content.replace("$filename", stem);
            }
        }
        Value::Sequence(sequence) => {
            for item in sequence {
                replace_variables_in_value(item, stem);
            }
        }
        Value::Mapping(mapping) => {
            for (_, val) in mapping {
                replace_variables_in_value(val, stem);
            }
        }
        _ => {}
    }
}

// =====================================================================
// TREE BUILDING AND MANIPULATION
// =====================================================================

pub fn build_tree_from_marked_node(
    yaml_node: &Node,
    parent_node: Rc<RefCell<CoalTree>>,
    origin_path: &str,
) {
    let file_stem = Path::new(origin_path)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    match yaml_node {
        Node::Mapping(mapping) => {
            for (key_node, value_node) in mapping.iter() {
                let mut value = marked_node_to_serde_value(value_node);
                replace_variables_in_value(&mut value, &file_stem);

                let (line, col) = key_node
                    .span()
                    .start()
                    .map(|m| (m.line(), m.column()))
                    .unwrap_or((1, 1));

                let child_node = Rc::new(RefCell::new(CoalTree {
                    key: Some(key_node.as_str().to_string()),
                    value: Some(value),
                    parent: Some(Rc::downgrade(&parent_node)),
                    origin: Some(origin_path.to_string()),
                    span: Some(Span { line, col }),
                    ..Default::default()
                }));

                add_or_replace_child_node_in_parent(Rc::clone(&parent_node), Rc::clone(&child_node), false);

                if matches!(value_node, Node::Mapping(_) | Node::Sequence(_)) {
                    build_tree_from_marked_node(value_node, child_node, origin_path);
                }
            }
        }
        Node::Sequence(sequence) => {
            for (index, item_node) in sequence.iter().enumerate() {
                let mut value = marked_node_to_serde_value(item_node);
                replace_variables_in_value(&mut value, &file_stem);

                let (line, col) = item_node
                    .span()
                    .start()
                    .map(|m| (m.line(), m.column()))
                    .unwrap_or((1, 1));

                let child_node = Rc::new(RefCell::new(CoalTree {
                    key: Some(index.to_string()),
                    value: Some(value),
                    parent: Some(Rc::downgrade(&parent_node)),
                    origin: Some(origin_path.to_string()),
                    span: Some(Span { line, col }),
                    ..Default::default()
                }));

                add_or_replace_child_node_in_parent(Rc::clone(&parent_node), Rc::clone(&child_node), false);

                if matches!(item_node, Node::Mapping(_) | Node::Sequence(_)) {
                    build_tree_from_marked_node(item_node, child_node, origin_path);
                }
            }
        }
        _ => {}
    }
}

pub fn add_or_replace_child_node_in_parent(
    parent_node: Rc<RefCell<CoalTree>>,
    new_child_node: Rc<RefCell<CoalTree>>,
    preserve_leaves: bool,
) {
    let child_key = new_child_node.borrow().key.clone();
    let existing_child_option = parent_node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key == child_key)
        .cloned();

    if let Some(existing_child) = existing_child_option {
        let existing_is_obj = existing_child.borrow().is_object_type();
        let new_is_obj = new_child_node.borrow().is_object_type();

        // NEW: Check if the incoming node is injecting system directives ($each, $append, etc.)
        let new_has_directives = new_child_node.borrow().children.iter().any(|c| {
            c.borrow().key.as_deref().unwrap_or("").starts_with('$')
        });

        // Merge if both are objects OR if the new node is dropping directives into an existing list/node
        if (existing_is_obj && new_is_obj) || new_has_directives {
            let children_to_transfer = new_child_node.borrow().children.clone();
            for child in children_to_transfer {
                child.borrow_mut().parent = Some(Rc::downgrade(&existing_child));
                add_or_replace_child_node_in_parent(Rc::clone(&existing_child), child, preserve_leaves);
            }
            if !preserve_leaves {
                let new_origin = new_child_node.borrow().origin.clone();
                if new_origin.is_some() {
                    existing_child.borrow_mut().origin = new_origin;
                }
            }
        } else if !preserve_leaves {
            let mut parent_borrowed = parent_node.borrow_mut();
            if let Some(position) = parent_borrowed
                .children
                .iter()
                .position(|child| child.borrow().key == child_key)
            {
                parent_borrowed.children[position] = new_child_node;
            }
        }
    } else {
        parent_node.borrow_mut().children.push(new_child_node);
    }
}

pub fn clone_tree_node_recursively(
    source_node: Rc<RefCell<CoalTree>>,
    parent_node: Rc<RefCell<CoalTree>>,
) -> Rc<RefCell<CoalTree>> {
    let source = source_node.borrow();
    let cloned_node = Rc::new(RefCell::new(CoalTree {
        key: source.key.clone(),
        value: source.value.clone(),
        parent: Some(Rc::downgrade(&parent_node)),
        field_type: source.field_type.clone(),
        origin: source.origin.clone(),
        schema_origin: source.schema_origin.clone(),
        optional: source.optional,
        default_value: source.default_value.clone(),
        allowed_values: source.allowed_values.clone(),
        allowed: source.allowed,
        children: Vec::new(),
        span: source.span.clone(),
    }));

    for child in &source.children {
        let cloned_child = clone_tree_node_recursively(Rc::clone(child), Rc::clone(&cloned_node));
        cloned_node.borrow_mut().children.push(cloned_child);
    }
    cloned_node
}

// =====================================================================
// PATH NAVIGATION AND JINJA RESOLUTION
// =====================================================================

pub fn find_target_node_using_reference_path(
    path: &str,
    current_node: Rc<RefCell<CoalTree>>,
    root_node: Rc<RefCell<CoalTree>>,
) -> Option<Rc<RefCell<CoalTree>>> {
    if path.starts_with("ROOT.") {
        return traverse_path(root_node, &path[5..]);
    }

    if path.starts_with('.') {
        let dot_count = path.chars().take_while(|&c| c == '.').count();
        let mut navigation_node = current_node;

        for _ in 0..dot_count {
            // FIX: Drop the borrow of navigation_node before reassigning it
            let next_parent = {
                let node_borrowed = navigation_node.borrow();
                node_borrowed.parent.as_ref().and_then(|weak| weak.upgrade())
            };
            navigation_node = next_parent?;
        }

        return if path[dot_count..].is_empty() {
            Some(navigation_node)
        } else {
            traverse_path(navigation_node, &path[dot_count..])
        };
    }

    // Upward scoping search
    let mut search_scope = Some(current_node);
    while let Some(node) = search_scope {
        if let Some(found) = traverse_path(Rc::clone(&node), path) {
            return Some(found);
        }
        search_scope = {
            let node_borrowed = node.borrow();
            node_borrowed.parent.as_ref().and_then(|weak| weak.upgrade())
        };
    }

    traverse_path(root_node, path)
}

fn traverse_path(node: Rc<RefCell<CoalTree>>, path: &str) -> Option<Rc<RefCell<CoalTree>>> {
    let mut current_search_node = node;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        let found_child = current_search_node
            .borrow()
            .children
            .iter()
            .find(|child| child.borrow().key.as_deref() == Some(segment))
            .cloned();
        current_search_node = found_child?;
    }
    Some(current_search_node)
}

pub fn resolve_jinja_reference_string(
    input: &str,
    current_node: Rc<RefCell<CoalTree>>,
    root_node: Rc<RefCell<CoalTree>>,
) -> String {
    let mut resolved_result = input.to_string();
    let partial_regex = get_partial_regex();

    for captures in partial_regex.captures_iter(input) {
        let reference_path = captures[1].trim();
        let placeholder = &captures[0];

        if let Some(target) = find_target_node_using_reference_path(
            reference_path,
            Rc::clone(&current_node),
            Rc::clone(&root_node),
        ) {
            let target_value_string = match &target.borrow().value {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::Bool(b)) => b.to_string(),
                _ => String::new(),
            };

            if !target_value_string.is_empty() {
                resolved_result = resolved_result.replace(placeholder, &target_value_string);
            }
        }
    }
    resolved_result
}

// =====================================================================
// TREE SYNCHRONIZATION AND CLEANUP
// =====================================================================

pub fn synchronize_node_values_depth_first(node: Rc<RefCell<CoalTree>>) {
    let children = node.borrow().children.clone();
    for child in children {
        synchronize_node_values_depth_first(child);
    }

    let mut node_mut = node.borrow_mut();
    if node_mut.children.is_empty() {
        return;
    }

    if node_mut.is_list_type() {
        let sequence = node_mut
            .children
            .iter()
            .map(|child| child.borrow().value.clone().unwrap_or(Value::Null))
            .collect();
        node_mut.value = Some(Value::Sequence(sequence));
    } else {
        let mut mapping = serde_yaml::Mapping::new();
        for child in &node_mut.children {
            let key = child.borrow().key.clone().unwrap_or_default();
            if !key.starts_with('$') {
                mapping.insert(
                    Value::String(key),
                    child.borrow().value.clone().unwrap_or(Value::Null),
                );
            }
        }
        node_mut.value = if mapping.is_empty() {
            Some(Value::Null)
        } else {
            Some(Value::Mapping(mapping))
        };
    }
}

pub fn resolve_all_jinja_values_in_tree(node: Rc<RefCell<CoalTree>>, root: Rc<RefCell<CoalTree>>) {
    let mut visited_paths = std::collections::HashSet::new();
    resolve_node_jinja_recursive(node, root, &mut visited_paths);
}

fn resolve_node_jinja_recursive(
    node: Rc<RefCell<CoalTree>>,
    root: Rc<RefCell<CoalTree>>,
    visited: &mut std::collections::HashSet<String>,
) {
    let current_path = node.borrow().get_path();
    if visited.contains(&current_path) {
        // Prevent infinite loops in case of circular references
        return;
    }
    visited.insert(current_path.clone());

    // 1. Process children first (Depth-First)
    let children_references = node.borrow().children.clone();
    for child in children_references {
        resolve_node_jinja_recursive(child, Rc::clone(&root), visited);
    }

    // 2. Identify if the current node value is a Jinja reference
    let mut substitution_occurred = false;
    let node_value_as_string = node.borrow().value.as_ref().and_then(|v| v.as_str()).map(|s| s.to_string());

    if let Some(content_string) = node_value_as_string {
        if let Some(captures) = get_exact_regex().captures(&content_string) {
            let reference_path = captures[1].trim();
            if let Some(target) = find_target_node_using_reference_path(reference_path, Rc::clone(&node), Rc::clone(&root)) {

                let target_path = target.borrow().get_path();

                if !visited.contains(&target_path) {
                    resolve_node_jinja_recursive(Rc::clone(&target), Rc::clone(&root), visited);

                    let (target_value, target_children) = {
                        let borrowed_target = target.borrow();
                        (borrowed_target.value.clone(), borrowed_target.children.clone())
                    };

                    let mut node_mut = node.borrow_mut();
                    node_mut.value = target_value;
                    node_mut.children.clear();
                    drop(node_mut);

                    for child in target_children {
                        let cloned = clone_tree_node_recursively(child, Rc::clone(&node));
                        node.borrow_mut().children.push(cloned);
                    }
                    substitution_occurred = true;
                }
            }
        } else if get_partial_regex().is_match(&content_string) {
            // Case B: Partial Match - Replace placeholders within the string

            // NEW: Pre-resolve all target nodes in their native scope before extracting their values
            let partial_regex = get_partial_regex();
            for captures in partial_regex.captures_iter(&content_string) {
                let reference_path = captures[1].trim();
                if let Some(target) = find_target_node_using_reference_path(
                    reference_path,
                    Rc::clone(&node),
                    Rc::clone(&root)
                ) {
                    let target_path = target.borrow().get_path();
                    if !visited.contains(&target_path) {
                        resolve_node_jinja_recursive(Rc::clone(&target), Rc::clone(&root), visited);
                    }
                }
            }

            // Now that targets are fully resolved, extract their strings
            let resolved_string = resolve_jinja_reference_string(&content_string, Rc::clone(&node), Rc::clone(&root));
            if resolved_string != content_string {
                node.borrow_mut().value = Some(Value::String(resolved_string));
                substitution_occurred = true;
            }
        }
    }

    visited.remove(&current_path);
    if substitution_occurred {
        resolve_node_jinja_recursive(node, root, visited);
    }
}

pub fn strip_system_directives_from_value(value: &mut Value) {
    match value {
        Value::Mapping(map) => {
            // Collect keys to remove (avoids mutable borrow conflicts while iterating)
            let mut keys_to_remove = Vec::new();
            for key in map.keys() {
                if let Value::String(s) = key {
                    if s.starts_with('$') {
                        keys_to_remove.push(key.clone());
                    }
                }
            }
            for key in keys_to_remove {
                map.remove(&key);
            }
            for (_, v) in map.iter_mut() {
                strip_system_directives_from_value(v);
            }
        }
        Value::Sequence(seq) => {
            for v in seq.iter_mut() {
                strip_system_directives_from_value(v);
            }
        }
        _ => {}
    }
}

pub fn clean_up_system_directives(node: Rc<RefCell<CoalTree>>) {
    {
        let mut borrowed = node.borrow_mut();

        // 1. Remove unprocessed directive children from the tree structure
        borrowed.children.retain(|child| {
            !child.borrow().key.as_ref().map_or(false, |k| k.starts_with('$'))
        });

        // 2. Scrub unprocessed directive keys from the underlying YAML value fallback
        if let Some(ref mut val) = borrowed.value {
            strip_system_directives_from_value(val);
        }
    }

    // Recurse down the remaining valid tree
    let remaining_children = node.borrow().children.clone();
    for child in remaining_children {
        clean_up_system_directives(child);
    }
}

pub fn deep_merge_yaml_values(base: &Value, override_value: &Value) -> Value {
    if let (Value::Mapping(base_map), Value::Mapping(override_map)) = (base, override_value) {
        let mut merged_map = base_map.clone();
        for (key, value) in override_map {
            if let Some(base_value) = merged_map.get_mut(key) {
                *base_value = deep_merge_yaml_values(base_value, value);
            } else {
                merged_map.insert(key.clone(), value.clone());
            }
        }
        Value::Mapping(merged_map)
    } else {
        override_value.clone()
    }
}

pub fn extract_string_array_from_yaml_value(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Sequence(seq) => seq
            .iter()
            .filter_map(|item| item.as_str().map(|s| s.to_string()))
            .collect(),
        _ => vec![],
    }
}

pub fn prune_empty_mapping_nodes(node: Rc<RefCell<CoalTree>>) {
    // 1. Traverse bottom-up: recursively clean children first
    let children = node.borrow().children.clone();
    for child in children {
        prune_empty_mapping_nodes(child);
    }

    // 2. Prune current node's children if they are empty mappings
    node.borrow_mut().children.retain(|child| {
        let child_borrowed = child.borrow();

        // Check if there are no AST child nodes
        let has_no_ast_children = child_borrowed.children.is_empty();

        // Check if the underlying raw YAML value is also empty or missing
        let has_no_raw_data = match &child_borrowed.value {
            Some(serde_yaml::Value::Mapping(m)) => m.is_empty(),
            None => true,
            _ => false, // If it's a string, number, or list, it's not an empty mapping
        };

        // Determine if this node is considered an object
        let is_object = child_borrowed.is_object_type() || child_borrowed.field_type.as_deref() == Some("object");

        // It is an empty mapping if all three conditions are met
        let is_empty_mapping = has_no_ast_children && has_no_raw_data && is_object;

        // Keep the node if it is NOT an empty mapping
        !is_empty_mapping
    });
}