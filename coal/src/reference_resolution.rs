//! Reference plumbing: navigating to a node by path, resolving `$parentN`
//! placeholders, and driving the Jinja engine over the whole tree.
//!
//! The templating itself lives in [`crate::jinja_engine`]; this module decides
//! *when* templates are evaluated and what happens to the results.

use crate::jinja_engine::{
    TemplateError, TemplateScope, contains_template, emits_literal_delimiters,
    prepare_tree_for_templates, render_template_text, render_value_template,
    value_contains_template,
};
use crate::tree_operations::rebuild_node_children_from_value;
use crate::type_aliases::TreeNodeReference;
use minijinja::Value as JinjaValue;
use regex::Regex;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::OnceLock;

// =====================================================================
// PATH NAVIGATION
// =====================================================================

/// Resolves a reference path to a tree node. Three addressing modes exist:
/// "ROOT."-prefixed paths resolve from the tree root, "."-prefixed paths
/// climb one parent per leading dot, and bare paths search upward through
/// every enclosing scope before falling back to the root.
///
/// This is COAL's own path syntax, used by directives that point at another
/// node (`$inherit`, `$override`, `$extend`, `$select`) and by the language
/// server. It is not Jinja: a directive value is rendered as a template first
/// and the resulting text is then read as a path.
pub fn find_target_node_using_reference_path(
    path: &str,
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> Option<TreeNodeReference> {
    if let Some(path_from_root) = path.strip_prefix("ROOT.") {
        return traverse_path_segments(root_node, path_from_root);
    }

    if path.starts_with('.') {
        return resolve_relative_dot_path(path, current_node);
    }

    resolve_path_by_upward_scope_search(path, current_node, root_node)
}

fn resolve_relative_dot_path(
    path: &str,
    current_node: TreeNodeReference,
) -> Option<TreeNodeReference> {
    let leading_dot_count = path
        .chars()
        .take_while(|&character| character == '.')
        .count();
    let mut navigation_node = current_node;

    for _ in 0..leading_dot_count {
        let parent = get_parent_node(&navigation_node);
        navigation_node = parent?;
    }

    let remaining_path = &path[leading_dot_count..];
    if remaining_path.is_empty() {
        Some(navigation_node)
    } else {
        traverse_path_segments(navigation_node, remaining_path)
    }
}

fn resolve_path_by_upward_scope_search(
    path: &str,
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> Option<TreeNodeReference> {
    let mut search_scope = Some(current_node);
    while let Some(scope_node) = search_scope {
        if let Some(found_node) = traverse_path_segments(Rc::clone(&scope_node), path) {
            return Some(found_node);
        }
        search_scope = get_parent_node(&scope_node);
    }

    traverse_path_segments(root_node, path)
}

fn get_parent_node(node: &TreeNodeReference) -> Option<TreeNodeReference> {
    node.borrow()
        .parent
        .as_ref()
        .and_then(|weak_parent| weak_parent.upgrade())
}

fn traverse_path_segments(start_node: TreeNodeReference, path: &str) -> Option<TreeNodeReference> {
    let mut current_node = start_node;
    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
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

// =====================================================================
// $parent PLACEHOLDERS
// =====================================================================

static PARENT_PLACEHOLDER_REGEX: OnceLock<Regex> = OnceLock::new();

/// Matches "$parent" optionally followed by the number of levels to walk up
/// (e.g. "$parent", "$parent2", "$parent10").
fn parent_placeholder_regex() -> &'static Regex {
    PARENT_PLACEHOLDER_REGEX.get_or_init(|| Regex::new(r"\$parent(\d*)").unwrap())
}

/// Replaces every "$parentN" placeholder in `text` with the key of the
/// ancestor `N` levels above `node`. Bare "$parent" is equivalent to
/// "$parent1" (the immediate parent). Placeholders that reach past the tree
/// root are left in place. Resolution is anchored at `node`, so the caller
/// chooses the field the placeholders are relative to.
pub fn resolve_parent_placeholders(text: &str, node: &TreeNodeReference) -> String {
    if !text.contains("$parent") {
        return text.to_string();
    }

    parent_placeholder_regex()
        .replace_all(
            text,
            |captures: &regex::Captures| match parse_parent_placeholder_levels(&captures[1]) {
                Some(levels_up) => ancestor_key_at_level(node, levels_up)
                    .unwrap_or_else(|| captures[0].to_string()),
                None => captures[0].to_string(),
            },
        )
        .into_owned()
}

/// An empty suffix means one level up; "0" is meaningless and left alone.
fn parse_parent_placeholder_levels(digits: &str) -> Option<usize> {
    if digits.is_empty() {
        return Some(1);
    }
    match digits.parse::<usize>() {
        Ok(0) | Err(_) => None,
        Ok(levels_up) => Some(levels_up),
    }
}

fn ancestor_key_at_level(node: &TreeNodeReference, levels_up: usize) -> Option<String> {
    let mut ancestor = Rc::clone(node);
    for _ in 0..levels_up {
        ancestor = get_parent_node(&ancestor)?;
    }
    ancestor.borrow().key.clone()
}

// =====================================================================
// SINGLE-STRING RESOLUTION
// =====================================================================

/// Renders a template in the scope of `current_node` and returns the text.
/// Used by the directives that need a string before the tree is finished —
/// include paths, inheritance targets, counter names — and by the language
/// server. A template that cannot be rendered is returned unchanged, so a
/// reference to an as-yet-unbuilt part of the tree simply stays as written.
pub fn resolve_jinja_reference_string(
    input: &str,
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> String {
    if !contains_template(input) {
        return input.to_string();
    }

    let scope = TemplateScope::for_node(&current_node, &root_node);
    render_template_text(input, &scope).unwrap_or_else(|_| input.to_string())
}

// =====================================================================
// TREE-WIDE JINJA RESOLUTION
// =====================================================================

/// A backstop on the number of passes. Every pass resolves at least one field
/// or ends the loop, so this is only ever reached by a configuration with a
/// reference chain thousands of links long.
const MAX_RESOLUTION_PASSES: usize = 1024;

/// Resolves every Jinja template in the tree.
///
/// Rendering happens in passes. Each pass renders every field whose own
/// references are already resolved, and another pass follows for as long as
/// that moves something forward. A field that references a field that
/// references a value therefore resolves in as many passes as the chain is
/// long, regardless of the order the fields are written in — and a field is
/// never rendered against a reference that is still a template.
///
/// Each pass renders against the tree as it stood when the pass began and only
/// writes the results afterwards, so no field can see half of another field's
/// resolution.
///
/// Fields that never become renderable — a reference cycle, a name that does
/// not exist, a filter used on the wrong kind of value — are left in the
/// output as written, and a warning explaining each one is printed to stderr.
pub fn resolve_all_jinja_values_in_tree(node: TreeNodeReference, root_node: TreeNodeReference) {
    let mut last_reasons = BTreeMap::new();
    let mut exhausted_passes = false;

    // The first round resolves everything that can be resolved without any
    // give: a field waits for every field it reads. The second round then lets
    // the fields that read their own container go ahead, which is the one
    // shape that would otherwise wait for itself.
    for relax_enclosing_scopes in [false, true] {
        let (reasons, ran_out_of_passes) =
            run_resolution_rounds(&node, &root_node, relax_enclosing_scopes);
        last_reasons = reasons;
        exhausted_passes |= ran_out_of_passes;
    }

    report_unresolved_templates(node, &last_reasons, exhausted_passes);
}

fn run_resolution_rounds(
    node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    relax_enclosing_scopes: bool,
) -> (BTreeMap<String, TemplateError>, bool) {
    let mut last_reasons = BTreeMap::new();

    for _ in 0..MAX_RESOLUTION_PASSES {
        let mut pass = ResolutionPass {
            root_node: Rc::clone(root_node),
            prepared_tree: prepare_tree_for_templates(Rc::clone(root_node)),
            relax_enclosing_scopes,
            resolved_values: Vec::new(),
            reasons: BTreeMap::new(),
        };

        collect_resolved_values(Rc::clone(node), &mut pass);
        let ResolutionPass {
            resolved_values,
            reasons,
            ..
        } = pass;
        last_reasons = reasons;

        if resolved_values.is_empty() {
            return (last_reasons, false);
        }

        for (resolved_node, resolved_value) in resolved_values {
            apply_resolved_value(&resolved_node, resolved_value);
        }
    }

    (last_reasons, true)
}

/// One pass over the tree. Nothing is written while a pass is running: the
/// results are collected first and applied afterwards, so every field in the
/// pass sees exactly the same tree.
struct ResolutionPass {
    root_node: TreeNodeReference,
    /// The tree as it stood when the pass began, ready for the engine.
    prepared_tree: JinjaValue,
    /// Whether a field reading its own container may go ahead.
    relax_enclosing_scopes: bool,
    /// What this pass resolved, waiting to be written.
    resolved_values: Vec<(TreeNodeReference, Value)>,
    /// Why each field could not be rendered, keyed by field path.
    reasons: BTreeMap<String, TemplateError>,
}

fn collect_resolved_values(node: TreeNodeReference, pass: &mut ResolutionPass) {
    collect_resolved_value_of_node(&node, pass);

    let children = node.borrow().children.clone();
    for child in children {
        collect_resolved_values(child, pass);
    }
}

fn collect_resolved_value_of_node(node: &TreeNodeReference, pass: &mut ResolutionPass) {
    let Some(template_text) = template_text_of_node(node) else {
        return;
    };

    let mut scope = TemplateScope::for_node(node, &pass.root_node)
        .with_prepared_tree(pass.prepared_tree.clone());
    if pass.relax_enclosing_scopes {
        scope = scope.relaxed();
    }

    match render_value_template(&template_text, &scope) {
        // The field read itself. A field that reads the object it lives in —
        // `{{ PARENT }}`, or anything that takes ROOT as a whole — gets its own
        // template back as part of the answer, and accepting that would write
        // the reference into its own value. Only a `{% raw %}` block is meant
        // to produce text that looks like a template.
        Ok(resolved_value)
            if value_contains_text(&resolved_value, &template_text)
                && !emits_literal_delimiters(&template_text) =>
        {
            pass.reasons.insert(
                node.borrow().get_path(),
                TemplateError::Failed("it resolves to a copy of itself".to_string()),
            );
        }
        Ok(resolved_value) => pass.resolved_values.push((Rc::clone(node), resolved_value)),
        Err(reason) => {
            pass.reasons.insert(node.borrow().get_path(), reason);
        }
    }
}

/// Writes a resolved value onto its field. A value that came out as a list or
/// a mapping becomes real tree nodes, so the rest of the pipeline treats it
/// exactly like structure that was written by hand.
fn apply_resolved_value(node: &TreeNodeReference, resolved_value: Value) {
    if matches!(resolved_value, Value::Sequence(_) | Value::Mapping(_)) {
        rebuild_node_children_from_value(node, &resolved_value);
    }

    let mut node_borrowed = node.borrow_mut();
    node_borrowed.value = Some(resolved_value);
    node_borrowed.template_resolved = true;
}

/// Whether the value holds `text` anywhere inside it.
fn value_contains_text(value: &Value, text: &str) -> bool {
    match value {
        Value::String(candidate) => candidate.contains(text),
        Value::Sequence(sequence) => sequence.iter().any(|item| value_contains_text(item, text)),
        Value::Mapping(mapping) => mapping
            .values()
            .any(|entry| value_contains_text(entry, text)),
        _ => false,
    }
}

/// A field carries a template only when its own value is text containing one
/// and the engine has not already finished with it. Nodes with children hold a
/// mapping or a sequence that is rebuilt from those children, so there is
/// nothing to render on them directly.
fn template_text_of_node(node: &TreeNodeReference) -> Option<String> {
    let node_borrowed = node.borrow();
    if !node_borrowed.children.is_empty() || node_borrowed.template_resolved {
        return None;
    }

    match node_borrowed.value {
        Some(Value::String(ref text)) if contains_template(text) => Some(text.clone()),
        _ => None,
    }
}

// =====================================================================
// REPORTING
// =====================================================================

fn report_unresolved_templates(
    node: TreeNodeReference,
    reasons: &BTreeMap<String, TemplateError>,
    exhausted_passes: bool,
) {
    let mut stuck_fields = Vec::new();
    collect_unresolved_fields(node, &mut stuck_fields);

    for field_path in stuck_fields {
        let field_label = if field_path.is_empty() {
            "<root>".to_string()
        } else {
            field_path.clone()
        };

        let reason = match reasons.get(&field_path) {
            Some(reason) => reason.describe(),
            None if exhausted_passes => "the reference chain is too long to resolve".to_string(),
            None => "the reference could not be resolved".to_string(),
        };

        eprintln!(
            "Warning: Jinja template on '{}' left unresolved ({}).",
            field_label, reason
        );
    }
}

fn collect_unresolved_fields(node: TreeNodeReference, stuck_fields: &mut Vec<String>) {
    {
        let node_borrowed = node.borrow();
        let is_stuck = !node_borrowed.template_resolved
            && node_borrowed.children.is_empty()
            && node_borrowed
                .value
                .as_ref()
                .is_some_and(value_contains_template);
        if is_stuck {
            stuck_fields.push(node_borrowed.get_path());
        }
    }

    let children = node.borrow().children.clone();
    for child in children {
        collect_unresolved_fields(child, stuck_fields);
    }
}
