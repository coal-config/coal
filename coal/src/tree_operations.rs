use crate::coal_tree::{CoalTree, Span};
use crate::type_aliases::{TreeNodeReference, new_tree_node_reference};
use crate::yaml_values::convert_marked_yaml_node_to_value;
use marked_yaml::Node;
use serde_yaml::Value;
use std::rc::Rc;

/// Expands a marked YAML node into CoalTree children of the given parent.
/// Mapping entries become keyed children; sequence items become children with
/// numeric keys. Scalar nodes have no children and are left untouched.
///
/// `source` is the text the node was parsed from. It is only read to place
/// list items the parser cannot place itself (see `sequence_item_position`).
pub fn build_tree_from_marked_node(
    yaml_node: &Node,
    parent_node: TreeNodeReference,
    origin_path: &str,
    source: &str,
) {
    match yaml_node {
        Node::Mapping(mapping) => {
            for (key_node, value_node) in mapping.iter() {
                let (line, column) = node_start_position(key_node.span());
                attach_child_from_marked_node(
                    key_node.as_str().to_string(),
                    value_node,
                    &parent_node,
                    origin_path,
                    source,
                    line,
                    column,
                );
            }
        }
        Node::Sequence(sequence) => {
            let items: Vec<&Node> = sequence.iter().collect();
            for (index, item_node) in items.iter().copied().enumerate() {
                let next_item = items.get(index + 1).copied();
                let (line, column) = sequence_item_position(item_node, next_item, source);
                attach_child_from_marked_node(
                    index.to_string(),
                    item_node,
                    &parent_node,
                    origin_path,
                    source,
                    line,
                    column,
                );
            }
        }
        _ => {}
    }
}

fn node_start_position(span: &marked_yaml::Span) -> (usize, usize) {
    span.start()
        .map(|marker| (marker.line(), marker.column()))
        .unwrap_or((1, 1))
}

/// Where a list item is written.
///
/// An item left empty (`- ` with nothing after it, which is exactly what a
/// list looks like while it is being typed) is a scalar with no text, and the
/// parser places it where the *next* token starts: on the line below, or on
/// the next item. Reported there, a finding about the empty item lands on the
/// wrong line and an editor asking "what is on this line?" finds nothing. The
/// item belongs to the `-` that introduced it, so an empty item is moved back
/// to the nearest dash above the parser's position.
fn sequence_item_position(item_node: &Node, next_item: Option<&Node>, source: &str) -> (usize, usize) {
    let parsed_position = node_start_position(item_node.span());

    let is_empty_scalar = matches!(item_node, Node::Scalar(scalar) if scalar.as_str().is_empty());
    if !is_empty_scalar {
        return parsed_position;
    }

    let (parsed_line, parsed_column) = parsed_position;
    let lines: Vec<&str> = source.lines().collect();
    if parsed_line == 0 || parsed_line > lines.len() + 1 {
        return parsed_position;
    }

    // A quoted empty string (`- ""`) has text of its own, and the parser puts
    // it where it is written. An unquoted one borrows the position of what
    // follows it: the next item when there is one, and otherwise whatever
    // comes after the list — which is never a quote behind a dash.
    let borrowed_from_next_item =
        next_item.is_some_and(|next| node_start_position(next.span()) == parsed_position);
    if !borrowed_from_next_item {
        let written_line = lines.get(parsed_line - 1).copied().unwrap_or("");
        let before: String = written_line.chars().take(parsed_column.saturating_sub(1)).collect();
        let at = written_line.chars().nth(parsed_column.saturating_sub(1));
        let quoted_item_here = matches!(at, Some('"') | Some('\''))
            && before.trim_end().ends_with('-');
        if quoted_item_here {
            return parsed_position;
        }
    }

    // Nothing follows an empty item's dash on its own line but a comment, so
    // the parser's line is never the item's: a dash there introduces the
    // *next* item. The nearest line above that is not blank or a comment is
    // the item's own, provided it ends in a dash.
    for line_number in (1..parsed_line).rev() {
        let content = strip_yaml_comment(lines[line_number - 1]).trim_end();
        if content.is_empty() {
            continue;
        }

        let Some(before_dash) = content.strip_suffix('-') else {
            return parsed_position;
        };
        let dash_introduces_item = before_dash
            .chars()
            .last()
            .is_none_or(|previous| previous.is_whitespace() || previous == '-');
        if !dash_introduces_item {
            return parsed_position;
        }
        return (line_number, before_dash.chars().count() + 1);
    }

    parsed_position
}

/// A line with its trailing `# comment` removed. A `#` only starts a comment
/// at the start of the line or after whitespace; the check is deliberately
/// simple, since it is only applied to lines that end an empty list item.
fn strip_yaml_comment(line: &str) -> &str {
    let mut previous_is_space = true;
    for (byte_index, character) in line.char_indices() {
        if character == '#' && previous_is_space {
            return &line[..byte_index];
        }
        previous_is_space = character.is_whitespace();
    }
    line
}

fn attach_child_from_marked_node(
    key: String,
    value_node: &Node,
    parent_node: &TreeNodeReference,
    origin_path: &str,
    source: &str,
    line: usize,
    column: usize,
) {
    let value = convert_marked_yaml_node_to_value(value_node);

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
        build_tree_from_marked_node(value_node, child_node, origin_path, source);
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
        object_fields_checked: source.object_fields_checked,
        template_resolved: source.template_resolved,
        schema_error: source.schema_error.clone(),
        file_root: source.file_root,
        // Deliberately not copied: a copy has landed somewhere new, and the
        // `_each` template of wherever that is has not had its say yet.
        each_directives_copied: false,
        include_scope: source.include_scope.clone(),
        children: Vec::new(),
        span: source.span.clone(),
    });

    for child in &source.children {
        let cloned_child = clone_tree_node_recursively(Rc::clone(child), Rc::clone(&cloned_node));
        cloned_node.borrow_mut().children.push(cloned_child);
    }
    cloned_node
}

/// Deep-copies a whole tree, detached from wherever the original sat.
///
/// A build works on a copy of the directory as it was read, so the same
/// reading can be built again for a different selection without the first
/// build's expansions being visible to the second.
pub fn clone_tree_root(root_node: TreeNodeReference) -> TreeNodeReference {
    let cloned_root = clone_tree_node_recursively(Rc::clone(&root_node), Rc::clone(&root_node));
    cloned_root.borrow_mut().parent = None;
    cloned_root
}

/// Replaces a node's children with the structure held in `value`, turning a
/// list or mapping that a Jinja template produced into real tree nodes. The
/// new nodes inherit the origin and location of the field the template was
/// written on, so diagnostics point at the template rather than at nothing.
///
/// They are also marked as resolved. Everything in a template's result came
/// either from values that were already resolved or from the template's own
/// literal text, so a piece of it that happens to look like a template is
/// literal text and must not be rendered again.
pub fn rebuild_node_children_from_value(node: &TreeNodeReference, value: &Value) {
    let (origin, span) = {
        let node_borrowed = node.borrow();
        (node_borrowed.origin.clone(), node_borrowed.span.clone())
    };

    node.borrow_mut().children.clear();

    match value {
        Value::Mapping(mapping) => {
            for (key, entry_value) in mapping {
                let key_text = yaml_key_as_child_key(key);
                attach_generated_child(node, key_text, entry_value, &origin, &span);
            }
        }
        Value::Sequence(sequence) => {
            for (index, item_value) in sequence.iter().enumerate() {
                attach_generated_child(node, index.to_string(), item_value, &origin, &span);
            }
        }
        _ => {}
    }
}

fn attach_generated_child(
    parent_node: &TreeNodeReference,
    key: String,
    value: &Value,
    origin: &Option<String>,
    span: &Option<Span>,
) {
    let child_node = new_tree_node_reference(CoalTree {
        key: Some(key),
        value: Some(value.clone()),
        parent: Some(Rc::downgrade(parent_node)),
        origin: origin.clone(),
        span: span.clone(),
        template_resolved: true,
        ..Default::default()
    });

    rebuild_node_children_from_value(&child_node, value);
    parent_node.borrow_mut().children.push(child_node);
}

fn yaml_key_as_child_key(key: &Value) -> String {
    match key {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        _ => String::new(),
    }
}
