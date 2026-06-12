use crate::type_aliases::TreeNodeReference;
use crate::yaml_values::{
    sort_value_mapping_keys_recursively, strip_system_directive_keys_from_value,
};
use serde_yaml::Value;

/// Rebuilds every node's raw YAML value from its children, bottom-up. Nodes
/// whose children all have numeric keys become sequences; the rest become
/// mappings. System directive children ('$'-prefixed) are excluded.
pub fn synchronize_node_values_depth_first(node: TreeNodeReference) {
    let children = node.borrow().children.clone();
    for child in children {
        synchronize_node_values_depth_first(child);
    }

    let mut node_borrowed = node.borrow_mut();
    if node_borrowed.children.is_empty() {
        return;
    }

    node_borrowed.value = Some(if node_borrowed.is_list_type() {
        build_sequence_value_from_children(&node_borrowed.children)
    } else {
        build_mapping_value_from_children(&node_borrowed.children)
    });
}

fn build_sequence_value_from_children(children: &[TreeNodeReference]) -> Value {
    let sequence = children
        .iter()
        .map(|child| child.borrow().value.clone().unwrap_or(Value::Null))
        .collect();
    Value::Sequence(sequence)
}

fn build_mapping_value_from_children(children: &[TreeNodeReference]) -> Value {
    let mut mapping = serde_yaml::Mapping::new();
    for child in children {
        let key = child.borrow().key.clone().unwrap_or_default();
        if !key.starts_with('$') {
            mapping.insert(
                Value::String(key),
                child.borrow().value.clone().unwrap_or(Value::Null),
            );
        }
    }

    if mapping.is_empty() {
        Value::Null
    } else {
        Value::Mapping(mapping)
    }
}

/// Removes every unprocessed system directive ('$'-prefixed key) from the
/// tree structure and from the raw YAML values, so internal directives never
/// leak into the compiled output.
pub fn remove_system_directives_from_tree(node: TreeNodeReference) {
    {
        let mut node_borrowed = node.borrow_mut();

        node_borrowed.children.retain(|child| {
            !child
                .borrow()
                .key
                .as_ref()
                .is_some_and(|key| key.starts_with('$'))
        });

        if let Some(ref mut value) = node_borrowed.value {
            strip_system_directive_keys_from_value(value);
        }
    }

    let remaining_children = node.borrow().children.clone();
    for child in remaining_children {
        remove_system_directives_from_tree(child);
    }
}

/// Removes children that are empty mappings: no tree children, no raw data,
/// and typed or shaped as an object. Runs bottom-up so a chain of empty
/// parents collapses entirely.
pub fn prune_empty_mapping_nodes(node: TreeNodeReference) {
    let children = node.borrow().children.clone();
    for child in children {
        prune_empty_mapping_nodes(child);
    }

    node.borrow_mut()
        .children
        .retain(|child| !is_empty_mapping_node(child));
}

fn is_empty_mapping_node(node: &TreeNodeReference) -> bool {
    let node_borrowed = node.borrow();

    let has_no_children = node_borrowed.children.is_empty();

    let has_no_raw_data = match &node_borrowed.value {
        Some(Value::Mapping(mapping)) => mapping.is_empty(),
        None => true,
        _ => false,
    };

    let is_object =
        node_borrowed.is_object_type() || node_borrowed.field_type.as_deref() == Some("object");

    has_no_children && has_no_raw_data && is_object
}

/// Recursively sorts the tree so that sibling keys appear in alphabetical
/// order. List nodes (numeric keys) are sorted numerically instead, so that
/// item 10 does not jump in front of item 2. Raw YAML mapping values held on
/// leaf nodes are sorted as well so printed output is always alphabetical.
pub fn sort_tree_alphabetically(node: TreeNodeReference) {
    {
        let mut node_borrowed = node.borrow_mut();

        if node_borrowed.is_list_type() {
            node_borrowed.children.sort_by_key(|child| {
                child
                    .borrow()
                    .key
                    .as_deref()
                    .and_then(|key| key.parse::<usize>().ok())
                    .unwrap_or(usize::MAX)
            });
        } else {
            node_borrowed.children.sort_by(|left, right| {
                let left_key = left.borrow().key.clone().unwrap_or_default();
                let right_key = right.borrow().key.clone().unwrap_or_default();
                left_key.cmp(&right_key)
            });
        }

        if let Some(ref mut value) = node_borrowed.value {
            sort_value_mapping_keys_recursively(value);
        }
    }

    let children = node.borrow().children.clone();
    for child in children {
        sort_tree_alphabetically(child);
    }
}
