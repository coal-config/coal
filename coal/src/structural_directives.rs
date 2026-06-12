use crate::coal_tree::{CoalTree, Span};
use crate::identifier_directives::{
    IdentifierCounters, resolve_identifier_directive_for_node,
    resolve_identifier_directives_depth_first,
};
use crate::reference_resolution::{
    find_target_node_using_reference_path, resolve_jinja_reference_string,
};
use crate::tree_operations::{add_or_replace_child_node_in_parent, clone_tree_node_recursively};
use crate::type_aliases::{TreeNodeReference, new_tree_node_reference};
use crate::yaml_values::extract_string_list_from_value;
use serde_yaml::Value;
use std::error::Error;
use std::rc::Rc;

/// Traverses the tree depth-first and evaluates every structural directive
/// ($inherit, $override, $extend, $append, $select, $each) along with the
/// "$parent" placeholder and "$id" identifier directives.
pub fn process_structural_directives_depth_first(
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
    identifier_counters: &mut IdentifierCounters,
) -> Result<(), Box<dyn Error>> {
    while let Some(directive_node) = find_next_structural_directive(&current_node) {
        execute_structural_directive(
            Rc::clone(&directive_node),
            Rc::clone(&current_node),
            Rc::clone(&root_node),
            identifier_counters,
        )?;

        current_node
            .borrow_mut()
            .children
            .retain(|child| !Rc::ptr_eq(child, &directive_node));
    }

    resolve_parent_placeholder_in_node_value(Rc::clone(&current_node));

    // "$id" resolves during structural processing. Nodes that are inheritance
    // targets may already have been resolved by handle_inherit_directive, in
    // which case this is a no-op.
    resolve_identifier_directive_for_node(
        Rc::clone(&current_node),
        Rc::clone(&root_node),
        "$id",
        identifier_counters,
    );

    let children = current_node.borrow().children.clone();
    for child in children {
        process_structural_directives_depth_first(
            child,
            Rc::clone(&root_node),
            identifier_counters,
        )?;
    }

    Ok(())
}

/// Replaces "$parent" inside the node's string value with the key of the
/// node's immediate parent.
fn resolve_parent_placeholder_in_node_value(node: TreeNodeReference) {
    let mut node_borrowed = node.borrow_mut();

    let value_text = match &node_borrowed.value {
        Some(Value::String(text)) if text.contains("$parent") => text.clone(),
        _ => return,
    };

    let parent_node = node_borrowed
        .parent
        .as_ref()
        .and_then(|weak_parent| weak_parent.upgrade());

    if let Some(parent_node) = parent_node {
        let parent_key = parent_node.borrow().key.clone().unwrap_or_default();
        node_borrowed.value = Some(Value::String(value_text.replace("$parent", &parent_key)));
    }
}

/// Picks the next directive to evaluate among the node's children. Cloning
/// directives ($inherit, $override, $extend, $append, $select) take priority
/// over $each, so $each templates apply to fully expanded siblings.
fn find_next_structural_directive(parent_node: &TreeNodeReference) -> Option<TreeNodeReference> {
    let children = parent_node.borrow().children.clone();
    let mut each_directive = None;

    for child in children {
        let key = child.borrow().key.clone().unwrap_or_default();
        match key.as_str() {
            "$inherit" | "$override" | "$extend" | "$append" | "$select" => return Some(child),
            "$each" if each_directive.is_none() => each_directive = Some(child),
            _ => {}
        }
    }
    each_directive
}

fn execute_structural_directive(
    directive_node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    identifier_counters: &mut IdentifierCounters,
) -> Result<(), Box<dyn Error>> {
    let directive_key = directive_node.borrow().key.clone().unwrap();

    match directive_key.as_str() {
        "$inherit" => handle_inherit_directive(
            directive_node,
            parent_node,
            root_node,
            false,
            identifier_counters,
        ),
        "$override" => handle_inherit_directive(
            directive_node,
            parent_node,
            root_node,
            true,
            identifier_counters,
        ),
        "$extend" => handle_extend_directive(directive_node, parent_node, root_node),
        "$append" => handle_append_directive(directive_node, parent_node),
        "$select" => handle_select_directive(directive_node, parent_node, root_node),
        "$each" => handle_each_directive(directive_node, parent_node),
        _ => Ok(()),
    }
}

// =====================================================================
// $select
// =====================================================================

/// A key requested by a $select directive, paired with the source node it
/// resolved to (if any) and the location it was declared at.
struct SelectedKey {
    key: String,
    span: Option<Span>,
    origin: Option<String>,
    source_node: Option<TreeNodeReference>,
}

/// Transforms a "$keys" list into child objects by looking each key up in
/// the $select target mapping and cloning the matching values. Keys missing
/// from the target produce empty, not-allowed children so the schema
/// validator can flag them.
fn handle_select_directive(
    directive_node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> Result<(), Box<dyn Error>> {
    let raw_target_path = match &directive_node.borrow().value {
        Some(Value::String(path)) => path.clone(),
        _ => return Ok(()),
    };

    let target_path = resolve_jinja_reference_string(
        &raw_target_path,
        Rc::clone(&directive_node),
        Rc::clone(&root_node),
    );
    let target_node = find_target_node_using_reference_path(
        &target_path,
        Rc::clone(&directive_node),
        Rc::clone(&root_node),
    );

    let (requested_keys, keys_directive_node) = extract_requested_keys(&parent_node);
    let selected_keys = resolve_selected_keys(requested_keys, target_node);

    if let Some(keys_directive_node) = keys_directive_node {
        parent_node
            .borrow_mut()
            .children
            .retain(|child| !Rc::ptr_eq(child, &keys_directive_node));
    }

    attach_selected_children(selected_keys, &parent_node);

    Ok(())
}

/// Collects the requested key names from the sibling "$keys" directive,
/// returning them together with the directive node so it can be removed.
fn extract_requested_keys(
    parent_node: &TreeNodeReference,
) -> (Vec<SelectedKey>, Option<TreeNodeReference>) {
    let mut requested_keys = Vec::new();
    let mut keys_directive_node = None;

    let parent_borrowed = parent_node.borrow();
    for child in &parent_borrowed.children {
        let child_borrowed = child.borrow();
        if child_borrowed.key.as_deref() == Some("$keys") {
            keys_directive_node = Some(Rc::clone(child));
            for key_entry in &child_borrowed.children {
                let key_entry_borrowed = key_entry.borrow();
                if let Some(Value::String(key)) = &key_entry_borrowed.value {
                    requested_keys.push(SelectedKey {
                        key: key.clone(),
                        span: key_entry_borrowed.span.clone(),
                        origin: key_entry_borrowed.origin.clone(),
                        source_node: None,
                    });
                }
            }
        }
    }

    (requested_keys, keys_directive_node)
}

fn resolve_selected_keys(
    requested_keys: Vec<SelectedKey>,
    target_node: Option<TreeNodeReference>,
) -> Vec<SelectedKey> {
    let Some(target_node) = target_node else {
        return requested_keys;
    };

    let target_borrowed = target_node.borrow();
    requested_keys
        .into_iter()
        .map(|mut selected_key| {
            selected_key.source_node = target_borrowed
                .children
                .iter()
                .find(|child| child.borrow().key.as_deref() == Some(selected_key.key.as_str()))
                .map(Rc::clone);
            selected_key
        })
        .collect()
}

fn attach_selected_children(selected_keys: Vec<SelectedKey>, parent_node: &TreeNodeReference) {
    for selected_key in selected_keys {
        let new_child = new_tree_node_reference(CoalTree {
            key: Some(selected_key.key),
            parent: Some(Rc::downgrade(parent_node)),
            allowed: true,
            span: selected_key.span,
            origin: selected_key.origin,
            ..Default::default()
        });

        if let Some(source_node) = selected_key.source_node {
            let source_borrowed = source_node.borrow();
            new_child.borrow_mut().value = source_borrowed.value.clone();

            for source_child in &source_borrowed.children {
                let cloned_child =
                    clone_tree_node_recursively(Rc::clone(source_child), Rc::clone(&new_child));
                new_child.borrow_mut().children.push(cloned_child);
            }
        } else {
            let mut new_child_borrowed = new_child.borrow_mut();
            new_child_borrowed.value = Some(Value::Null);
            new_child_borrowed.allowed = false;
        }

        parent_node.borrow_mut().children.push(new_child);
    }
}

// =====================================================================
// $append and $extend
// =====================================================================

/// Clones the directive's own template children onto the end of the parent
/// list, then renumbers the list.
fn handle_append_directive(
    directive_node: TreeNodeReference,
    parent_node: TreeNodeReference,
) -> Result<(), Box<dyn Error>> {
    let template_children = directive_node.borrow().children.clone();
    for child in template_children {
        let cloned_child = clone_tree_node_recursively(child, Rc::clone(&parent_node));
        parent_node.borrow_mut().children.push(cloned_child);
    }
    reindex_list_children(parent_node);
    Ok(())
}

/// Clones the children of each referenced target node onto the end of the
/// parent list, then renumbers the list.
fn handle_extend_directive(
    directive_node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> Result<(), Box<dyn Error>> {
    let raw_value = directive_node.borrow().value.clone();

    if let Some(value) = raw_value {
        for unresolved_path in extract_string_list_from_value(&value) {
            let resolved_path = resolve_jinja_reference_string(
                &unresolved_path,
                Rc::clone(&directive_node),
                Rc::clone(&root_node),
            );

            if let Some(target_node) = find_target_node_using_reference_path(
                &resolved_path,
                Rc::clone(&directive_node),
                Rc::clone(&root_node),
            ) {
                let target_children = target_node.borrow().children.clone();
                for child in target_children {
                    let cloned_child = clone_tree_node_recursively(child, Rc::clone(&parent_node));
                    parent_node.borrow_mut().children.push(cloned_child);
                }
            }
        }
    }
    reindex_list_children(parent_node);
    Ok(())
}

/// Renumbers all non-directive children sequentially so list items keep a
/// gapless numeric order after items have been appended.
fn reindex_list_children(parent_node: TreeNodeReference) {
    let parent_borrowed = parent_node.borrow_mut();
    let mut next_index = 0;

    for child in parent_borrowed.children.iter() {
        let is_directive = child
            .borrow()
            .key
            .as_ref()
            .is_some_and(|key| key.starts_with('$'));
        if !is_directive {
            child.borrow_mut().key = Some(next_index.to_string());
            next_index += 1;
        }
    }
}

// =====================================================================
// $inherit and $override
// =====================================================================

/// Clones the target node's children into the parent. With $inherit, local
/// values win on conflict; with $override, the target's values win.
fn handle_inherit_directive(
    directive_node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    is_override: bool,
    identifier_counters: &mut IdentifierCounters,
) -> Result<(), Box<dyn Error>> {
    // Inline form: the directive holds template children directly instead of
    // a reference path.
    if !directive_node.borrow().children.is_empty() {
        process_structural_directives_depth_first(
            Rc::clone(&directive_node),
            Rc::clone(&root_node),
            identifier_counters,
        )?;
        apply_children_to_parent(directive_node, parent_node, is_override);
        return Ok(());
    }

    let raw_value = directive_node.borrow().value.clone();
    let Some(value) = raw_value else {
        return Ok(());
    };

    for unresolved_path in extract_string_list_from_value(&value) {
        let resolved_path = resolve_jinja_reference_string(
            &unresolved_path,
            Rc::clone(&directive_node),
            Rc::clone(&root_node),
        );

        if let Some(target_node) = find_target_node_using_reference_path(
            &resolved_path,
            Rc::clone(&directive_node),
            Rc::clone(&root_node),
        ) {
            // Resolve every "$id" inside the target itself just before it is
            // inherited. The clone below then carries the actual ID values
            // instead of the unresolved tags, so all inheritors (and the
            // target) share the same numbers. "$id_after" tags are
            // deliberately left untouched and cloned as-is.
            resolve_identifier_directives_depth_first(
                Rc::clone(&target_node),
                Rc::clone(&root_node),
                "$id",
                identifier_counters,
            );

            let detached_clone = clone_target_for_processing(target_node, &root_node);
            process_structural_directives_depth_first(
                Rc::clone(&detached_clone),
                Rc::clone(&root_node),
                identifier_counters,
            )?;
            apply_children_to_parent(detached_clone, Rc::clone(&parent_node), is_override);
        }
    }
    Ok(())
}

/// Deep-copies the inheritance target so its own directives can be expanded
/// without mutating the original node.
fn clone_target_for_processing(
    target_node: TreeNodeReference,
    root_node: &TreeNodeReference,
) -> TreeNodeReference {
    let target_parent = target_node
        .borrow()
        .parent
        .as_ref()
        .and_then(|weak_parent| weak_parent.upgrade())
        .unwrap_or_else(|| Rc::clone(root_node));
    clone_tree_node_recursively(target_node, target_parent)
}

fn apply_children_to_parent(
    source_node: TreeNodeReference,
    parent_node: TreeNodeReference,
    is_override: bool,
) {
    let children = source_node.borrow().children.clone();
    for child in children {
        let cloned_child = clone_tree_node_recursively(child, Rc::clone(&parent_node));
        add_or_replace_child_node_in_parent(Rc::clone(&parent_node), cloned_child, !is_override);
    }
}

// =====================================================================
// $each
// =====================================================================

/// Clones the $each directive's template children into every sibling of the
/// directive. Existing sibling values always win over the template.
fn handle_each_directive(
    directive_node: TreeNodeReference,
    parent_node: TreeNodeReference,
) -> Result<(), Box<dyn Error>> {
    let template_children = directive_node.borrow().children.clone();
    let sibling_nodes = parent_node.borrow().children.clone();

    for sibling in sibling_nodes {
        if Rc::ptr_eq(&sibling, &directive_node) {
            continue;
        }
        for template_child in &template_children {
            let cloned_child =
                clone_tree_node_recursively(Rc::clone(template_child), Rc::clone(&sibling));
            add_or_replace_child_node_in_parent(Rc::clone(&sibling), cloned_child, true);
        }
    }
    Ok(())
}
