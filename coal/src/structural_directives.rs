//! The directives that reshape the tree while it is being built.
//!
//! Three are written in the configuration — `$extend` and `$append`, which
//! both add to a list, and `$inherit`, which derives one object from another —
//! and one comes from the directory layout: the template an `_each` folder
//! contributes to every sibling field.
//!
//! `$inherit` and `_structure.yaml` both layer values, and they answer
//! different questions. The structure says how the configuration as a whole is
//! assembled for the one thing being built. `$inherit` says how a *single*
//! object derives from another, chosen by a value that object carries — and
//! written once inside an `_each` folder, it says that for a whole collection
//! at once.

use crate::coal_tree::CoalTree;
use crate::directory_loader::{EACH_TEMPLATE_KEY, IGNORE_KEY, is_each_template};
use crate::identifier_directives::{
    IdentifierCounters, resolve_identifier_directive_for_node,
    resolve_identifier_directives_depth_first,
};
use crate::jinja_engine::ResolutionStage;
use crate::reference_resolution::{
    check_reference_path_is_absolute, find_target_node_using_reference_path,
    try_resolve_jinja_reference_string_at,
};
use crate::tree_operations::{add_or_replace_child_node_in_parent, clone_tree_node_recursively};
use crate::type_aliases::{TreeNodeReference, new_tree_node_reference};
use crate::yaml_values::extract_string_list_from_value;
use std::error::Error;
use std::rc::Rc;

/// Maximum number of structural directives expanded on a single node. A
/// runaway count indicates a directive that keeps re-creating itself, such as
/// an `$extend`/`$append` cycle, which would otherwise loop forever.
const MAX_DIRECTIVES_PER_NODE: usize = 1024;

/// Maximum depth of nested inheritance. A chain longer than this is almost
/// certainly an `$inherit` cycle (two objects that derive from each other),
/// which would otherwise recurse until the stack overflows.
const MAX_INHERITANCE_DEPTH: usize = 64;

/// The directives handled here, in the order they are looked for.
const STRUCTURAL_DIRECTIVE_KEYS: [&str; 3] = ["$inherit", "$extend", "$append"];

/// Evaluates every structural directive whose target is known, resolves `$id`,
/// and reports whether anything was expanded.
pub fn process_structural_directives_depth_first(
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
    identifier_counters: &mut IdentifierCounters,
    stage: ResolutionStage,
) -> Result<bool, Box<dyn Error>> {
    let mut expanded_any = false;
    expand_structural_directives(
        current_node,
        root_node,
        identifier_counters,
        stage,
        0,
        &mut expanded_any,
    )?;
    Ok(expanded_any)
}

/// Whether a directive names a target the tree cannot work out yet.
///
/// A target that reads a field the tree has not grown is not a missing target;
/// it is an early one. The same goes for one that names a node which has not
/// been built yet — an object waiting for the file that declares it. Such a
/// directive is left where it is and tried again once the rest of the tree has
/// been expanded.
fn directive_target_is_unresolved(
    directive_node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    stage: ResolutionStage,
) -> bool {
    unresolved_directive_targets(directive_node, root_node, stage).is_some()
}

/// The targets a directive still cannot reach, and why, or `None` when it is
/// ready. Used both to defer a directive and, once the build has run out of
/// rounds, to say what it was waiting for.
fn unresolved_directive_targets(
    directive_node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    stage: ResolutionStage,
) -> Option<Vec<String>> {
    let key = directive_node.borrow().key.clone().unwrap_or_default();
    if !STRUCTURAL_DIRECTIVE_KEYS.contains(&key.as_str()) {
        return None;
    }

    let raw_value = directive_node.borrow().value.clone()?;
    let mut unresolved = Vec::new();

    for path in extract_string_list_from_value(&raw_value) {
        let Ok(resolved_path) = try_resolve_jinja_reference_string_at(
            &path,
            Rc::clone(directive_node),
            Rc::clone(root_node),
            stage,
        ) else {
            unresolved.push(format!("{} (a field it reads has no value yet)", path));
            continue;
        };

        // `$append` carries items rather than a path, and `$extend` tolerates a
        // target that is simply not there. `$inherit` does not: an object that
        // says what it derives from and does not get it is silently wrong.
        if key == "$inherit"
            && find_target_node_using_reference_path(
                &resolved_path,
                Rc::clone(directive_node),
                Rc::clone(root_node),
            )
            .is_none()
        {
            unresolved.push(format!("'{}', which does not exist", resolved_path));
        }
    }

    if unresolved.is_empty() {
        None
    } else {
        Some(unresolved)
    }
}

/// Names what a directive left over at the end of the build was waiting for.
pub fn describe_unresolved_directive(
    node: &TreeNodeReference,
    root_node: &TreeNodeReference,
) -> Option<String> {
    // Reported only once the build has run out of moves, so the whole
    // configuration is there to be read.
    let unresolved =
        unresolved_directive_targets(node, root_node, ResolutionStage::AfterStructureMerge)?;
    let node_borrowed = node.borrow();

    Some(format!(
        "{} at '{}' still reads {}",
        node_borrowed.key.as_deref().unwrap_or_default(),
        node_borrowed.get_path(),
        unresolved.join(", ")
    ))
}

/// `inheritance_depth` counts how many nested `$inherit` expansions deep the
/// current call is. It grows only when an inheritance target is expanded (not
/// during ordinary tree traversal), so legitimately wide or deep trees are
/// unaffected.
fn expand_structural_directives(
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
    identifier_counters: &mut IdentifierCounters,
    stage: ResolutionStage,
    inheritance_depth: usize,
    expanded_any: &mut bool,
) -> Result<(), Box<dyn Error>> {
    if inheritance_depth > MAX_INHERITANCE_DEPTH {
        return Err(format!(
            "Possible $inherit cycle: inheritance nesting exceeded {} at '{}'. \
             Check for objects that inherit from each other.",
            MAX_INHERITANCE_DEPTH,
            current_node.borrow().get_path()
        )
        .into());
    }

    let mut deferred_directives: Vec<TreeNodeReference> = Vec::new();
    let mut directives_expanded = 0;

    while let Some(directive_node) =
        find_next_structural_directive(&current_node, &deferred_directives)
    {
        if directive_target_is_unresolved(&directive_node, &root_node, stage) {
            deferred_directives.push(Rc::clone(&directive_node));
            continue;
        }

        directives_expanded += 1;
        if directives_expanded > MAX_DIRECTIVES_PER_NODE {
            return Err(format!(
                "Too many structural directives expanded on '{}' (over {}); \
                 possible $extend/$append cycle.",
                current_node.borrow().get_path(),
                MAX_DIRECTIVES_PER_NODE
            )
            .into());
        }

        execute_structural_directive(
            Rc::clone(&directive_node),
            Rc::clone(&current_node),
            Rc::clone(&root_node),
            identifier_counters,
            stage,
            inheritance_depth,
            expanded_any,
        )?;
        *expanded_any = true;

        current_node
            .borrow_mut()
            .children
            .retain(|child| !Rc::ptr_eq(child, &directive_node));
    }

    resolve_identifier_directive_for_node(
        Rc::clone(&current_node),
        Rc::clone(&root_node),
        "$id",
        identifier_counters,
    );

    let children = current_node.borrow().children.clone();
    for child in children {
        if is_each_template(&child) {
            continue;
        }
        expand_structural_directives(
            child,
            Rc::clone(&root_node),
            identifier_counters,
            stage,
            inheritance_depth,
            expanded_any,
        )?;
    }

    Ok(())
}

/// Picks the next directive to evaluate among the node's children.
///
/// `$inherit` goes first, so a list built by `$extend` or `$append` is built
/// on top of whatever was inherited rather than beside it. Those two are then
/// taken in the order they are written, because that order is what decides
/// where their items land in the list.
fn find_next_structural_directive(
    parent_node: &TreeNodeReference,
    deferred_directives: &[TreeNodeReference],
) -> Option<TreeNodeReference> {
    let children = parent_node.borrow().children.clone();

    let is_available = |child: &&TreeNodeReference| {
        !deferred_directives
            .iter()
            .any(|deferred| Rc::ptr_eq(deferred, child))
    };

    let inherit_directive = children
        .iter()
        .filter(is_available)
        .find(|child| child.borrow().key.as_deref() == Some("$inherit"));

    if let Some(inherit_directive) = inherit_directive {
        return Some(Rc::clone(inherit_directive));
    }

    children
        .iter()
        .filter(is_available)
        .find(|child| {
            matches!(
                child.borrow().key.as_deref().unwrap_or_default(),
                "$extend" | "$append"
            )
        })
        .map(Rc::clone)
}

fn execute_structural_directive(
    directive_node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    identifier_counters: &mut IdentifierCounters,
    stage: ResolutionStage,
    inheritance_depth: usize,
    expanded_any: &mut bool,
) -> Result<(), Box<dyn Error>> {
    let directive_key = directive_node.borrow().key.clone().unwrap_or_default();

    match directive_key.as_str() {
        "$inherit" => handle_inherit_directive(
            directive_node,
            parent_node,
            root_node,
            identifier_counters,
            stage,
            inheritance_depth,
            expanded_any,
        ),
        "$extend" => handle_extend_directive(directive_node, parent_node, root_node, stage),
        "$append" => handle_append_directive(directive_node, parent_node),
        _ => Ok(()),
    }
}

// =====================================================================
// $inherit
// =====================================================================

/// Derives an object from another one, named by a path.
///
/// The point of it is that the path is a template, so an object can say what
/// it derives from using a value it carries itself:
///
/// ```yaml
/// # sensors/_each/_self.yaml
/// $inherit: "sensor_types.{{ parent().sensor_type }}"
/// ```
///
/// Written inside an `_each` folder, that one line gives every sensor the
/// values of whichever sensor type it named. `parent()` is the object carrying
/// the directive — the sensor — which is how the same line reads a different
/// value in each of them.
///
/// What the target says is merged *underneath* the object: anything the object
/// states for itself wins, exactly as it does over an `$include`.
fn handle_inherit_directive(
    directive_node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    identifier_counters: &mut IdentifierCounters,
    stage: ResolutionStage,
    inheritance_depth: usize,
    expanded_any: &mut bool,
) -> Result<(), Box<dyn Error>> {
    let raw_value = directive_node.borrow().value.clone();
    let Some(value) = raw_value else {
        return Ok(());
    };

    // Everything named is assembled off to one side first, so a directive
    // listing several paths reads the way a list reads: each one refines what
    // came before it. Only then does the result meet the object that asked for
    // it, whose own values win over all of them.
    let staging_node = new_tree_node_reference(CoalTree {
        key: parent_node.borrow().key.clone(),
        parent: Some(Rc::downgrade(&parent_node)),
        origin: directive_node.borrow().origin.clone(),
        ..Default::default()
    });

    for unresolved_path in extract_string_list_from_value(&value) {
        let Ok(resolved_path) = try_resolve_jinja_reference_string_at(
            &unresolved_path,
            Rc::clone(&directive_node),
            Rc::clone(&root_node),
            stage,
        ) else {
            continue;
        };
        check_reference_path_is_absolute(&resolved_path)?;

        let Some(target_node) = find_target_node_using_reference_path(
            &resolved_path,
            Rc::clone(&directive_node),
            Rc::clone(&root_node),
        ) else {
            continue;
        };

        // Resolve every "$id" inside the target before it is copied, so every
        // object inheriting from it receives the same number rather than the
        // tag. "$id_after" is deliberately left alone and copied as it is.
        resolve_identifier_directives_depth_first(
            Rc::clone(&target_node),
            Rc::clone(&root_node),
            "$id",
            identifier_counters,
        );

        // The target may itself inherit, so its own directives are expanded on
        // a copy before anything is taken from it. The copy is what keeps the
        // original untouched for whoever else inherits from it.
        let copied_target = clone_tree_node_recursively(target_node, Rc::clone(&staging_node));
        expand_structural_directives(
            Rc::clone(&copied_target),
            Rc::clone(&root_node),
            identifier_counters,
            stage,
            inheritance_depth + 1,
            expanded_any,
        )?;

        merge_object_into(copied_target, Rc::clone(&staging_node), false);
    }

    merge_object_into(staging_node, parent_node, true);
    Ok(())
}

/// Splices one object into another.
///
/// `preserve_existing` says who wins when both sides give the same field: it
/// is false while the targets of one directive are assembled, so a later one
/// refines an earlier one, and true when the result reaches the object that
/// asked for it.
fn merge_object_into(
    source_node: TreeNodeReference,
    target_node: TreeNodeReference,
    preserve_existing: bool,
) {
    let source_children = source_node.borrow().children.clone();

    for child in source_children {
        child.borrow_mut().parent = Some(Rc::downgrade(&target_node));
        add_or_replace_child_node_in_parent(Rc::clone(&target_node), child, preserve_existing);
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
    stage: ResolutionStage,
) -> Result<(), Box<dyn Error>> {
    let raw_value = directive_node.borrow().value.clone();

    if let Some(value) = raw_value {
        for unresolved_path in extract_string_list_from_value(&value) {
            let Ok(resolved_path) = try_resolve_jinja_reference_string_at(
                &unresolved_path,
                Rc::clone(&directive_node),
                Rc::clone(&root_node),
                stage,
            ) else {
                continue;
            };
            check_reference_path_is_absolute(&resolved_path)?;

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
// _each
// =====================================================================

/// Merges every `_each` template into the fields it applies to, and reports
/// whether that added anything.
///
/// The template is applied on every round rather than consumed by the first
/// one, because a field it should reach may not exist yet: `$include` and the
/// structure layers both add siblings while the tree is still growing. Since a
/// value already present always wins, applying the template again once nothing
/// new has arrived changes nothing, which is what lets the build know it has
/// settled.
///
/// Directives are the exception, and are copied into a given field exactly
/// once. A directive is *consumed* by the phase that carries it out, so its
/// absence the next time round does not mean the field never had it — and
/// copying it in again would set the build going round forever.
pub fn apply_each_templates_depth_first(
    node: TreeNodeReference,
    root_node: TreeNodeReference,
    identifier_counters: &mut IdentifierCounters,
) -> bool {
    let mut applied_anything = false;

    let template_node = node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key.as_deref() == Some(EACH_TEMPLATE_KEY))
        .cloned();

    if let Some(template_node) = template_node {
        // Every "$id" inside the template is resolved before the template is
        // copied anywhere, so each copy receives the number itself rather than
        // the tag. That is what makes one `$id` one number, shared by every
        // field the template reaches — where `$id_after`, which survives
        // copying, gives each of them one of their own.
        resolve_identifier_directives_depth_first(
            Rc::clone(&template_node),
            Rc::clone(&root_node),
            "$id",
            identifier_counters,
        );

        let target_nodes: Vec<TreeNodeReference> = node
            .borrow()
            .children
            .iter()
            .filter(|child| !is_system_directive(child))
            .cloned()
            .collect();

        for target_node in target_nodes {
            let directives_already_copied = target_node.borrow().each_directives_copied;
            applied_anything |= merge_template_children_into(
                &template_node,
                &target_node,
                directives_already_copied,
            );
            target_node.borrow_mut().each_directives_copied = true;
        }
    }

    let children = node.borrow().children.clone();
    for child in children {
        // A template inside a template applies to the copies, not here.
        if is_each_template(&child) {
            continue;
        }
        applied_anything |= apply_each_templates_depth_first(
            child,
            Rc::clone(&root_node),
            identifier_counters,
        );
    }

    applied_anything
}

/// Copies the template's children onto a field, leaving anything the field
/// already says untouched. Reports whether it wrote anything.
fn merge_template_children_into(
    template_node: &TreeNodeReference,
    target_node: &TreeNodeReference,
    skip_directives: bool,
) -> bool {
    let mut wrote_anything = false;
    let template_children = template_node.borrow().children.clone();

    for template_child in template_children {
        if skip_directives && is_system_directive(&template_child) {
            continue;
        }

        let child_key = template_child.borrow().key.clone();

        let existing_child = target_node
            .borrow()
            .children
            .iter()
            .find(|child| child.borrow().key == child_key)
            .cloned();

        match existing_child {
            Some(existing_child) => {
                wrote_anything |= merge_template_node_into(&template_child, &existing_child);
            }
            None => {
                let cloned_child =
                    clone_tree_node_recursively(template_child, Rc::clone(target_node));
                target_node.borrow_mut().children.push(cloned_child);
                wrote_anything = true;
            }
        }
    }

    wrote_anything
}

/// Merges one template node onto the field of the same name. An object is
/// merged field by field; a value the configuration already gives is left
/// exactly as it is.
fn merge_template_node_into(
    template_node: &TreeNodeReference,
    target_node: &TreeNodeReference,
) -> bool {
    if template_node.borrow().children.is_empty() {
        let target_is_empty = {
            let target_borrowed = target_node.borrow();
            target_borrowed.value.is_none() && target_borrowed.children.is_empty()
        };

        if !target_is_empty {
            return false;
        }

        let template_borrowed = template_node.borrow();
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.value = template_borrowed.value.clone();
        target_borrowed.origin = template_borrowed.origin.clone();
        target_borrowed.span = template_borrowed.span.clone();
        target_borrowed.template_resolved = template_borrowed.template_resolved;
        return true;
    }

    merge_template_children_into(template_node, target_node, false)
}

// =====================================================================
// $ignore
// =====================================================================

/// Drops every field an `$ignore` list names, and reports whether it dropped
/// any.
///
/// The names are read relative to the object the list is written on, so
/// `$ignore` in a directory's `_self.yaml` names that directory's own
/// entries. A dotted name reaches further down for the occasions where that
/// is what is meant.
///
/// A name that matches nothing is not an error. The point of the list is to
/// say which of the things that *might* be there should not be built, and a
/// configuration that has already dropped one should not have to keep the
/// list in step.
///
/// This runs after `_each`, so a field is ignored whether or not a template
/// had something to say about it. The list is not consumed, so a field that
/// arrives later — from an `$include`, or a structure layer — is dropped too.
pub fn apply_ignore_lists_depth_first(
    node: TreeNodeReference,
    root_node: TreeNodeReference,
    stage: ResolutionStage,
) -> bool {
    let mut dropped_anything = false;

    let ignore_node = node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key.as_deref() == Some(IGNORE_KEY))
        .cloned();

    if let Some(ignore_node) = ignore_node {
        let raw_value = ignore_node.borrow().value.clone();

        for raw_name in raw_value
            .as_ref()
            .map(extract_string_list_from_value)
            .unwrap_or_default()
        {
            // A name may be worked out from the configuration, so a build can
            // decide what to leave out. One that is not settled yet is left
            // for a later round rather than treated as a literal.
            let Ok(name) = try_resolve_jinja_reference_string_at(
                &raw_name,
                Rc::clone(&ignore_node),
                Rc::clone(&root_node),
                stage,
            ) else {
                continue;
            };
            dropped_anything |= drop_named_field(&node, &name);
        }
    }

    let children = node.borrow().children.clone();
    for child in children {
        // A template's own list applies to the copies, not to the stencil.
        if is_each_template(&child) {
            continue;
        }
        dropped_anything |= apply_ignore_lists_depth_first(child, Rc::clone(&root_node), stage);
    }

    dropped_anything
}

/// Removes the field a (possibly dotted) name reaches, reporting whether it
/// was there to remove.
fn drop_named_field(node: &TreeNodeReference, name: &str) -> bool {
    let segments: Vec<&str> = name
        .split('.')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect();

    let Some((last_segment, parent_segments)) = segments.split_last() else {
        return false;
    };

    let mut current_node = Rc::clone(node);
    for segment in parent_segments {
        let child = current_node
            .borrow()
            .children
            .iter()
            .find(|child| child.borrow().key.as_deref() == Some(*segment))
            .cloned();
        match child {
            Some(child) => current_node = child,
            None => return false,
        }
    }

    let had_it = current_node
        .borrow()
        .children
        .iter()
        .any(|child| child.borrow().key.as_deref() == Some(*last_segment));

    if had_it {
        current_node
            .borrow_mut()
            .children
            .retain(|child| child.borrow().key.as_deref() != Some(*last_segment));
    }
    had_it
}

fn is_system_directive(node: &TreeNodeReference) -> bool {
    node.borrow()
        .key
        .as_deref()
        .is_some_and(|key| key.starts_with('$'))
}
