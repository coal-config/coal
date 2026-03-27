use crate::coal_tree::CoalTree;
use crate::shared_logic::{
    add_or_replace_child_node_in_parent, clone_tree_node_recursively,
    extract_string_array_from_yaml_value, find_target_node_using_reference_path,
    resolve_jinja_reference_string,
};
use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;

pub fn process_abstract_syntax_tree_functions_depth_first(
    current_tree_node: Rc<RefCell<CoalTree>>,
    root_tree_node: Rc<RefCell<CoalTree>>,
    id_counters: &mut std::collections::HashMap<String, i64>,
) -> Result<(), Box<dyn Error>> {

    while let Some(directive_node) = identify_next_directive(&current_tree_node) {
        execute_directive(
            Rc::clone(&directive_node),
            Rc::clone(&current_tree_node),
            Rc::clone(&root_tree_node),
            id_counters
        )?;

        current_tree_node.borrow_mut().children.retain(|child| !Rc::ptr_eq(child, &directive_node));
    }

    resolve_parent_for_node(Rc::clone(&current_tree_node));
    resolve_id_for_node(Rc::clone(&current_tree_node), Rc::clone(&root_tree_node), id_counters);

    let children = current_tree_node.borrow().children.clone();
    for child in children {
        process_abstract_syntax_tree_functions_depth_first(child, Rc::clone(&root_tree_node), id_counters)?;
    }

    Ok(())
}

fn resolve_parent_for_node(node: Rc<RefCell<CoalTree>>) {
    let mut borrowed = node.borrow_mut();
    if let Some(serde_yaml::Value::String(s)) = &borrowed.value {
        if s.contains("$parent") {
            if let Some(parent_rc) = borrowed.parent.as_ref().and_then(|w| w.upgrade()) {
                let parent_key = parent_rc.borrow().key.clone().unwrap_or_default();
                let new_s = s.replace("$parent", &parent_key);
                borrowed.value = Some(serde_yaml::Value::String(new_s));
            }
        }
    }
}

fn resolve_id_for_node(
    node: Rc<RefCell<CoalTree>>,
    root_node: Rc<RefCell<CoalTree>>,
    counters: &mut std::collections::HashMap<String, i64>
) {
    let mut id_key_to_process = None;

    {
        let borrowed = node.borrow();
        for child in &borrowed.children {
            let child_borrowed = child.borrow();
            if child_borrowed.key.as_deref() == Some("$id") {
                if let Some(serde_yaml::Value::String(ref s)) = child_borrowed.value {
                    id_key_to_process = Some(s.clone());
                    break;
                }
            }
        }
    }

    if let Some(raw_id_key) = id_key_to_process {
        let resolved_id_key = resolve_jinja_reference_string(&raw_id_key, Rc::clone(&node), Rc::clone(&root_node));
        let count = counters.entry(resolved_id_key).or_insert(1);

        let mut borrowed_mut = node.borrow_mut();
        borrowed_mut.value = Some(serde_yaml::Value::Number((*count).into()));
        borrowed_mut.children.retain(|c| c.borrow().key.as_deref() != Some("$id"));

        *count += 1;
    }
}

fn identify_next_directive(parent: &Rc<RefCell<CoalTree>>) -> Option<Rc<RefCell<CoalTree>>> {
    let children = parent.borrow().children.clone();
    let mut each_node = None;

    for child in children {
        let key = child.borrow().key.clone().unwrap_or_default();
        if key == "$inherit" || key == "$override" || key == "$extend" || key == "$append" || key == "$select" {
            return Some(child);
        }
        if key == "$each" && each_node.is_none() {
            each_node = Some(child);
        }
    }
    each_node
}

fn execute_directive(
    directive: Rc<RefCell<CoalTree>>,
    parent: Rc<RefCell<CoalTree>>,
    root: Rc<RefCell<CoalTree>>,
    id_counters: &mut std::collections::HashMap<String, i64>,
) -> Result<(), Box<dyn Error>> {
    let directive_key = directive.borrow().key.clone().unwrap();

    match directive_key.as_str() {
        "$inherit" => handle_inherit_node(directive, parent, root, false, id_counters),
        "$override" => handle_inherit_node(directive, parent, root, true, id_counters),
        "$extend" => handle_extend_node(directive, parent, root),
        "$append" => handle_append_node(directive, parent),
        "$select" => handle_select_node(directive, parent, root),
        "$each" => handle_each_node(directive, parent),
        _ => Ok(()),
    }
}

fn handle_select_node(
    directive_node: Rc<RefCell<CoalTree>>,
    parent_node: Rc<RefCell<CoalTree>>,
    root_node: Rc<RefCell<CoalTree>>,
) -> Result<(), Box<dyn Error>> {
    let target_path_raw = match &directive_node.borrow().value {
        Some(serde_yaml::Value::String(s)) => s.clone(),
        _ => return Ok(()),
    };

    let target_path = resolve_jinja_reference_string(&target_path_raw, Rc::clone(&directive_node), Rc::clone(&root_node));
    let target_node_opt = find_target_node_using_reference_path(&target_path, Rc::clone(&directive_node), Rc::clone(&root_node));

    let mut keys_to_select = Vec::new();
    let mut keys_node_to_remove = None;

    // 1. Extract the requested keys AND their exact text spans/origins
    {
        let parent_borrowed = parent_node.borrow();
        for child in &parent_borrowed.children {
            let child_borrowed = child.borrow();
            if child_borrowed.key.as_deref() == Some("$keys") {
                keys_node_to_remove = Some(Rc::clone(child));
                for k_child in &child_borrowed.children {
                    let k_borrowed = k_child.borrow();
                    if let Some(serde_yaml::Value::String(k)) = &k_borrowed.value {
                        // CRITICAL: Capture the span and origin here
                        keys_to_select.push((k.clone(), k_borrowed.span.clone(), k_borrowed.origin.clone()));
                    }
                }
            }
        }
    }

    let mut transformations = Vec::new();

    // 2. Map requested keys to available targets
    if let Some(target_node) = target_node_opt {
        let target_borrowed = target_node.borrow();
        for (requested_key, span, origin) in keys_to_select {
            if let Some(source_child) = target_borrowed.children.iter().find(|c| c.borrow().key.as_deref() == Some(requested_key.as_str())) {
                transformations.push((requested_key, span, origin, Some(Rc::clone(source_child))));
            } else {
                transformations.push((requested_key, span, origin, None));
            }
        }
    } else {
        for (requested_key, span, origin) in keys_to_select {
            transformations.push((requested_key, span, origin, None));
        }
    }

    // 3. Remove the $keys node so it doesn't linger in the tree
    if let Some(kn) = keys_node_to_remove {
        parent_node.borrow_mut().children.retain(|c| !Rc::ptr_eq(c, &kn));
    }

    // 4. Generate the new children on the parent node
    for (new_key, span, origin, source_child_opt) in transformations {

        // CRITICAL: Pass the captured span and origin into the new morphed node
        let new_child = Rc::new(RefCell::new(CoalTree {
            key: Some(new_key.clone()),
            parent: Some(Rc::downgrade(&parent_node)),
            allowed: true,
            span,
            origin,
            ..Default::default()
        }));

        if let Some(source_child) = source_child_opt {
            let source_borrowed = source_child.borrow();
            new_child.borrow_mut().value = source_borrowed.value.clone();

            for sc in &source_borrowed.children {
                let cloned = clone_tree_node_recursively(Rc::clone(sc), Rc::clone(&new_child));
                new_child.borrow_mut().children.push(cloned);
            }
        } else {
            // Missing key constraint: Make it empty, but trigger a targeted diagnostics error
            // exactly at the line where the user typed the invalid array item.
            let mut borrowed_mut = new_child.borrow_mut();
            borrowed_mut.value = Some(serde_yaml::Value::Null);
            borrowed_mut.allowed = false;
        }

        parent_node.borrow_mut().children.push(new_child);
    }

    Ok(())
}

fn handle_append_node(
    directive_node: Rc<RefCell<CoalTree>>,
    parent_node: Rc<RefCell<CoalTree>>,
) -> Result<(), Box<dyn Error>> {
    let target_children = directive_node.borrow().children.clone();
    for child in target_children {
        let cloned = clone_tree_node_recursively(child, Rc::clone(&parent_node));
        parent_node.borrow_mut().children.push(cloned);
    }
    reindex_parent_list(parent_node);
    Ok(())
}

fn handle_extend_node(
    directive_node: Rc<RefCell<CoalTree>>,
    parent_node: Rc<RefCell<CoalTree>>,
    root_node: Rc<RefCell<CoalTree>>,
) -> Result<(), Box<dyn Error>> {
    let raw_value = directive_node.borrow().value.clone();

    if let Some(value) = raw_value {
        for unresolved_path in extract_string_array_from_yaml_value(&value) {
            let resolved_path = resolve_jinja_reference_string(&unresolved_path, Rc::clone(&directive_node), Rc::clone(&root_node));

            if let Some(target) = find_target_node_using_reference_path(&resolved_path, Rc::clone(&directive_node), Rc::clone(&root_node)) {
                let target_children = target.borrow().children.clone();
                for child in target_children {
                    let cloned = clone_tree_node_recursively(child, Rc::clone(&parent_node));
                    parent_node.borrow_mut().children.push(cloned);
                }
            }
        }
    }
    reindex_parent_list(parent_node);
    Ok(())
}

fn reindex_parent_list(parent_node: Rc<RefCell<CoalTree>>) {
    let mut parent_borrowed = parent_node.borrow_mut();
    let mut current_index = 0;

    for child in parent_borrowed.children.iter() {
        let is_directive = child.borrow().key.as_ref().map(|k| k.starts_with('$')).unwrap_or(false);
        if !is_directive {
            child.borrow_mut().key = Some(current_index.to_string());
            current_index += 1;
        }
    }
}

fn handle_inherit_node(
    directive_node: Rc<RefCell<CoalTree>>,
    parent_node: Rc<RefCell<CoalTree>>,
    root_node: Rc<RefCell<CoalTree>>,
    is_override: bool,
    id_counters: &mut std::collections::HashMap<String, i64>,
) -> Result<(), Box<dyn Error>> {
    if !directive_node.borrow().children.is_empty() {
        process_abstract_syntax_tree_functions_depth_first(Rc::clone(&directive_node), Rc::clone(&root_node), id_counters)?;
        apply_children_to_parent(directive_node, parent_node, is_override);
        return Ok(());
    }

    let raw_value = directive_node.borrow().value.clone();
    if let Some(value) = raw_value {
        for unresolved_path in extract_string_array_from_yaml_value(&value) {
            let resolved_path = resolve_jinja_reference_string(&unresolved_path, Rc::clone(&directive_node), Rc::clone(&root_node));

            if let Some(target) = find_target_node_using_reference_path(&resolved_path, Rc::clone(&directive_node), Rc::clone(&root_node)) {
                let ghost = create_processing_ghost(target, &root_node);
                process_abstract_syntax_tree_functions_depth_first(Rc::clone(&ghost), Rc::clone(&root_node), id_counters)?;
                apply_children_to_parent(ghost, Rc::clone(&parent_node), is_override);
            }
        }
    }
    Ok(())
}

fn handle_each_node(
    directive: Rc<RefCell<CoalTree>>,
    parent: Rc<RefCell<CoalTree>>,
) -> Result<(), Box<dyn Error>> {
    let templates = directive.borrow().children.clone();
    let siblings = parent.borrow().children.clone();

    for sibling in siblings {
        if Rc::ptr_eq(&sibling, &directive) { continue; }
        for template in &templates {
            let cloned = clone_tree_node_recursively(Rc::clone(template), Rc::clone(&sibling));
            add_or_replace_child_node_in_parent(Rc::clone(&sibling), cloned, true);
        }
    }
    Ok(())
}

fn apply_children_to_parent(source: Rc<RefCell<CoalTree>>, parent: Rc<RefCell<CoalTree>>, is_override: bool) {
    let children = source.borrow().children.clone();
    for child in children {
        let cloned = clone_tree_node_recursively(child, Rc::clone(&parent));
        add_or_replace_child_node_in_parent(Rc::clone(&parent), cloned, !is_override);
    }
}

fn create_processing_ghost(target: Rc<RefCell<CoalTree>>, root: &Rc<RefCell<CoalTree>>) -> Rc<RefCell<CoalTree>> {
    let parent = target.borrow().parent.as_ref().and_then(|w| w.upgrade()).unwrap_or_else(|| Rc::clone(root));
    clone_tree_node_recursively(target, parent)
}