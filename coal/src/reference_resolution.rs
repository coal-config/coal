use crate::tree_operations::clone_tree_node_recursively;
use crate::type_aliases::TreeNodeReference;
use regex::Regex;
use serde_yaml::Value;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::OnceLock;

static JINJA_EXPRESSION_REGEX: OnceLock<Regex> = OnceLock::new();
static JINJA_EXACT_MATCH_REGEX: OnceLock<Regex> = OnceLock::new();

/// Matches any Jinja expression ("{{ ... }}") inside a string.
pub fn jinja_expression_regex() -> &'static Regex {
    JINJA_EXPRESSION_REGEX.get_or_init(|| Regex::new(r"\{\{\s*([^}]+)\s*\}\}").unwrap())
}

/// Matches strings that consist of exactly one Jinja expression and nothing else.
pub fn jinja_exact_match_regex() -> &'static Regex {
    JINJA_EXACT_MATCH_REGEX.get_or_init(|| Regex::new(r"^\{\{\s*([^}]+)\s*\}\}$").unwrap())
}

// =====================================================================
// PATH NAVIGATION
// =====================================================================

/// Resolves a reference path to a tree node. Three addressing modes exist:
/// "ROOT."-prefixed paths resolve from the tree root, "."-prefixed paths
/// climb one parent per leading dot, and bare paths search upward through
/// every enclosing scope before falling back to the root.
pub fn find_target_node_using_reference_path(
    path: &str,
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> Option<TreeNodeReference> {
    if let Some(path_from_root) = path.strip_prefix("ROOT.") {
        return traverse_path_segments(root_node, path_from_root);
    }

    if path.starts_with('.') {
        return resolve_relative_dot_path(path, current_node);
    }

    resolve_path_by_upward_scope_search(path, current_node, root_node)
}

fn resolve_relative_dot_path(
    path: &str,
    current_node: TreeNodeReference,
) -> Option<TreeNodeReference> {
    let leading_dot_count = path
        .chars()
        .take_while(|&character| character == '.')
        .count();
    let mut navigation_node = current_node;

    for _ in 0..leading_dot_count {
        let parent = get_parent_node(&navigation_node);
        navigation_node = parent?;
    }

    let remaining_path = &path[leading_dot_count..];
    if remaining_path.is_empty() {
        Some(navigation_node)
    } else {
        traverse_path_segments(navigation_node, remaining_path)
    }
}

fn resolve_path_by_upward_scope_search(
    path: &str,
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> Option<TreeNodeReference> {
    let mut search_scope = Some(current_node);
    while let Some(scope_node) = search_scope {
        if let Some(found_node) = traverse_path_segments(Rc::clone(&scope_node), path) {
            return Some(found_node);
        }
        search_scope = get_parent_node(&scope_node);
    }

    traverse_path_segments(root_node, path)
}

fn get_parent_node(node: &TreeNodeReference) -> Option<TreeNodeReference> {
    node.borrow()
        .parent
        .as_ref()
        .and_then(|weak_parent| weak_parent.upgrade())
}

fn traverse_path_segments(start_node: TreeNodeReference, path: &str) -> Option<TreeNodeReference> {
    let mut current_node = start_node;
    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        let matching_child = current_node
            .borrow()
            .children
            .iter()
            .find(|child| child.borrow().key.as_deref() == Some(segment))
            .cloned();
        current_node = matching_child?;
    }
    Some(current_node)
}

// =====================================================================
// $parent PLACEHOLDERS
// =====================================================================

static PARENT_PLACEHOLDER_REGEX: OnceLock<Regex> = OnceLock::new();

/// Matches "$parent" optionally followed by the number of levels to walk up
/// (e.g. "$parent", "$parent2", "$parent10").
fn parent_placeholder_regex() -> &'static Regex {
    PARENT_PLACEHOLDER_REGEX.get_or_init(|| Regex::new(r"\$parent(\d*)").unwrap())
}

/// Replaces every "$parentN" placeholder in `text` with the key of the
/// ancestor `N` levels above `node`. Bare "$parent" is equivalent to
/// "$parent1" (the immediate parent). Placeholders that reach past the tree
/// root are left in place. Resolution is anchored at `node`, so the caller
/// chooses the field the placeholders are relative to.
pub fn resolve_parent_placeholders(text: &str, node: &TreeNodeReference) -> String {
    if !text.contains("$parent") {
        return text.to_string();
    }

    parent_placeholder_regex()
        .replace_all(
            text,
            |captures: &regex::Captures| match parse_parent_placeholder_levels(&captures[1]) {
                Some(levels_up) => ancestor_key_at_level(node, levels_up)
                    .unwrap_or_else(|| captures[0].to_string()),
                None => captures[0].to_string(),
            },
        )
        .into_owned()
}

/// An empty suffix means one level up; "0" is meaningless and left alone.
fn parse_parent_placeholder_levels(digits: &str) -> Option<usize> {
    if digits.is_empty() {
        return Some(1);
    }
    match digits.parse::<usize>() {
        Ok(0) | Err(_) => None,
        Ok(levels_up) => Some(levels_up),
    }
}

fn ancestor_key_at_level(node: &TreeNodeReference, levels_up: usize) -> Option<String> {
    let mut ancestor = Rc::clone(node);
    for _ in 0..levels_up {
        ancestor = get_parent_node(&ancestor)?;
    }
    ancestor.borrow().key.clone()
}

// =====================================================================
// JINJA STRING RESOLUTION
// =====================================================================

/// Replaces every resolvable Jinja expression inside the input string with
/// the scalar value of the referenced node. Unresolvable expressions are
/// left in place.
pub fn resolve_jinja_reference_string(
    input: &str,
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> String {
    let mut resolved_result = input.to_string();

    for captures in jinja_expression_regex().captures_iter(input) {
        let reference_path = captures[1].trim();
        let placeholder = &captures[0];

        if let Some(target_node) = find_target_node_using_reference_path(
            reference_path,
            Rc::clone(&current_node),
            Rc::clone(&root_node),
        ) {
            let target_text = scalar_value_as_string(&target_node.borrow().value);
            if !target_text.is_empty() {
                resolved_result = resolved_result.replace(placeholder, &target_text);
            }
        }
    }
    resolved_result
}

fn scalar_value_as_string(value: &Option<Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(boolean)) => boolean.to_string(),
        _ => String::new(),
    }
}

// =====================================================================
// TREE-WIDE JINJA RESOLUTION
// =====================================================================

/// Resolves all Jinja templates in the tree. Values that consist of exactly
/// one expression adopt the referenced node's full structure; values with
/// embedded expressions receive string substitution.
///
/// Reference cycles (a field that resolves, directly or transitively, back to
/// itself) are detected via a visited-path set: the loop is broken, the
/// offending template is left unresolved, and a warning naming each cyclic
/// reference is printed once.
pub fn resolve_all_jinja_values_in_tree(node: TreeNodeReference, root_node: TreeNodeReference) {
    let mut visited_paths = HashSet::new();
    let mut circular_references = HashSet::new();
    resolve_jinja_values_recursively(
        node,
        root_node,
        &mut visited_paths,
        &mut circular_references,
    );
    report_circular_references(&circular_references);
}

fn report_circular_references(circular_references: &HashSet<String>) {
    for reference in circular_references {
        eprintln!(
            "Warning: circular Jinja reference left unresolved ({}).",
            reference
        );
    }
}

fn resolve_jinja_values_recursively(
    node: TreeNodeReference,
    root_node: TreeNodeReference,
    visited_paths: &mut HashSet<String>,
    circular_references: &mut HashSet<String>,
) {
    let current_path = node.borrow().get_path();
    if visited_paths.contains(&current_path) {
        return;
    }
    visited_paths.insert(current_path.clone());

    resolve_jinja_values_in_children(&node, &root_node, visited_paths, circular_references);

    let substitution_occurred =
        resolve_jinja_value_of_node(&node, &root_node, visited_paths, circular_references);

    visited_paths.remove(&current_path);
    if substitution_occurred {
        resolve_jinja_values_recursively(node, root_node, visited_paths, circular_references);
    }
}

fn resolve_jinja_values_in_children(
    node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    visited_paths: &mut HashSet<String>,
    circular_references: &mut HashSet<String>,
) {
    let children = node.borrow().children.clone();
    for child in children {
        resolve_jinja_values_recursively(
            child,
            Rc::clone(root_node),
            visited_paths,
            circular_references,
        );
    }
}

fn resolve_jinja_value_of_node(
    node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    visited_paths: &mut HashSet<String>,
    circular_references: &mut HashSet<String>,
) -> bool {
    let node_text = node
        .borrow()
        .value
        .as_ref()
        .and_then(|value| value.as_str())
        .map(|text| text.to_string());

    if let Some(text) = node_text {
        if let Some(captures) = jinja_exact_match_regex().captures(&text) {
            return substitute_exact_jinja_reference(
                node,
                root_node,
                visited_paths,
                circular_references,
                &captures[1],
            );
        } else if jinja_expression_regex().is_match(&text) {
            return substitute_embedded_jinja_references(
                node,
                root_node,
                visited_paths,
                circular_references,
                &text,
            );
        }
    }
    false
}

fn substitute_exact_jinja_reference(
    node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    visited_paths: &mut HashSet<String>,
    circular_references: &mut HashSet<String>,
    reference_path: &str,
) -> bool {
    let reference_path = reference_path.trim();
    if let Some(target_node) =
        find_target_node_using_reference_path(reference_path, Rc::clone(node), Rc::clone(root_node))
    {
        let target_path = target_node.borrow().get_path();

        if !visited_paths.contains(&target_path) {
            resolve_jinja_values_recursively(
                Rc::clone(&target_node),
                Rc::clone(root_node),
                visited_paths,
                circular_references,
            );
            // Only report progress (and trigger a re-resolution pass) when the
            // value actually changed. Adopting a target that is itself an
            // unresolved template copies an identical value, and reporting
            // that as progress would loop forever on mutual references such as
            // `b -> c -> b`.
            return adopt_target_node_structure(node, &target_node);
        }

        // The target is already being resolved further up the call stack, so
        // following it would loop. Break here and record the cycle.
        record_circular_reference(node, reference_path, circular_references);
    }
    false
}

fn record_circular_reference(
    node: &TreeNodeReference,
    reference_path: &str,
    circular_references: &mut HashSet<String>,
) {
    let node_path = node.borrow().get_path();
    let field = if node_path.is_empty() {
        "<root>".to_string()
    } else {
        node_path
    };
    circular_references.insert(format!("'{}' -> '{}'", field, reference_path));
}

/// Copies the target node's value and deep-cloned children onto the node,
/// replacing its previous contents. Returns whether the node's value actually
/// changed, which the caller uses to decide if another resolution pass could
/// make further progress.
fn adopt_target_node_structure(node: &TreeNodeReference, target_node: &TreeNodeReference) -> bool {
    let (target_value, target_children) = {
        let target_borrowed = target_node.borrow();
        (
            target_borrowed.value.clone(),
            target_borrowed.children.clone(),
        )
    };

    let value_changed = node.borrow().value != target_value;

    {
        let mut node_borrowed = node.borrow_mut();
        node_borrowed.value = target_value;
        node_borrowed.children.clear();
    }

    for child in target_children {
        let cloned_child = clone_tree_node_recursively(child, Rc::clone(node));
        node.borrow_mut().children.push(cloned_child);
    }

    value_changed
}

fn substitute_embedded_jinja_references(
    node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    visited_paths: &mut HashSet<String>,
    circular_references: &mut HashSet<String>,
    text: &str,
) -> bool {
    for captures in jinja_expression_regex().captures_iter(text) {
        let reference_path = captures[1].trim();
        if let Some(target_node) = find_target_node_using_reference_path(
            reference_path,
            Rc::clone(node),
            Rc::clone(root_node),
        ) {
            let target_path = target_node.borrow().get_path();
            if !visited_paths.contains(&target_path) {
                resolve_jinja_values_recursively(
                    Rc::clone(&target_node),
                    Rc::clone(root_node),
                    visited_paths,
                    circular_references,
                );
            } else {
                record_circular_reference(node, reference_path, circular_references);
            }
        }
    }

    let resolved_text = resolve_jinja_reference_string(text, Rc::clone(node), Rc::clone(root_node));
    if resolved_text != text {
        node.borrow_mut().value = Some(Value::String(resolved_text));
        return true;
    }
    false
}
