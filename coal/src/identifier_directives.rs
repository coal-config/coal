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
/// directive child. The counter name itself may contain Jinja references.
pub fn resolve_identifier_directive_for_node(
    node: TreeNodeReference,
    root_node: TreeNodeReference,
    directive_key: &str,
    counters: &mut IdentifierCounters,
) {
    let counter_name = find_directive_counter_name(&node, directive_key);

    if let Some(raw_counter_name) = counter_name {
        let resolved_counter_name = resolve_jinja_reference_string(
            &raw_counter_name,
            Rc::clone(&node),
            Rc::clone(&root_node),
        );
        let counter = counters.entry(resolved_counter_name).or_insert(1);

        let mut node_borrowed = node.borrow_mut();
        node_borrowed.value = Some(Value::Number((*counter).into()));
        node_borrowed
            .children
            .retain(|child| child.borrow().key.as_deref() != Some(directive_key));

        *counter += 1;
    }
}

fn find_directive_counter_name(node: &TreeNodeReference, directive_key: &str) -> Option<String> {
    let node_borrowed = node.borrow();
    for child in &node_borrowed.children {
        let child_borrowed = child.borrow();
        if child_borrowed.key.as_deref() == Some(directive_key)
            && let Some(Value::String(counter_name)) = &child_borrowed.value
        {
            return Some(counter_name.clone());
        }
    }
    None
}

/// Depth-first pass that resolves every occurrence of the given identifier
/// directive in the subtree. Used for "$id" (just before inheritance targets
/// are cloned) and "$id_after" (once the whole tree has been built).
pub fn resolve_identifier_directives_depth_first(
    node: TreeNodeReference,
    root_node: TreeNodeReference,
    directive_key: &str,
    counters: &mut IdentifierCounters,
) {
    resolve_identifier_directive_for_node(
        Rc::clone(&node),
        Rc::clone(&root_node),
        directive_key,
        counters,
    );

    let children = node.borrow().children.clone();
    for child in children {
        resolve_identifier_directives_depth_first(
            child,
            Rc::clone(&root_node),
            directive_key,
            counters,
        );
    }
}
