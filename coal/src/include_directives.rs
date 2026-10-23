//! `$include`: pulling a piece of the `_definitions` namespace into the tree.
//!
//! Every `_definitions` folder in the configuration is loaded exactly the way the
//! configuration root is. `$include` names a piece of the namespace by path —
//! `$include: templates.base` for `_definitions/templates/base.yaml` — so what
//! is included is a name, not a file location.
//!
//! Which namespace depends on where the include is written: the definitions of
//! the directory its file sits in and of every directory above it, the deeper
//! ones overriding (see `definition_scopes`). An include that arrives inside an
//! included definition reads what the place that asked for the definition
//! reads, so an override lower down reaches through shared definitions too.
//!
//! A path may name a folder as readily as a file, because the namespace is a
//! tree like any other: `$include: templates` brings in everything under
//! `_definitions/templates/`.

use crate::coal_tree::CoalTree;
use crate::definition_scopes::{DefinitionScopes, directory_of_origin};
use crate::directory_loader::{EACH_TEMPLATE_KEY, is_each_template};
use crate::reference_resolution::try_resolve_jinja_reference_string;
use crate::tree_operations::{add_or_replace_child_node_in_parent, clone_tree_node_recursively};
use crate::type_aliases::{TreeNodeReference, new_tree_node_reference};
use crate::yaml_values::extract_string_list_from_value;
use serde_yaml::Value;
use std::error::Error;
use std::rc::Rc;

/// How many rounds of includes may nest before the build gives up. An include
/// whose content includes something else is ordinary; a chain this long is a
/// cycle between two entries of the namespace.
const MAX_INCLUDE_DEPTH: usize = 64;

/// Expands every `$include` in the tree whose path is known, and reports
/// whether it expanded any.
///
/// A path that still reads an unresolved field is left where it is rather than
/// reported as missing: the build comes back once the field it names has a
/// value, which is what lets one part of the configuration decide what another
/// part includes.
pub fn process_include_directives_depth_first(
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
    definition_scopes: &DefinitionScopes,
) -> Result<bool, Box<dyn Error>> {
    let mut included_any = false;
    expand_includes_depth_first(
        current_node,
        root_node,
        definition_scopes,
        0,
        &mut included_any,
    )?;
    Ok(included_any)
}

/// The paths a directive still cannot work out, so the build can say what it
/// was waiting for if they never become possible.
pub fn describe_unresolved_include(node: &TreeNodeReference) -> Option<String> {
    let node_borrowed = node.borrow();
    let key = node_borrowed.key.as_deref()?;
    if !key.starts_with("$include") {
        return None;
    }

    let unresolved: Vec<String> = node_borrowed
        .value
        .as_ref()
        .map(extract_string_list_from_value)
        .unwrap_or_default()
        .into_iter()
        .filter(|path| path.contains("{{") || path.contains("{%"))
        .collect();

    if unresolved.is_empty() {
        return None;
    }
    Some(format!(
        "{} at '{}' still reads {}",
        key,
        node_borrowed.get_path(),
        unresolved.join(", ")
    ))
}

fn expand_includes_depth_first(
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
    definition_scopes: &DefinitionScopes,
    include_depth: usize,
    included_any: &mut bool,
) -> Result<(), Box<dyn Error>> {
    let children = current_node.borrow().children.clone();
    let mut expanded_directives = Vec::new();

    for child in children {
        let outcome = process_single_include_directive(
            Rc::clone(&child),
            Rc::clone(&current_node),
            Rc::clone(&root_node),
            definition_scopes,
            include_depth,
            included_any,
        )?;

        match outcome {
            IncludeOutcome::Expanded => expanded_directives.push(Rc::clone(&child)),
            // Left in place; the next round tries again.
            IncludeOutcome::Deferred => {}
            IncludeOutcome::NotAnInclude if !is_each_template(&child) => {
                expand_includes_depth_first(
                    Rc::clone(&child),
                    Rc::clone(&root_node),
                    definition_scopes,
                    include_depth,
                    included_any,
                )?
            }
            IncludeOutcome::NotAnInclude => {}
        }
    }

    if !expanded_directives.is_empty() {
        current_node.borrow_mut().children.retain(|child| {
            !expanded_directives
                .iter()
                .any(|expanded| Rc::ptr_eq(child, expanded))
        });
    }
    Ok(())
}

/// What became of one include directive.
enum IncludeOutcome {
    /// Carried out; the directive can be removed.
    Expanded,
    /// The path is not known yet. The directive stays for a later round.
    Deferred,
    NotAnInclude,
}

fn process_single_include_directive(
    node: TreeNodeReference,
    parent_node: TreeNodeReference,
    root_node: TreeNodeReference,
    definition_scopes: &DefinitionScopes,
    include_depth: usize,
    included_any: &mut bool,
) -> Result<IncludeOutcome, Box<dyn Error>> {
    let node_key = match node.borrow().key.clone() {
        Some(key) => key,
        None => return Ok(IncludeOutcome::NotAnInclude),
    };

    let is_optional = match node_key.as_str() {
        "$include" => false,
        "$include_optional" => true,
        _ => return Ok(IncludeOutcome::NotAnInclude),
    };

    let raw_paths = include_target_paths(&node);
    let mut resolved_paths = Vec::new();

    for raw_path in &raw_paths {
        // A path that still reads an unresolved field is not a missing name;
        // it is an early one.
        let Ok(resolved_path) = try_resolve_jinja_reference_string(
            raw_path,
            Rc::clone(&node),
            Rc::clone(&root_node),
        ) else {
            return Ok(IncludeOutcome::Deferred);
        };
        resolved_paths.push(resolved_path);
    }

    // Everything named is assembled off to one side first, so a directive
    // listing several paths reads the way a list reads: each one refines what
    // came before it. Only then does the result meet the field that asked for
    // it, where what is written locally wins.
    let staging_node = new_tree_node_reference(CoalTree {
        key: parent_node.borrow().key.clone(),
        parent: Some(Rc::downgrade(&parent_node)),
        origin: node.borrow().origin.clone(),
        ..Default::default()
    });

    let site_directory = include_site_directory(&node);
    let definitions_namespace = match &site_directory {
        Some(directory) => definition_scopes.visible_from_directory(directory),
        None => definition_scopes.visible_from_root(),
    };

    for resolved_path in resolved_paths {
        let Some(target_node) = find_in_namespace(&definitions_namespace, &resolved_path) else {
            if is_optional {
                continue;
            }
            return Err(describe_missing_include(&definitions_namespace, &resolved_path, &node).into());
        };

        guard_against_include_cycle(&resolved_path, include_depth)?;

        let copied_node = clone_tree_node_recursively(target_node, Rc::clone(&staging_node));
        if let Some(directory) = &site_directory {
            mark_include_scope(&copied_node, directory);
        }
        expand_includes_depth_first(
            Rc::clone(&copied_node),
            Rc::clone(&root_node),
            definition_scopes,
            include_depth + 1,
            included_any,
        )?;
        merge_tree_into(copied_node, Rc::clone(&staging_node), false);
    }

    merge_tree_into(staging_node, Rc::clone(&parent_node), true);

    *included_any = true;
    Ok(IncludeOutcome::Expanded)
}

/// The directory whose definitions an include reads: the one it was handed by
/// the definition it arrived in, or else the one its own file sits in.
fn include_site_directory(include_node: &TreeNodeReference) -> Option<String> {
    let node = include_node.borrow();
    node.include_scope
        .clone()
        .or_else(|| node.origin.as_deref().map(directory_of_origin))
}

/// Hands every include inside a copied definition the directory of the place
/// that asked for it.
fn mark_include_scope(node: &TreeNodeReference, directory: &str) {
    let children = node.borrow().children.clone();
    for child in children {
        let is_include = child
            .borrow()
            .key
            .as_deref()
            .is_some_and(|key| key == "$include" || key == "$include_optional");
        if is_include {
            child.borrow_mut().include_scope = Some(directory.to_string());
        }
        mark_include_scope(&child, directory);
    }
}

/// Splices one tree into another.
///
/// `preserve_existing` says who wins when both sides give the same field. It
/// is false while the paths of one directive are assembled, so a later path
/// refines an earlier one, and true when the result is handed to the field
/// that asked for it: an include supplies a starting point, it does not
/// overrule what is written on the spot.
fn merge_tree_into(
    source_node: TreeNodeReference,
    target_node: TreeNodeReference,
    preserve_existing: bool,
) {
    let source_children = source_node.borrow().children.clone();

    if source_children.is_empty() {
        // Nothing was found to include at all — an `$include_optional` that
        // named something absent. Writing the emptiness over the field would
        // turn an object the configuration states into a null.
        if source_node.borrow().value.is_none() {
            return;
        }
        if !preserve_existing || holds_nothing_of_its_own(&target_node) {
            target_node.borrow_mut().value = source_node.borrow().value.clone();
        }
        return;
    }

    let source_is_list = source_node.borrow().is_list_type();
    let mut next_list_index = find_next_list_index(&target_node);

    for child in source_children {
        child.borrow_mut().parent = Some(Rc::downgrade(&target_node));

        if source_is_list {
            child.borrow_mut().key = Some(next_list_index.to_string());
            target_node.borrow_mut().children.push(Rc::clone(&child));
            next_list_index += 1;
        } else {
            add_or_replace_child_node_in_parent(
                Rc::clone(&target_node),
                child,
                preserve_existing,
            );
        }
    }
}

/// Whether the field that asked for an include has nothing of its own to keep.
///
/// A field written as `port: {$include: ...}` still parses as a mapping, so its
/// recorded value is the mapping the directive was written in rather than
/// anything the configuration states. Reading that as "already has a value"
/// would drop every scalar and list element the namespace holds — the include
/// would quietly produce an empty object. What counts as its own is a plain
/// value it was given outright, or a field beside the directive.
fn holds_nothing_of_its_own(target_node: &TreeNodeReference) -> bool {
    let node = target_node.borrow();

    let has_a_field_of_its_own = node.children.iter().any(|child| {
        !child
            .borrow()
            .key
            .as_deref()
            .unwrap_or_default()
            .starts_with('$')
    });
    if has_a_field_of_its_own {
        return false;
    }

    !matches!(
        node.value,
        Some(Value::Bool(_) | Value::Number(_) | Value::String(_))
    )
}

fn find_next_list_index(node: &TreeNodeReference) -> usize {
    node.borrow()
        .children
        .iter()
        .filter_map(|child| {
            child
                .borrow()
                .key
                .as_deref()
                .unwrap_or("")
                .parse::<usize>()
                .ok()
        })
        .max()
        .map(|highest_index| highest_index + 1)
        .unwrap_or(0)
}

fn guard_against_include_cycle(path: &str, include_depth: usize) -> Result<(), Box<dyn Error>> {
    if include_depth >= MAX_INCLUDE_DEPTH {
        return Err(format!(
            "Possible $include cycle: nesting exceeded {} while including '{}'. \
             Check for entries of the _definitions namespace that include each other.",
            MAX_INCLUDE_DEPTH, path
        )
        .into());
    }
    Ok(())
}

/// Walks a dotted path into the include namespace. An `_each` template is not
/// addressable: it belongs to the folder that declared it.
fn find_in_namespace(
    definitions_namespace: &TreeNodeReference,
    path: &str,
) -> Option<TreeNodeReference> {
    let mut current_node = Rc::clone(definitions_namespace);

    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        if segment == EACH_TEMPLATE_KEY {
            return None;
        }
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

/// Says what the namespace does have at the point the path went wrong, which
/// is nearly always enough to spot a typo or a folder that was never created.
fn describe_missing_include(
    definitions_namespace: &TreeNodeReference,
    path: &str,
    node: &TreeNodeReference,
) -> String {
    let mut current_node = Rc::clone(definitions_namespace);
    let mut reached = Vec::new();

    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        let matching_child = current_node
            .borrow()
            .children
            .iter()
            .find(|child| child.borrow().key.as_deref() == Some(segment))
            .cloned();

        match matching_child {
            Some(child) => {
                reached.push(segment.to_string());
                current_node = child;
            }
            None => break,
        }
    }

    let available: Vec<String> = current_node
        .borrow()
        .children
        .iter()
        .filter_map(|child| child.borrow().key.clone())
        .filter(|key| !key.starts_with('$'))
        .collect();

    let location = if reached.is_empty() {
        "the _definitions visible there".to_string()
    } else {
        format!("'{}'", reached.join("."))
    };

    format!(
        "$include at '{}' names '{}', which the _definitions visible there do not have.\n  {} holds: {}",
        node.borrow().get_path(),
        path,
        location,
        if available.is_empty() {
            "nothing".to_string()
        } else {
            available.join(", ")
        }
    )
}

fn include_target_paths(include_node: &TreeNodeReference) -> Vec<String> {
    include_node
        .borrow()
        .value
        .as_ref()
        .map(extract_string_list_from_value)
        .unwrap_or_default()
}
