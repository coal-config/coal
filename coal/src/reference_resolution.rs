//! Reference plumbing: navigating to a node by path, and driving the Jinja
//! engine over the whole tree.
//!
//! The templating itself lives in [`crate::jinja_engine`]; this module decides
//! *when* templates are evaluated and what happens to the results.

use crate::jinja_engine::{
    ResolutionStage, TemplateError, TemplateScope, contains_template, emits_literal_delimiters,
    render_template_text, render_value_template, unrendered_template_of_node,
};
use crate::coal_tree::Span;
use crate::directory_loader::is_each_template;
use crate::tree_operations::rebuild_node_children_from_value;
use crate::type_aliases::TreeNodeReference;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::rc::Rc;

// =====================================================================
// PATH NAVIGATION
// =====================================================================

/// Resolves a reference path to a tree node.
///
/// A path starts at the configuration root, exactly as a Jinja reference does:
/// `defaults.production` is that field of the root, wherever the directive
/// naming it was written. To point at something relative to the directive
/// instead, start the path with `parent(n).`, which walks n levels up first.
///
/// This is COAL's own path syntax, used by the directives that point at
/// another node (`$inherit`, `$override`, `$extend`, `$select`) and by the
/// language server. It is not Jinja: a directive value is rendered as a
/// template first and the resulting text is then read as a path.
pub fn find_target_node_using_reference_path(
    path: &str,
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> Option<TreeNodeReference> {
    match split_ancestor_prefix(path) {
        Some((levels_up, remaining_path)) => {
            let mut start_node = current_node;
            for _ in 0..levels_up {
                start_node = get_parent_node(&start_node)?;
            }
            traverse_path_segments(start_node, remaining_path)
        }
        None => traverse_path_segments(root_node, path),
    }
}

/// Recognizes a leading `parent(n).` and returns the level together with the
/// rest of the path.
fn split_ancestor_prefix(path: &str) -> Option<(usize, &str)> {
    let after_call = path.strip_prefix("parent")?.trim_start();
    let (argument, remainder) = after_call.strip_prefix('(')?.split_once(')')?;

    let levels_up = match argument.trim() {
        "" => 1,
        digits => digits.parse::<usize>().ok()?,
    };
    Some((levels_up, remainder.trim_start_matches('.')))
}

/// Reports a path written in the syntax COAL used before references became
/// absolute, so an existing configuration says what to change rather than
/// silently resolving to nothing.
pub fn check_reference_path_is_absolute(path: &str) -> Result<(), Box<dyn Error>> {
    if let Some(remainder) = path.strip_prefix("ROOT.") {
        return Err(format!(
            "'{}' still names the root explicitly. Every path now starts at the root, so write '{}'.",
            path, remainder
        )
        .into());
    }

    if path.starts_with('.') {
        let levels_up = path
            .chars()
            .take_while(|&character| character == '.')
            .count();
        return Err(format!(
            "'{}' is a relative path written with leading dots. Write 'parent({}).{}' instead.",
            path,
            levels_up,
            path.trim_start_matches('.')
        )
        .into());
    }
    Ok(())
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
    try_resolve_jinja_reference_string(input, current_node, root_node)
        .unwrap_or_else(|_| input.to_string())
}

/// The same, at a stated moment in the build. Before the structure has merged,
/// a directive whose target reads beyond its own file is left unresolved and
/// comes back once the configuration has been assembled.
pub fn try_resolve_jinja_reference_string_at(
    input: &str,
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
    stage: ResolutionStage,
) -> Result<String, TemplateError> {
    if !contains_template(input) {
        return Ok(input.to_string());
    }

    let scope = TemplateScope::for_node(&current_node, &root_node).at_stage(stage);
    render_template_text(input, &scope)
}

/// Renders a template in the scope of `current_node`, saying why it could not
/// be rendered rather than handing back the text unchanged.
///
/// Directives use this to tell "there is no such file" apart from "the name of
/// the file is not known yet". A directive that reads a field the tree has not
/// grown yet is not wrong — it is early, and the build comes back to it once
/// the rest of the tree has been expanded.
pub fn try_resolve_jinja_reference_string(
    input: &str,
    current_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> Result<String, TemplateError> {
    if !contains_template(input) {
        return Ok(input.to_string());
    }

    let scope = TemplateScope::for_node(&current_node, &root_node);
    render_template_text(input, &scope)
}

// =====================================================================
// TREE-WIDE JINJA RESOLUTION
// =====================================================================

/// A backstop on the number of passes. Every pass resolves at least one field
/// or ends the loop, so this is only ever reached by a configuration with a
/// reference chain thousands of links long.
const MAX_RESOLUTION_PASSES: usize = 1024;

/// Resolves as much as one pass over the tree can, reporting whether anything
/// moved. Used while the tree is still being expanded: a value that resolves
/// now may be the name of the next file to include.
pub fn resolve_resolvable_jinja_values(
    node: TreeNodeReference,
    root_node: TreeNodeReference,
    stage: ResolutionStage,
) -> bool {
    let mut pass = ResolutionPass {
        root_node: Rc::clone(&root_node),
        relax_enclosing_reads: false,
        stage,
        resolved_values: Vec::new(),
        reasons: BTreeMap::new(),
    };

    collect_resolved_values(node, &mut pass);
    let made_progress = !pass.resolved_values.is_empty();

    for (resolved_node, resolved_value) in pass.resolved_values {
        apply_resolved_value(&resolved_node, resolved_value);
    }
    made_progress
}

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
/// output as written and returned as errors, one per cause rather than one
/// per field left stranded by it.
pub fn resolve_all_jinja_values_in_tree(
    node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> Vec<UnresolvedTemplate> {
    // The first round resolves everything that can be resolved without any
    // give: a field waits for every field it reads. The second round then lets
    // the fields that read a whole enclosing object go ahead, which is the one
    // shape that would otherwise wait for itself.
    let mut reasons = BTreeMap::new();
    let mut exhausted_passes = false;

    for relax_enclosing_reads in [false, true] {
        let (round_reasons, ran_out_of_passes) =
            run_resolution_rounds(&node, &root_node, relax_enclosing_reads);
        reasons = round_reasons;
        exhausted_passes |= ran_out_of_passes;
    }

    collect_unresolved_templates(node, &reasons, exhausted_passes)
}

fn run_resolution_rounds(
    node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    relax_enclosing_reads: bool,
) -> (BTreeMap<String, TemplateError>, bool) {
    let mut last_reasons = BTreeMap::new();

    for _ in 0..MAX_RESOLUTION_PASSES {
        let mut pass = ResolutionPass {
            root_node: Rc::clone(root_node),
            relax_enclosing_reads,
            stage: ResolutionStage::AfterStructureMerge,
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
    /// Whether a field reading a whole enclosing object may go ahead.
    relax_enclosing_reads: bool,
    /// Where in the build this pass runs, which decides how far a reference
    /// may read.
    stage: ResolutionStage,
    /// What this pass resolved, waiting to be written.
    resolved_values: Vec<(TreeNodeReference, Value)>,
    /// Why each field could not be rendered, keyed by field path.
    reasons: BTreeMap<String, TemplateError>,
}

fn collect_resolved_values(node: TreeNodeReference, pass: &mut ResolutionPass) {
    collect_resolved_value_of_node(&node, pass);

    let children = node.borrow().children.clone();
    for child in children {
        // An `_each` template is a stencil. Rendering it where it sits would
        // resolve `parent()` against the folder that declared it, and the
        // copies would then carry that answer instead of their own.
        if is_each_template(&child) {
            continue;
        }
        collect_resolved_values(child, pass);
    }
}

fn collect_resolved_value_of_node(node: &TreeNodeReference, pass: &mut ResolutionPass) {
    let Some(template_text) = template_text_of_node(node) else {
        return;
    };

    let mut scope = TemplateScope::for_node(node, &pass.root_node).at_stage(pass.stage);
    if pass.relax_enclosing_reads {
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
    unrendered_template_of_node(&node.borrow()).map(str::to_string)
}

// =====================================================================
// REPORTING
// =====================================================================

/// A field that could not be resolved on its own account, together with every
/// field that is only unresolved because of it.
///
/// One of these is the head of a chain: whatever it names is the thing to fix,
/// and the fields listed under it resolve on their own once it does.
pub struct UnresolvedTemplate {
    pub field_path: String,
    pub reason: String,
    pub origin: Option<String>,
    pub span: Option<Span>,
    dependents: Vec<DependentField>,
}

impl UnresolvedTemplate {
    /// The whole finding as one message: what cannot resolve, why, and what it
    /// is holding up.
    pub fn message(&self) -> String {
        let mut message = format!(
            "template: {} -> {}",
            label_of_field(&self.field_path),
            self.reason
        );

        if self.dependents.is_empty() {
            return message;
        }

        message.push_str(&format!(
            "\n  {} left unresolved by it:",
            match self.dependents.len() {
                1 => "1 other field is".to_string(),
                count => format!("{} other fields are", count),
            }
        ));

        for dependent in self.dependents.iter().take(MAX_LISTED_DEPENDENTS) {
            message.push_str(&format!("\n    {}", dependent.describe()));
        }

        if self.dependents.len() > MAX_LISTED_DEPENDENTS {
            message.push_str(&format!(
                "\n    ... and {} more.",
                self.dependents.len() - MAX_LISTED_DEPENDENTS
            ));
        }
        message
    }
}

/// How many of a cause's dependents are named before the list is summarized.
/// One unresolvable field at the head of a wide tree can hold up hundreds.
const MAX_LISTED_DEPENDENTS: usize = 8;

/// A field that still holds a template once resolution has run out of moves.
struct StuckField {
    path: String,
    origin: Option<String>,
    span: Option<Span>,
}

/// A field waiting, directly or at a remove, on a root cause.
struct DependentField {
    path: String,
    /// The fields in between, starting at the one this field reads.
    through: Vec<String>,
}

impl DependentField {
    fn describe(&self) -> String {
        if self.through.is_empty() {
            return label_of_field(&self.path);
        }

        let trail: Vec<String> = self.through.iter().map(|field| label_of_field(field)).collect();
        format!("{} (through {})", label_of_field(&self.path), trail.join(" -> "))
    }
}

fn collect_unresolved_templates(
    node: TreeNodeReference,
    reasons: &BTreeMap<String, TemplateError>,
    exhausted_passes: bool,
) -> Vec<UnresolvedTemplate> {
    let mut stuck_fields = Vec::new();
    collect_unresolved_fields(node, &mut stuck_fields);
    group_by_root_cause(&stuck_fields, reasons, exhausted_passes)
}

/// Collapses the stuck fields into the handful of fields that actually caused
/// them.
///
/// Every `WaitingOn` names the field it is waiting for, so the reasons form a
/// graph: following those links from any stuck field arrives either at a field
/// that failed on its own terms — a name that does not exist, a filter that
/// could not be applied — or at a loop of fields waiting on each other. That
/// endpoint is the one worth reporting; everything on the way to it is a
/// consequence.
fn group_by_root_cause(
    stuck_fields: &[StuckField],
    reasons: &BTreeMap<String, TemplateError>,
    exhausted_passes: bool,
) -> Vec<UnresolvedTemplate> {
    let known_fields: BTreeMap<&str, &StuckField> = stuck_fields
        .iter()
        .map(|field| (field.path.as_str(), field))
        .collect();

    let mut causes: BTreeMap<String, UnresolvedTemplate> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();

    for field in stuck_fields {
        let trail = trace_to_root_cause(&field.path, reasons, &known_fields, exhausted_passes);

        let cause = causes.entry(trail.cause.clone()).or_insert_with(|| {
            order.push(trail.cause.clone());
            let head = known_fields.get(trail.cause.as_str());
            UnresolvedTemplate {
                field_path: trail.cause.clone(),
                reason: trail.reason,
                origin: head.and_then(|head| head.origin.clone()),
                span: head.and_then(|head| head.span.clone()),
                dependents: Vec::new(),
            }
        });

        if field.path != cause.field_path {
            cause.dependents.push(DependentField {
                path: field.path.clone(),
                through: trail.through,
            });
        }
    }

    order
        .into_iter()
        .filter_map(|path| causes.remove(&path))
        .collect()
}

/// Where the trail from one stuck field ends.
struct CauseTrail {
    /// The field the trail ends at.
    cause: String,
    /// Why that field is stuck.
    reason: String,
    /// The fields between the field the trail started at and the cause.
    through: Vec<String>,
}

fn trace_to_root_cause(
    start_field: &str,
    reasons: &BTreeMap<String, TemplateError>,
    known_fields: &BTreeMap<&str, &StuckField>,
    exhausted_passes: bool,
) -> CauseTrail {
    let mut walked = vec![start_field.to_string()];

    loop {
        let current_field = walked.last().expect("the trail always has a head");
        let reason = reason_for_field(current_field, reasons, exhausted_passes);

        // A field waiting on something that is not itself stuck has already
        // been told everything there is to tell: whatever it waits for either
        // resolved after this reason was recorded or was never a field.
        let TemplateError::WaitingOn(waited_for) = &reason else {
            return CauseTrail {
                cause: current_field.clone(),
                reason: reason.describe(),
                through: trail_between(&walked),
            };
        };
        if !known_fields.contains_key(waited_for.as_str()) {
            return CauseTrail {
                cause: current_field.clone(),
                reason: reason.describe(),
                through: trail_between(&walked),
            };
        }

        if let Some(loop_start) = walked.iter().position(|field| field == waited_for) {
            return describe_reference_loop(&walked, loop_start);
        }
        walked.push(waited_for.clone());
    }
}

/// The fields between the start of a trail and its end, both excluded.
fn trail_between(walked: &[String]) -> Vec<String> {
    walked
        .get(1..walked.len().saturating_sub(1))
        .unwrap_or_default()
        .to_vec()
}

/// Reports a set of fields that wait on each other.
///
/// The loop is named from its lowest member whichever field the trail started
/// at, so every field caught in it reports the same cause rather than one
/// error per way in.
fn describe_reference_loop(walked: &[String], loop_start: usize) -> CauseTrail {
    let members = &walked[loop_start..];
    let entry_point = members
        .iter()
        .enumerate()
        .min_by_key(|(_, field)| field.as_str())
        .map(|(index, _)| index)
        .unwrap_or(0);

    let mut named: Vec<String> = members
        .iter()
        .cycle()
        .skip(entry_point)
        .take(members.len())
        .map(|field| label_of_field(field))
        .collect();
    let cause = members[entry_point].clone();
    named.push(label_of_field(&cause));

    // Everything up to the loop led into it; the loop itself is the reason.
    let through = walked.get(1..loop_start).unwrap_or_default().to_vec();

    CauseTrail {
        cause,
        reason: format!("it is part of a reference loop: {}", named.join(" -> ")),
        through,
    }
}

fn reason_for_field(
    field_path: &str,
    reasons: &BTreeMap<String, TemplateError>,
    exhausted_passes: bool,
) -> TemplateError {
    match reasons.get(field_path) {
        Some(reason) => reason.clone(),
        None if exhausted_passes => {
            TemplateError::Failed("the reference chain is too long to resolve".to_string())
        }
        None => TemplateError::Failed("the reference could not be resolved".to_string()),
    }
}

fn label_of_field(field_path: &str) -> String {
    if field_path.is_empty() {
        "<root>".to_string()
    } else {
        field_path.to_string()
    }
}

fn collect_unresolved_fields(node: TreeNodeReference, stuck_fields: &mut Vec<StuckField>) {
    {
        let node_borrowed = node.borrow();
        if unrendered_template_of_node(&node_borrowed).is_some() {
            stuck_fields.push(StuckField {
                path: node_borrowed.get_path(),
                origin: node_borrowed.origin.clone(),
                span: node_borrowed.span.clone(),
            });
        }
    }

    let children = node.borrow().children.clone();
    for child in children {
        if is_each_template(&child) {
            continue;
        }
        collect_unresolved_fields(child, stuck_fields);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stuck(paths: &[&str]) -> Vec<StuckField> {
        paths
            .iter()
            .map(|path| StuckField {
                path: (*path).to_string(),
                origin: Some("main.yaml".to_string()),
                span: None,
            })
            .collect()
    }

    fn reasons(entries: &[(&str, TemplateError)]) -> BTreeMap<String, TemplateError> {
        entries
            .iter()
            .map(|(path, reason)| ((*path).to_string(), reason.clone()))
            .collect()
    }

    fn waiting(field_path: &str) -> TemplateError {
        TemplateError::WaitingOn(field_path.to_string())
    }

    #[test]
    fn a_chain_of_waiting_fields_reports_only_its_head() {
        let causes = group_by_root_cause(
            &stuck(&["a", "b", "c"]),
            &reasons(&[
                ("a", TemplateError::missing("nowhere")),
                ("b", waiting("a")),
                ("c", waiting("b")),
            ]),
            false,
        );

        assert_eq!(causes.len(), 1);
        assert_eq!(causes[0].field_path, "a");
        assert_eq!(causes[0].reason, "it reads 'nowhere', which does not exist");
        assert_eq!(causes[0].origin.as_deref(), Some("main.yaml"));

        let dependents: Vec<(&str, Vec<&str>)> = causes[0]
            .dependents
            .iter()
            .map(|dependent| {
                (
                    dependent.path.as_str(),
                    dependent.through.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(dependents, vec![("b", vec![]), ("c", vec!["b"])]);
    }

    #[test]
    fn separate_causes_stay_separate() {
        let causes = group_by_root_cause(
            &stuck(&["a", "b", "x", "y"]),
            &reasons(&[
                ("a", TemplateError::missing("nowhere")),
                ("b", waiting("a")),
                ("x", TemplateError::Failed("a filter failed".to_string())),
                ("y", waiting("x")),
            ]),
            false,
        );

        assert_eq!(causes.len(), 2);
        assert_eq!(causes[0].field_path, "a");
        assert_eq!(causes[1].field_path, "x");
        assert_eq!(causes[1].dependents.len(), 1);
    }

    #[test]
    fn a_reference_loop_is_reported_once_from_its_lowest_member() {
        let causes = group_by_root_cause(
            &stuck(&["b", "c", "d"]),
            &reasons(&[("b", waiting("c")), ("c", waiting("b")), ("d", waiting("b"))]),
            false,
        );

        assert_eq!(causes.len(), 1);
        assert_eq!(causes[0].field_path, "b");
        assert_eq!(
            causes[0].reason,
            "it is part of a reference loop: b -> c -> b"
        );
        assert_eq!(causes[0].dependents.len(), 2);
    }

    #[test]
    fn waiting_on_a_field_that_resolved_stays_its_own_cause() {
        let causes = group_by_root_cause(&stuck(&["a"]), &reasons(&[("a", waiting("b"))]), false);

        assert_eq!(causes.len(), 1);
        assert_eq!(causes[0].field_path, "a");
        assert_eq!(causes[0].reason, "it reads 'b', which is itself unresolved");
    }

    #[test]
    fn a_field_with_no_recorded_reason_still_reports() {
        let causes = group_by_root_cause(&stuck(&["a"]), &BTreeMap::new(), true);

        assert_eq!(causes.len(), 1);
        assert_eq!(causes[0].reason, "the reference chain is too long to resolve");
    }
}
