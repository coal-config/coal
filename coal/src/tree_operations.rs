use crate::coal_tree::{CoalTree, Span};
use crate::type_aliases::{TreeNodeReference, new_tree_node_reference};
use crate::yaml_values::{
    convert_marked_yaml_node_to_value, replace_filename_placeholder_in_value,
};
use marked_yaml::Node;
use std::path::Path;
use std::rc::Rc;

/// Expands a marked YAML node into CoalTree children of the given parent.
/// Mapping entries become keyed children; sequence items become children with
/// numeric keys. Scalar nodes have no children and are left untouched.
pub fn build_tree_from_marked_node(
    yaml_node: &Node,
    parent_node: TreeNodeReference,
    origin_path: &str,
) {
    let file_stem = extract_file_stem(origin_path);

    match yaml_node {
        Node::Mapping(mapping) => {
            for (key_node, value_node) in mapping.iter() {
                let (line, column) = node_start_position(key_node.span());
                attach_child_from_marked_node(
                    key_node.as_str().to_string(),
                    value_node,
                    &parent_node,
                    origin_path,
                    &file_stem,
                    line,
                    column,
                );
            }
        }
        Node::Sequence(sequence) => {
            for (index, item_node) in sequence.iter().enumerate() {
                let (line, column) = node_start_position(item_node.span());
                attach_child_from_marked_node(
                    index.to_string(),
                    item_node,
                    &parent_node,
                    origin_path,
                    &file_stem,
                    line,
                    column,
                );
            }
        }
        _ => {}
    }
}

fn extract_file_stem(origin_path: &str) -> String {
    Path::new(origin_path)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

fn node_start_position(span: &marked_yaml::Span) -> (usize, usize) {
    span.start()
        .map(|marker| (marker.line(), marker.column()))
        .unwrap_or((1, 1))
}

fn attach_child_from_marked_node(
    key: String,
    value_node: &Node,
    parent_node: &TreeNodeReference,
    origin_path: &str,
    file_stem: &str,
    line: usize,
    column: usize,
) {
    let mut value = convert_marked_yaml_node_to_value(value_node);
    replace_filename_placeholder_in_value(&mut value, file_stem);

    let child_node = new_tree_node_reference(CoalTree {
        key: Some(key),
        value: Some(value),
        parent: Some(Rc::downgrade(parent_node)),
        origin: Some(origin_path.to_string()),
        span: Some(Span { line, col: column }),
        ..Default::default()
    });

    add_or_replace_child_node_in_parent(Rc::clone(parent_node), Rc::clone(&child_node), false);

    if matches!(value_node, Node::Mapping(_) | Node::Sequence(_)) {
        build_tree_from_marked_node(value_node, child_node, origin_path);
    }
}

/// Inserts a child into the parent, merging or replacing any existing child
/// with the same key. When `preserve_leaves` is true, existing leaf values
/// win over incoming ones (used by $each so templates never clobber data).
pub fn add_or_replace_child_node_in_parent(
    parent_node: TreeNodeReference,
    new_child_node: TreeNodeReference,
    preserve_leaves: bool,
) {
    let child_key = new_child_node.borrow().key.clone();
    let existing_child = find_child_by_key(&parent_node, &child_key);

    if let Some(existing_child) = existing_child {
        if should_merge_nodes(&existing_child, &new_child_node) {
            merge_children_into_existing(new_child_node, &existing_child, preserve_leaves);
        } else if !preserve_leaves {
            replace_child_in_parent(&parent_node, new_child_node, &child_key);
        }
    } else {
        parent_node.borrow_mut().children.push(new_child_node);
    }
}

fn find_child_by_key(
    parent_node: &TreeNodeReference,
    key: &Option<String>,
) -> Option<TreeNodeReference> {
    parent_node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key == *key)
        .cloned()
}

fn should_merge_nodes(existing_node: &TreeNodeReference, new_node: &TreeNodeReference) -> bool {
    let both_are_objects =
        existing_node.borrow().is_object_type() && new_node.borrow().is_object_type();

    let new_node_carries_directives = new_node
        .borrow()
        .children
        .iter()
        .any(|child| child.borrow().key.as_deref().unwrap_or("").starts_with('$'));

    both_are_objects || new_node_carries_directives
}

fn merge_children_into_existing(
    source_node: TreeNodeReference,
    target_node: &TreeNodeReference,
    preserve_leaves: bool,
) {
    let children_to_transfer = source_node.borrow().children.clone();
    for child in children_to_transfer {
        child.borrow_mut().parent = Some(Rc::downgrade(target_node));
        add_or_replace_child_node_in_parent(Rc::clone(target_node), child, preserve_leaves);
    }

    if !preserve_leaves {
        let source_origin = source_node.borrow().origin.clone();
        if source_origin.is_some() {
            target_node.borrow_mut().origin = source_origin;
        }
    }
}

fn replace_child_in_parent(
    parent_node: &TreeNodeReference,
    new_child_node: TreeNodeReference,
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

/// Deep-copies a node and all of its descendants, re-parenting the copy under
/// the given parent. The original tree is left untouched.
pub fn clone_tree_node_recursively(
    source_node: TreeNodeReference,
    parent_node: TreeNodeReference,
) -> TreeNodeReference {
    let source = source_node.borrow();
    let cloned_node = new_tree_node_reference(CoalTree {
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
