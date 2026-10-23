use crate::reference_resolution::resolve_jinja_reference_string;
use crate::type_aliases::TreeNodeReference;
use serde_yaml::Value;
use std::collections::HashMap;
use std::rc::Rc;

/// Sequential counters keyed by counter name. A single map is shared between
/// "$id" and "$id_after" so a counter keeps incrementing regardless of which
/// directive variant references it.
pub type IdentifierCounters = HashMap<String, i64>;

/// Checks whether the node carries a child with the given identifier
/// directive key ("$id" or "$id_after"). If so, replaces the node's value
/// with the next sequential integer for that counter name and removes the
/// directive child.
///
/// The counter name is a template like any other value, so a counter may be
/// named after the node that holds it: `$id: "{{ parent(2).key() }}"`.
pub fn resolve_identifier_directive_for_node(
    node: TreeNodeReference,
    root_node: TreeNodeReference,
    directive_key: &str,
    counters: &mut IdentifierCounters,
) -> bool {
    let directive_field = find_directive_field(&node, directive_key);

    if let Some((directive_field, raw_counter_name)) = directive_field {
        let resolved_counter_name = resolve_jinja_reference_string(
            &raw_counter_name,
            Rc::clone(&directive_field),
            Rc::clone(&root_node),
        );
        let counter = counters.entry(resolved_counter_name).or_insert(1);

        let mut node_borrowed = node.borrow_mut();
        node_borrowed.value = Some(Value::Number((*counter).into()));
        node_borrowed
            .children
            .retain(|child| child.borrow().key.as_deref() != Some(directive_key));

        *counter += 1;
        return true;
    }
    false
}

/// Finds the identifier directive child and its raw counter-name string,
/// returning both the field node (so placeholders can be resolved relative to
/// it) and the name.
fn find_directive_field(
    node: &TreeNodeReference,
    directive_key: &str,
) -> Option<(TreeNodeReference, String)> {
    let node_borrowed = node.borrow();
    for child in &node_borrowed.children {
        let child_borrowed = child.borrow();
        if child_borrowed.key.as_deref() == Some(directive_key)
            && let Some(Value::String(counter_name)) = &child_borrowed.value
        {
            return Some((Rc::clone(child), counter_name.clone()));
        }
    }
    None
}

/// Depth-first pass that resolves every occurrence of the given identifier
/// directive in the subtree, reporting whether it found any. Used for "$id"
/// (just before inheritance targets are cloned) and "$id_after" (once the
/// whole tree has been built).
pub fn resolve_identifier_directives_depth_first(
    node: TreeNodeReference,
    root_node: TreeNodeReference,
    directive_key: &str,
    counters: &mut IdentifierCounters,
) -> bool {
    let mut resolved_any = resolve_identifier_directive_for_node(
        Rc::clone(&node),
        Rc::clone(&root_node),
        directive_key,
        counters,
    );

    let children = node.borrow().children.clone();
    for child in children {
        resolved_any |= resolve_identifier_directives_depth_first(
            child,
            Rc::clone(&root_node),
            directive_key,
            counters,
        );
    }
    resolved_any
}
