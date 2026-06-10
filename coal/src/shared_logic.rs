use crate::coal_tree::{CoalTree, Span};
use crate::type_aliases::{TreeRef, new_tree_ref};
use marked_yaml::Node;
use regex::Regex;
use serde_yaml::Value;
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
    parent_node: TreeRef,
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
                let (line, col) = key_node
                    .span()
                    .start()
                    .map(|m| (m.line(), m.column()))
                    .unwrap_or((1, 1));
                
                process_child_yaml_node(
                    key_node.as_str().to_string(),
                    value_node,
                    &parent_node,
                    origin_path,
                    &file_stem,
                    line,
                    col,
                );
            }
        }
        Node::Sequence(sequence) => {
            for (index, item_node) in sequence.iter().enumerate() {
                let (line, col) = item_node
                    .span()
                    .start()
                    .map(|m| (m.line(), m.column()))
                    .unwrap_or((1, 1));
                
                process_child_yaml_node(
                    index.to_string(),
                    item_node,
                    &parent_node,
                    origin_path,
                    &file_stem,
                    line,
                    col,
                );
            }
        }
        _ => {}
    }
}

fn process_child_yaml_node(
    key: String,
    value_node: &Node,
    parent_node: &TreeRef,
    origin_path: &str,
    file_stem: &str,
    line: usize,
    col: usize,
) {
    let mut value = marked_node_to_serde_value(value_node);
    replace_variables_in_value(&mut value, file_stem);

    let child_node = new_tree_ref(CoalTree {
        key: Some(key),
        value: Some(value),
        parent: Some(Rc::downgrade(parent_node)),
        origin: Some(origin_path.to_string()),
        span: Some(Span { line, col }),
        ..Default::default()
    });

    add_or_replace_child_node_in_parent(Rc::clone(parent_node), Rc::clone(&child_node), false);

    if matches!(value_node, Node::Mapping(_) | Node::Sequence(_)) {
        build_tree_from_marked_node(value_node, child_node, origin_path);
    }
}

pub fn add_or_replace_child_node_in_parent(
    parent_node: TreeRef,
    new_child_node: TreeRef,
    preserve_leaves: bool,
) {
    let child_key = new_child_node.borrow().key.clone();
    let existing_child_option = find_child_by_key(&parent_node, &child_key);

    if let Some(existing_child) = existing_child_option {
        if should_merge_nodes(&existing_child, &new_child_node) {
            merge_children_into_existing(new_child_node, &existing_child, preserve_leaves);
        } else if !preserve_leaves {
            replace_child_in_parent(&parent_node, new_child_node, &child_key);
        }
    } else {
        parent_node.borrow_mut().children.push(new_child_node);
    }
}

fn find_child_by_key(parent_node: &TreeRef, key: &Option<String>) -> Option<TreeRef> {
    parent_node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key == *key)
        .cloned()
}

fn should_merge_nodes(existing_node: &TreeRef, new_node: &TreeRef) -> bool {
    let existing_is_obj = existing_node.borrow().is_object_type();
    let new_is_obj = new_node.borrow().is_object_type();

    let new_has_directives = new_node.borrow().children.iter().any(|c| {
        c.borrow().key.as_deref().unwrap_or("").starts_with('$')
    });

    (existing_is_obj && new_is_obj) || new_has_directives
}

fn merge_children_into_existing(
    source_node: TreeRef,
    target_node: &TreeRef,
    preserve_leaves: bool,
) {
    let children_to_transfer = source_node.borrow().children.clone();
    for child in children_to_transfer {
        child.borrow_mut().parent = Some(Rc::downgrade(target_node));
        add_or_replace_child_node_in_parent(Rc::clone(target_node), child, preserve_leaves);
    }
    
    if !preserve_leaves {
        let new_origin = source_node.borrow().origin.clone();
        if new_origin.is_some() {
            target_node.borrow_mut().origin = new_origin;
        }
    }
}

fn replace_child_in_parent(
    parent_node: &TreeRef,
    new_child_node: TreeRef,
    child_key: &Option<String>,
) {
    let mut parent_borrowed = parent_node.borrow_mut();
    if let Some(position) = parent_borrowed
        .children
        .iter()
        .position(|child| child.borrow().key == *child_key)
    {
        parent_borrowed.children[position] = new_child_node;
    }
}

pub fn clone_tree_node_recursively(
    source_node: TreeRef,
    parent_node: TreeRef,
) -> TreeRef {
    let source = source_node.borrow();
    let cloned_node = new_tree_ref(CoalTree {
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
    });

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
    current_node: TreeRef,
    root_node: TreeRef,
) -> Option<TreeRef> {
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

fn traverse_path(node: TreeRef, path: &str) -> Option<TreeRef> {
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
    current_node: TreeRef,
    root_node: TreeRef,
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
// ID DIRECTIVE RESOLUTION ($id_now / $id / $id_end)
// =====================================================================

/// Checks whether `node` carries a child with the given id directive key
/// (e.g. "$id_now", "$id", "$id_end"). If so, replaces the node's value
/// with the next sequential integer for that counter name and removes the
/// directive child. The counter name itself may contain Jinja references.
pub fn resolve_id_directive_for_node(
    node: TreeRef,
    root_node: TreeRef,
    directive_key: &str,
    counters: &mut std::collections::HashMap<String, i64>,
) {
    let mut id_key_to_process = None;

    {
        let borrowed = node.borrow();
        for child in &borrowed.children {
            let child_borrowed = child.borrow();
            if child_borrowed.key.as_deref() == Some(directive_key) {
                if let Some(Value::String(ref s)) = child_borrowed.value {
                    id_key_to_process = Some(s.clone());
                    break;
                }
            }
        }
    }

    if let Some(raw_id_key) = id_key_to_process {
        let resolved_id_key =
            resolve_jinja_reference_string(&raw_id_key, Rc::clone(&node), Rc::clone(&root_node));
        let count = counters.entry(resolved_id_key).or_insert(1);

        let mut borrowed_mut = node.borrow_mut();
        borrowed_mut.value = Some(Value::Number((*count).into()));
        borrowed_mut
            .children
            .retain(|c| c.borrow().key.as_deref() != Some(directive_key));

        *count += 1;
    }
}

/// Depth-first pass that resolves every occurrence of the given id directive
/// in the (sub)tree. Used for "$id" (right after the include phase) and
/// "$id_end" (once the whole tree has been built by the AST phase).
pub fn resolve_id_directives_depth_first(
    node: TreeRef,
    root_node: TreeRef,
    directive_key: &str,
    counters: &mut std::collections::HashMap<String, i64>,
) {
    resolve_id_directive_for_node(
        Rc::clone(&node),
        Rc::clone(&root_node),
        directive_key,
        counters,
    );

    let children = node.borrow().children.clone();
    for child in children {
        resolve_id_directives_depth_first(child, Rc::clone(&root_node), directive_key, counters);
    }
}

// =====================================================================
// ALPHABETICAL ORDERING
// =====================================================================

/// Recursively sorts the tree so that sibling keys appear in alphabetical
/// order. List nodes (numeric keys) are sorted numerically instead, so that
/// item 10 does not jump in front of item 2. Raw YAML mapping values held on
/// leaf nodes are sorted as well so printed output is always alphabetical.
pub fn sort_tree_alphabetically(node: TreeRef) {
    {
        let mut borrowed = node.borrow_mut();

        if borrowed.is_list_type() {
            borrowed.children.sort_by_key(|child| {
                child
                    .borrow()
                    .key
                    .as_deref()
                    .and_then(|k| k.parse::<usize>().ok())
                    .unwrap_or(usize::MAX)
            });
        } else {
            borrowed.children.sort_by(|a, b| {
                let key_a = a.borrow().key.clone().unwrap_or_default();
                let key_b = b.borrow().key.clone().unwrap_or_default();
                key_a.cmp(&key_b)
            });
        }

        if let Some(ref mut value) = borrowed.value {
            sort_yaml_value_keys_recursively(value);
        }
    }

    let children = node.borrow().children.clone();
    for child in children {
        sort_tree_alphabetically(child);
    }
}

/// Sorts all mapping keys inside a raw serde_yaml Value alphabetically.
pub fn sort_yaml_value_keys_recursively(value: &mut Value) {
    match value {
        Value::Mapping(mapping) => {
            let mut entries: Vec<(Value, Value)> = mapping
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            entries.sort_by(|a, b| yaml_key_as_string(&a.0).cmp(&yaml_key_as_string(&b.0)));

            let mut sorted_mapping = serde_yaml::Mapping::new();
            for (key, mut val) in entries {
                sort_yaml_value_keys_recursively(&mut val);
                sorted_mapping.insert(key, val);
            }
            *mapping = sorted_mapping;
        }
        Value::Sequence(sequence) => {
            for item in sequence.iter_mut() {
                sort_yaml_value_keys_recursively(item);
            }
        }
        _ => {}
    }
}

fn yaml_key_as_string(key: &Value) -> String {
    match key {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

// =====================================================================
// TREE SYNCHRONIZATION AND CLEANUP
// =====================================================================

pub fn synchronize_node_values_depth_first(node: TreeRef) {
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

pub fn resolve_all_jinja_values_in_tree(node: TreeRef, root: TreeRef) {
    let mut visited_paths = std::collections::HashSet::new();
    resolve_node_jinja_recursive(node, root, &mut visited_paths);
}

fn resolve_node_jinja_recursive(
    node: TreeRef,
    root: TreeRef,
    visited: &mut std::collections::HashSet<String>,
) {
    let current_path = node.borrow().get_path();
    if visited.contains(&current_path) {
        return;
    }
    visited.insert(current_path.clone());

    process_jinja_children(&node, &root, visited);

    let substitution_occurred = process_jinja_node_value(&node, &root, visited);

    visited.remove(&current_path);
    if substitution_occurred {
        resolve_node_jinja_recursive(node, root, visited);
    }
}

fn process_jinja_children(
    node: &TreeRef,
    root: &TreeRef,
    visited: &mut std::collections::HashSet<String>,
) {
    let children_references = node.borrow().children.clone();
    for child in children_references {
        resolve_node_jinja_recursive(child, Rc::clone(root), visited);
    }
}

fn process_jinja_node_value(
    node: &TreeRef,
    root: &TreeRef,
    visited: &mut std::collections::HashSet<String>,
) -> bool {
    let node_value_as_string = node.borrow().value.as_ref().and_then(|v| v.as_str()).map(|s| s.to_string());

    if let Some(content_string) = node_value_as_string {
        if let Some(captures) = get_exact_regex().captures(&content_string) {
            return process_exact_jinja_match(node, root, visited, &captures[1]);
        } else if get_partial_regex().is_match(&content_string) {
            return process_partial_jinja_match(node, root, visited, &content_string);
        }
    }
    false
}

fn process_exact_jinja_match(
    node: &TreeRef,
    root: &TreeRef,
    visited: &mut std::collections::HashSet<String>,
    reference_path: &str,
) -> bool {
    let reference_path = reference_path.trim();
    if let Some(target) = find_target_node_using_reference_path(reference_path, Rc::clone(node), Rc::clone(root)) {
        let target_path = target.borrow().get_path();

        if !visited.contains(&target_path) {
            resolve_node_jinja_recursive(Rc::clone(&target), Rc::clone(root), visited);
            apply_target_node_substitution(node, &target);
            return true;
        }
    }
    false
}

fn apply_target_node_substitution(
    node: &TreeRef,
    target: &TreeRef,
) {
    let (target_value, target_children) = {
        let borrowed_target = target.borrow();
        (borrowed_target.value.clone(), borrowed_target.children.clone())
    };

    let mut node_mut = node.borrow_mut();
    node_mut.value = target_value;
    node_mut.children.clear();
    drop(node_mut);

    for child in target_children {
        let cloned = clone_tree_node_recursively(child, Rc::clone(node));
        node.borrow_mut().children.push(cloned);
    }
}

fn process_partial_jinja_match(
    node: &TreeRef,
    root: &TreeRef,
    visited: &mut std::collections::HashSet<String>,
    content_string: &str,
) -> bool {
    let partial_regex = get_partial_regex();
    for captures in partial_regex.captures_iter(content_string) {
        let reference_path = captures[1].trim();
        if let Some(target) = find_target_node_using_reference_path(
            reference_path,
            Rc::clone(node),
            Rc::clone(root)
        ) {
            let target_path = target.borrow().get_path();
            if !visited.contains(&target_path) {
                resolve_node_jinja_recursive(Rc::clone(&target), Rc::clone(root), visited);
            }
        }
    }

    let resolved_string = resolve_jinja_reference_string(content_string, Rc::clone(node), Rc::clone(root));
    if resolved_string != content_string {
        node.borrow_mut().value = Some(Value::String(resolved_string));
        return true;
    }
    false
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

pub fn clean_up_system_directives(node: TreeRef) {
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

pub fn prune_empty_mapping_nodes(node: TreeRef) {
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