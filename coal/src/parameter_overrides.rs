use crate::coal_tree::CoalTree;
use crate::type_aliases::{TreeNodeReference, new_tree_node_reference};
use crate::yaml_values::parse_scalar_value;
use serde_yaml::Value;
use std::error::Error;
use std::rc::Rc;

/// Origin label attached to nodes that were set from the command line.
pub const COMMAND_LINE_ORIGIN: &str = "<command-line>";

/// A value supplied on the command line via `-p path=value`. It is seeded
/// into the tree before includes and structural directives run, so it can
/// drive the rest of the build; from then on it behaves like any value
/// declared in the configuration files.
#[derive(Debug, Clone, PartialEq)]
pub struct ParameterOverride {
    /// The target location as a list of keys, e.g. `["environment", "paths", "config"]`.
    pub path_segments: Vec<String>,
    /// The value to write. Parsed like any COAL scalar, so a Jinja expression
    /// remains a string and is resolved during the normal Jinja phase.
    pub value: Value,
}

/// Parses one `-p` argument of the form `dotted.path=value`.
///
/// The path is split on dots; an optional leading `ROOT.` is accepted for
/// symmetry with reference paths. The value is everything after the first
/// `=`, so values may themselves contain `=`. The value is parsed with the
/// same scalar rules as YAML leaves, which deliberately keeps a Jinja
/// expression (`{{ ... }}`) as a string so it resolves later in the pipeline.
pub fn parse_parameter_override(input: &str) -> Result<ParameterOverride, Box<dyn Error>> {
    let (path_text, value_text) = input.split_once('=').ok_or_else(|| {
        Box::<dyn Error>::from(format!(
            "Invalid -p argument '{}': expected the form path=value \
             (e.g. environment.paths.config=/home/config)",
            input
        ))
    })?;

    let path_segments = parse_path_segments(path_text)?;
    let value = parse_scalar_value(value_text);

    Ok(ParameterOverride {
        path_segments,
        value,
    })
}

fn parse_path_segments(path_text: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let trimmed_path = path_text.trim();
    let trimmed_path = trimmed_path.strip_prefix("ROOT.").unwrap_or(trimmed_path);

    let path_segments: Vec<String> = trimmed_path
        .split('.')
        .map(|segment| segment.trim().to_string())
        .collect();

    if path_segments.iter().any(|segment| segment.is_empty()) {
        return Err(Box::<dyn Error>::from(format!(
            "Invalid -p path '{}': path segments must not be empty",
            path_text
        )));
    }

    Ok(path_segments)
}

/// Writes every parameter value into the tree, creating intermediate object
/// nodes along the path where they do not yet exist.
pub fn apply_parameter_overrides(root_node: TreeNodeReference, overrides: &[ParameterOverride]) {
    for parameter_override in overrides {
        apply_single_override(Rc::clone(&root_node), parameter_override);
    }
}

fn apply_single_override(root_node: TreeNodeReference, parameter_override: &ParameterOverride) {
    let mut current_node = root_node;
    let last_index = parameter_override.path_segments.len() - 1;

    for (index, segment) in parameter_override.path_segments.iter().enumerate() {
        let child_node = find_or_create_child(&current_node, segment);

        if index == last_index {
            write_pinned_value(&child_node, parameter_override.value.clone());
        }

        current_node = child_node;
    }
}

/// Sets the leaf's value and clears its children. Clearing children matters
/// because value synchronization rebuilds a node's value from its children,
/// so a leaf that kept children would lose the seeded scalar.
fn write_pinned_value(node: &TreeNodeReference, value: Value) {
    let mut node_borrowed = node.borrow_mut();
    node_borrowed.value = Some(value);
    node_borrowed.children.clear();
    node_borrowed.origin = Some(COMMAND_LINE_ORIGIN.to_string());
}

fn find_or_create_child(parent_node: &TreeNodeReference, key: &str) -> TreeNodeReference {
    let existing_child = parent_node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key.as_deref() == Some(key))
        .cloned();

    if let Some(existing_child) = existing_child {
        return existing_child;
    }

    let new_child = new_tree_node_reference(CoalTree {
        key: Some(key.to_string()),
        parent: Some(Rc::downgrade(parent_node)),
        origin: Some(COMMAND_LINE_ORIGIN.to_string()),
        ..Default::default()
    });
    parent_node
        .borrow_mut()
        .children
        .push(Rc::clone(&new_child));
    new_child
}
