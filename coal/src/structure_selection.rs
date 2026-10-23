//! `_structure.yaml`: selecting a configuration and saying how it is layered.
//!
//! A configuration usually describes many things — every environment, every
//! mode, every node — while a build is nearly always *for* one of them.
//! `_structure.yaml` says which parts of the tree name a selectable thing, and
//! which parts are layered on top of each other to produce it:
//!
//! ```yaml
//! selectors:
//!   - environments.{environment}.nodes.{node}
//!   - environments.{environment}.modes.{mode}
//! layers:
//!   - "node_types.{{ node.node_type }}"
//!   - "environments.{{ environment }}"
//!   - "{node}"
//!   - "{mode}"
//! drop: environments
//! ```
//!
//! `coal -s cop01 -s operational` then finds the one path matching each
//! selector, merges the layers onto the configuration root weakest first, and
//! drops `environments` — scaffolding that has done its job.
//!
//! # Selectors
//!
//! A selector is a path with `{captures}` in it. Its **last** segment is the
//! thing being selected and names the selector; every other capture is settled
//! by the match and can be read by a layer. A capture already settled — by an
//! earlier selector, or pinned with `-p` — constrains the ones after it, which
//! is what keeps a node in one environment from being built with a mode from
//! another.
//!
//! A selection is any right-aligned part of the selector's path, so `cop01`,
//! `nodes.cop01`, `production.nodes.cop01` and
//! `environments.production.nodes.cop01` all name the same node — the longer
//! forms exist for when the short one matches more than one place.
//!
//! # Layers
//!
//! Layers are one list, weakest first, shared by every selector. That shared
//! order is the whole point of allowing several selections at once: it is
//! where the configuration says whether a node's own settings beat the mode it
//! picked, or the other way round.
//!
//! An entry is either **`{name}`** — the node that selector matched — or a
//! Jinja template producing a path from the root. Inside a template, a capture
//! reads as its value (`{{ environment }}` is `production`) and a selector
//! reads as its match: `{{ node }}` is `cop01` and `{{ node.node_type }}` is
//! that node's `node_type`.
//!
//! A selector that nothing was selected for contributes nothing, and its
//! `{name}` layer is skipped. A selector that matched but is never named in
//! `layers` is added at the end, since the selected thing itself is usually
//! the most specific thing there is to say.
//!
//! A layer whose path names nothing is skipped rather than reported. Layering
//! is sparse by nature: most environments have nothing to say about most node
//! types, and having to declare an empty object for each would defeat the
//! point. A layer whose *first* name is not a field of the configuration at
//! all is different — there is no corner for it to be missing from, so it can
//! never contribute anything and is reported.

use crate::coal_tree::CoalTree;
use crate::jinja_engine::{TemplateError, TemplateScope, render_template_text};
use crate::reference_resolution::find_target_node_using_reference_path;
use crate::tree_operations::clone_tree_node_recursively;
use crate::type_aliases::{TreeNodeReference, new_tree_node_reference};
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::rc::Rc;

/// One segment of a selector pattern.
enum PathSegment {
    /// A key that has to be there exactly as written.
    Literal(String),
    /// A key that is read off the tree and bound to a name.
    Capture(String),
}

/// What survives into the compiled output beside the merged result.
enum OutputRule {
    /// Everything. The default: the structure says how to layer, not what to
    /// throw away.
    KeepEverything,
    /// Everything except these root paths.
    Drop(Vec<String>),
    /// Only these root fields, plus whatever the layers contributed.
    Keep(Vec<String>),
}

/// One entry of `selectors:`.
struct Selector {
    /// The last capture of the pattern, which names both the selector and the
    /// root field that says what to build.
    name: String,
    pattern: Vec<PathSegment>,
}

/// One entry of `layers:`.
enum Layer {
    /// `{node}` — whatever that selector matched.
    Match(String),
    /// A Jinja template producing a path from the configuration root.
    Path(String),
}

/// A parsed `_structure.yaml`.
pub struct StructureDocument {
    selectors: Vec<Selector>,
    layers: Vec<Layer>,
    output: OutputRule,
}

impl StructureDocument {
    pub fn is_empty(&self) -> bool {
        self.selectors.is_empty()
    }

    /// The names this structure declares, for a message that has to say what
    /// the configuration does offer.
    pub fn selector_names(&self) -> Vec<String> {
        self.selectors
            .iter()
            .map(|selector| selector.name.clone())
            .collect()
    }
}

/// One selection of every selector, which is one configuration the directory
/// can produce.
#[derive(Clone, Debug)]
pub struct SelectionCombination {
    /// Selector name and selected value, in the order the structure declares
    /// them.
    pub values: Vec<(String, String)>,
}

impl SelectionCombination {
    /// How the combination is named in a report.
    pub fn describe(&self) -> String {
        self.values
            .iter()
            .map(|(_, value)| value.clone())
            .collect::<Vec<String>>()
            .join(", ")
    }
}

/// A `-s` argument worked out against the structure: which selector it answers,
/// what it selects, and whatever the path form settled along the way.
pub struct ResolvedSelection {
    pub name: String,
    pub value: String,
    /// Captures the path form pinned, as (name, value).
    pub pins: Vec<(String, String)>,
}

// =====================================================================
// PARSING
// =====================================================================

/// Reads a `_structure.yaml` document, reporting anything it cannot act on
/// rather than quietly doing less than it says.
pub fn parse_structure_document(document: &Value) -> Result<StructureDocument, Box<dyn Error>> {
    let Value::Mapping(document_mapping) = document else {
        return Err("_structure.yaml must be a mapping with a 'selectors:' entry.".into());
    };

    if document_mapping.contains_key("select") {
        return Err("_structure.yaml has a 'select:' entry, which COAL no longer reads. Selectors \
                    are now one flat list, and 'layers:' and 'drop:'/'keep:' are written beside \
                    them rather than inside each entry:\n  selectors:\n    - \
                    environments.{environment}.nodes.{node}\n  layers:\n    - \"environments.{{ \
                    environment }}\"\n    - \"{node}\"\n  drop: environments"
            .into());
    }

    let Some(selector_patterns) = document_mapping.get("selectors") else {
        return Err("_structure.yaml has no 'selectors:' entry, so it says nothing.".into());
    };

    let selectors = parse_selectors(selector_patterns)?;
    let layers = parse_layers(document_mapping.get("layers"))?;

    let drop_paths = string_list(document_mapping.get("drop"), "drop")?;
    let keep_paths = string_list(document_mapping.get("keep"), "keep")?;

    let output = match (drop_paths, keep_paths) {
        (Some(_), Some(_)) => {
            return Err("_structure.yaml gives both 'drop:' and 'keep:'. Say which fields go, or \
                        which stay — not both."
                .into());
        }
        (Some(drop_paths), None) => OutputRule::Drop(drop_paths),
        (None, Some(keep_paths)) => OutputRule::Keep(keep_paths),
        (None, None) => OutputRule::KeepEverything,
    };

    Ok(StructureDocument {
        selectors,
        layers,
        output,
    })
}

fn parse_selectors(patterns: &Value) -> Result<Vec<Selector>, Box<dyn Error>> {
    let pattern_texts: Vec<String> = match patterns {
        Value::String(single) => vec![single.clone()],
        Value::Sequence(sequence) => sequence
            .iter()
            .map(|entry| match entry {
                Value::String(pattern) => Ok(pattern.clone()),
                _ => Err("every entry of 'selectors:' must be a path pattern.".to_string()),
            })
            .collect::<Result<Vec<String>, String>>()?,
        _ => return Err("'selectors:' must be a path pattern or a list of them.".into()),
    };

    if pattern_texts.is_empty() {
        return Err("'selectors:' is empty, so the structure says nothing.".into());
    }

    let mut selectors: Vec<Selector> = Vec::new();
    for pattern_text in pattern_texts {
        let pattern = parse_path_pattern(&pattern_text);

        // The last segment is the thing being selected, and names the
        // selector. Anything else would leave the pattern reading as a path to
        // something other than what it selects.
        let name = match pattern.last() {
            Some(PathSegment::Capture(name)) => name.clone(),
            _ => {
                return Err(format!(
                    "The selector '{}' does not end in the thing it selects. Write the path down \
                     to it and capture it last, as in \
                     environments.{{environment}}.nodes.{{node}}.",
                    pattern_text
                )
                .into());
            }
        };

        if selectors.iter().any(|selector| selector.name == name) {
            return Err(format!(
                "Two selectors both select {{{}}}. Each one has to end in a name of its own, \
                 since that name is how a selection says which it answers.",
                name
            )
            .into());
        }

        selectors.push(Selector { name, pattern });
    }
    Ok(selectors)
}

fn parse_layers(layers: Option<&Value>) -> Result<Vec<Layer>, Box<dyn Error>> {
    let Some(layers) = layers else {
        return Ok(Vec::new());
    };
    let Value::Sequence(layer_sequence) = layers else {
        return Err("'layers:' must be a list.".into());
    };

    layer_sequence.iter().map(parse_layer).collect()
}

fn parse_layer(layer: &Value) -> Result<Layer, Box<dyn Error>> {
    let layer_text = match layer {
        Value::String(text) => text.trim().to_string(),

        // `- {node}` is a flow mapping as far as YAML is concerned, and that is
        // how a person naturally writes it. A one-key mapping with nothing on
        // the right is that and nothing else, so it is read as written rather
        // than refused over a pair of quotes.
        Value::Mapping(mapping) => match mapping.iter().collect::<Vec<_>>().as_slice() {
            [(Value::String(only_key), Value::Null)] => format!("{{{}}}", only_key.trim()),
            _ => {
                return Err("every entry of 'layers:' must be a path or a selector like \
                            \"{node}\"."
                    .into());
            }
        },
        _ => {
            return Err(
                "every entry of 'layers:' must be a path or a selector like \"{node}\".".into(),
            );
        }
    };

    match layer_text
        .strip_prefix('{')
        .and_then(|inner| inner.strip_suffix('}'))
    {
        Some(selector_name) => Ok(Layer::Match(selector_name.trim().to_string())),
        None => Ok(Layer::Path(layer_text)),
    }
}

fn string_list(value: Option<&Value>, field_name: &str) -> Result<Option<Vec<String>>, Box<dyn Error>> {
    match value {
        None => Ok(None),
        Some(Value::String(single)) => Ok(Some(vec![single.clone()])),
        Some(Value::Sequence(sequence)) => {
            let mut paths = Vec::new();
            for entry in sequence {
                match entry {
                    Value::String(path) => paths.push(path.clone()),
                    _ => {
                        return Err(format!(
                            "every entry of '{}:' must be a field path.",
                            field_name
                        )
                        .into());
                    }
                }
            }
            Ok(Some(paths))
        }
        Some(_) => {
            Err(format!("'{}:' must be a field path or a list of them.", field_name).into())
        }
    }
}

/// Splits `environments.{environment}.nodes.{node}` into its segments.
fn parse_path_pattern(pattern: &str) -> Vec<PathSegment> {
    pattern
        .split('.')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            match segment
                .strip_prefix('{')
                .and_then(|inner| inner.strip_suffix('}'))
            {
                Some(capture_name) => PathSegment::Capture(capture_name.trim().to_string()),
                None => PathSegment::Literal(segment.to_string()),
            }
        })
        .collect()
}

// =====================================================================
// READING A SELECTION OFF THE COMMAND LINE
// =====================================================================

/// Works out which selector a `-s` argument answers and what it settles.
///
/// A selection is any right-aligned part of a selector's path. `cop01` names
/// the node when only one selector could mean it; `production.nodes.cop01`
/// says which one, and pins `{environment}` on the way.
///
/// "Could mean it" is decided against the configuration, not just against the
/// patterns: with a node selector and a mode selector, a bare name fits both
/// patterns, and what tells them apart is that one of them actually has
/// something of that name. `-s cop01 -s operational` therefore needs no
/// further help.
pub fn resolve_selection_argument(
    structure: &StructureDocument,
    name: Option<&str>,
    value: &str,
    root_node: &TreeNodeReference,
) -> Result<ResolvedSelection, Box<dyn Error>> {
    if structure.is_empty() {
        return Err(format!(
            "-s {} was given, but this configuration has no _structure.yaml saying what can be \
             selected.",
            value
        )
        .into());
    }

    let segments: Vec<&str> = value.split('.').filter(|part| !part.is_empty()).collect();
    let Some((&selected_value, _)) = segments.split_last() else {
        return Err("-s was given an empty selection.".into());
    };

    let mut candidates: Vec<&Selector> = structure
        .selectors
        .iter()
        .filter(|selector| name.is_none_or(|name| name == selector.name))
        .filter(|selector| pattern_takes_suffix(&selector.pattern, &segments))
        .collect();

    // Several selectors can fit the same shape of path. The configuration
    // settles it: only one of them is likely to hold something of this name.
    if candidates.len() > 1 {
        let with_a_match: Vec<&Selector> = candidates
            .iter()
            .copied()
            .filter(|selector| selector_holds_value(selector, selected_value, root_node))
            .collect();
        if with_a_match.len() == 1 {
            candidates = with_a_match;
        }
    }

    let selector = match candidates.as_slice() {
        [only] => *only,
        [] if name.is_some() => {
            return Err(format!(
                "-s {}={} does not fit the selector {{{}}}. This configuration declares {}.",
                name.unwrap_or_default(),
                value,
                name.unwrap_or_default(),
                describe_selectors(structure)
            )
            .into());
        }
        [] => {
            return Err(format!(
                "-s {} does not fit any selector. This configuration declares {}.",
                value,
                describe_selectors(structure)
            )
            .into());
        }
        _ => {
            return Err(format!(
                "-s {} could answer more than one selector: {}. Say which, with -s <name>={}, or \
                 give more of the path.",
                value,
                candidates
                    .iter()
                    .map(|selector| selector.name.clone())
                    .collect::<Vec<String>>()
                    .join(", "),
                value
            )
            .into());
        }
    };

    // Everything the path named before the last segment settles a capture, so
    // the long form both says which selector is meant and narrows it.
    let offset = selector.pattern.len() - segments.len();
    let mut pins = Vec::new();
    for (index, segment) in segments.iter().enumerate().take(segments.len() - 1) {
        if let PathSegment::Capture(capture_name) = &selector.pattern[offset + index] {
            pins.push((capture_name.clone(), (*segment).to_string()));
        }
    }

    Ok(ResolvedSelection {
        name: selector.name.clone(),
        value: selected_value.to_string(),
        pins,
    })
}

/// Whether this selector has anything of that name, which is what tells two
/// selectors of the same shape apart.
fn selector_holds_value(
    selector: &Selector,
    selected_value: &str,
    root_node: &TreeNodeReference,
) -> bool {
    let mut constraints = BTreeMap::new();
    constraints.insert(selector.name.clone(), selected_value.to_string());

    // Captures already settled — by an earlier `-s`, or pinned with `-p` —
    // narrow the search the same way they narrow the build.
    for segment in &selector.pattern {
        let PathSegment::Capture(capture_name) = segment else {
            continue;
        };
        if constraints.contains_key(capture_name) {
            continue;
        }
        if let Some(pinned) = selection_value(root_node, capture_name) {
            constraints.insert(capture_name.clone(), pinned);
        }
    }

    let mut matches = Vec::new();
    collect_pattern_matches(
        Rc::clone(root_node),
        &selector.pattern,
        &mut BTreeMap::new(),
        &constraints,
        &mut matches,
    );
    !matches.is_empty()
}

/// Whether the path segments line up with the end of the pattern.
fn pattern_takes_suffix(pattern: &[PathSegment], segments: &[&str]) -> bool {
    if segments.len() > pattern.len() {
        return false;
    }

    let offset = pattern.len() - segments.len();
    segments
        .iter()
        .enumerate()
        .all(|(index, segment)| match &pattern[offset + index] {
            PathSegment::Literal(literal) => literal == segment,
            PathSegment::Capture(_) => true,
        })
}

fn describe_selectors(structure: &StructureDocument) -> String {
    structure
        .selectors
        .iter()
        .map(|selector| format!("{} ({})", selector.name, describe_pattern(&selector.pattern)))
        .collect::<Vec<String>>()
        .join(", ")
}

// =====================================================================
// APPLYING
// =====================================================================

/// Applies the structure if a selection has been made and everything it needs
/// is known, reporting whether it did.
///
/// Returning `false` is not a failure: the selection may name a field that has
/// no value yet, or a layer may read one. The build comes back on the next
/// round, exactly as it does for an include whose path is not settled.
pub fn apply_structure_selection(
    structure: &StructureDocument,
    root_node: TreeNodeReference,
) -> Result<bool, Box<dyn Error>> {
    let mut matched: BTreeMap<String, TreeNodeReference> = BTreeMap::new();
    let mut captures: BTreeMap<String, String> = BTreeMap::new();
    let mut anything_selected = false;

    for selector in &structure.selectors {
        let Some(selected_value) = selection_value(&root_node, &selector.name) else {
            continue;
        };
        anything_selected = true;

        // Whatever an earlier selector settled narrows this one, so a mode is
        // looked for in the environment the node was found in rather than in
        // every environment there is.
        let mut constraints = captures.clone();
        constraints.insert(selector.name.clone(), selected_value.clone());
        for segment in &selector.pattern {
            let PathSegment::Capture(capture_name) = segment else {
                continue;
            };
            if constraints.contains_key(capture_name) {
                continue;
            }
            if let Some(pinned) = selection_value(&root_node, capture_name) {
                constraints.insert(capture_name.clone(), pinned);
            }
        }

        let (selected_node, selector_captures) =
            find_the_one_match(selector, &selected_value, &constraints, &root_node)?;

        captures.extend(selector_captures);
        matched.insert(selector.name.clone(), selected_node);
    }

    if !anything_selected {
        return Ok(false);
    }

    apply_layers(structure, &matched, &captures, root_node)
}

/// Finds the single place a selector matches, or says why it cannot.
fn find_the_one_match(
    selector: &Selector,
    selected_value: &str,
    constraints: &BTreeMap<String, String>,
    root_node: &TreeNodeReference,
) -> Result<(TreeNodeReference, BTreeMap<String, String>), Box<dyn Error>> {
    let mut matches = Vec::new();
    collect_pattern_matches(
        Rc::clone(root_node),
        &selector.pattern,
        &mut BTreeMap::new(),
        constraints,
        &mut matches,
    );

    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(format!(
            "Nothing matches '{}' with {} = '{}'.",
            describe_pattern(&selector.pattern),
            selector.name,
            selected_value
        )
        .into()),
        _ => {
            let described: Vec<String> = matches
                .iter()
                .map(|(node, _)| node.borrow().get_path())
                .collect();
            Err(format!(
                "'{}' matches more than one place with {} = '{}':\n  {}\nGive more of the path to \
                 say which — for example -s {}.",
                describe_pattern(&selector.pattern),
                selector.name,
                selected_value,
                described.join("\n  "),
                described
                    .first()
                    .cloned()
                    .unwrap_or_else(|| selected_value.to_string())
            )
            .into())
        }
    }
}

/// Merges the layers onto the configuration root, weakest first.
fn apply_layers(
    structure: &StructureDocument,
    matched: &BTreeMap<String, TreeNodeReference>,
    captures: &BTreeMap<String, String>,
    root_node: TreeNodeReference,
) -> Result<bool, Box<dyn Error>> {
    // A layer template reads bare names off the first selection made, so a
    // structure with one selector reads exactly as it always has.
    let scope_node = structure
        .selectors
        .iter()
        .find_map(|selector| matched.get(&selector.name))
        .map(Rc::clone)
        .unwrap_or_else(|| Rc::clone(&root_node));

    let mut layer_nodes = Vec::new();
    let mut placed: BTreeSet<String> = BTreeSet::new();

    for layer in &structure.layers {
        match layer {
            Layer::Match(selector_name) => {
                if !structure
                    .selectors
                    .iter()
                    .any(|selector| &selector.name == selector_name)
                {
                    return Err(format!(
                        "The layer '{{{}}}' names a selector this structure does not declare. It \
                         declares {}.",
                        selector_name,
                        describe_selectors(structure)
                    )
                    .into());
                }

                // A selector nothing was selected for has nothing to place.
                if let Some(selected_node) = matched.get(selector_name) {
                    layer_nodes.push(Rc::clone(selected_node));
                }
                placed.insert(selector_name.clone());
            }

            Layer::Path(layer_template) => {
                match resolve_layer_path(layer_template, &scope_node, &root_node, captures, matched)
                {
                    // A layer that reads a field with no value yet: come back
                    // later, having changed nothing.
                    Err(TemplateError::WaitingOn(_)) => return Ok(false),
                    Err(TemplateError::Failed(reason)) => {
                        return Err(format!(
                            "The layer '{}' could not be worked out -> {}",
                            layer_template, reason
                        )
                        .into());
                    }
                    Ok(layer_path) => add_layer_at_path(
                        &layer_path,
                        layer_template,
                        &scope_node,
                        &root_node,
                        &mut layer_nodes,
                    )?,
                }
            }
        }
    }

    // A selection the layers never mention goes last: the thing that was asked
    // for is usually the most specific thing there is to say.
    for selector in &structure.selectors {
        if placed.contains(&selector.name) {
            continue;
        }
        if let Some(selected_node) = matched.get(&selector.name) {
            layer_nodes.push(Rc::clone(selected_node));
        }
    }

    // Every layer is copied out before anything is written, so a layer that
    // sits inside a part of the tree an earlier layer overwrites still
    // contributes what it said when the merge began.
    let detached_layers: Vec<TreeNodeReference> = layer_nodes
        .iter()
        .map(|layer_node| clone_tree_node_recursively(Rc::clone(layer_node), Rc::clone(&root_node)))
        .collect();

    let mut merged_keys = BTreeSet::new();
    for layer in detached_layers {
        overlay_children(&layer, &root_node, &mut merged_keys);
    }

    apply_output_rule(&structure.output, &root_node, &merged_keys);
    Ok(true)
}

/// Adds the node a resolved layer path reaches, or decides whether its absence
/// is ordinary sparseness or a mistake.
fn add_layer_at_path(
    layer_path: &str,
    layer_template: &str,
    scope_node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    layer_nodes: &mut Vec<TreeNodeReference>,
) -> Result<(), Box<dyn Error>> {
    if let Some(layer_node) = find_target_node_using_reference_path(
        layer_path,
        Rc::clone(scope_node),
        Rc::clone(root_node),
    ) {
        layer_nodes.push(layer_node);
        return Ok(());
    }

    // Layering is sparse: a layer naming a corner of the configuration that is
    // simply not there has nothing to contribute, and most environments say
    // nothing about most node types. A layer whose *first* name is not a field
    // of the configuration at all is a different thing — there is no corner for
    // it to be missing from, so it can never contribute anything, however the
    // build is run. That is a mistake in the structure, and saying so beats
    // silently building a configuration the layer was supposed to complete.
    let Some(missing_field) = layer_root_the_configuration_lacks(root_node, layer_path) else {
        return Ok(());
    };

    Err(format!(
        "The layer '{}' reads '{}', which is not a field of the configuration, so it can never \
         contribute anything.{}",
        layer_template,
        missing_field,
        describe_near_miss(root_node, &missing_field)
    )
    .into())
}

/// The first name of a layer path when the configuration has no field of that
/// name, which is what tells a mistake apart from ordinary sparseness.
///
/// A path written relative to the selected node is left alone: it is not
/// reaching for a root field, so a root field is not what it could be missing.
fn layer_root_the_configuration_lacks(
    root_node: &TreeNodeReference,
    layer_path: &str,
) -> Option<String> {
    if layer_path.trim_start().starts_with("parent") {
        return None;
    }

    let first_segment = layer_path
        .split('.')
        .find(|segment| !segment.trim().is_empty())?
        .trim();

    let root_has_it = root_node
        .borrow()
        .children
        .iter()
        .any(|child| child.borrow().key.as_deref() == Some(first_segment));

    match root_has_it {
        true => None,
        false => Some(first_segment.to_string()),
    }
}

/// Names the field the layer most likely meant, when one of them is nearly
/// what was written. `environment` for `environments` is the mistake this
/// exists for, and it is the one a reader stares straight past.
fn describe_near_miss(root_node: &TreeNodeReference, missing_field: &str) -> String {
    let candidates: Vec<String> = root_node
        .borrow()
        .children
        .iter()
        .filter_map(|child| child.borrow().key.clone())
        .filter(|key| !key.starts_with('$'))
        .filter(|key| key.starts_with(missing_field) || missing_field.starts_with(key.as_str()))
        .collect();

    match candidates.first() {
        Some(candidate) => format!(" Did you mean '{}'?", candidate),
        None => String::new(),
    }
}

/// Every configuration this structure can produce.
///
/// This is what `--validate` checks over: a configuration describing many
/// things has to be right for all of them, and a mistake in one node's
/// environment says nothing about another's. With several selectors that means
/// every combination of them — but only the combinations that can actually be
/// built, so a node in one environment is never paired with a mode from
/// another.
pub fn enumerate_selection_combinations(
    structure: &StructureDocument,
    root_node: &TreeNodeReference,
) -> Vec<SelectionCombination> {
    let mut combinations: Vec<PartialCombination> = vec![PartialCombination::default()];

    for selector in &structure.selectors {
        // Captures other than the selector's own are honoured if the
        // configuration pinned them, so `-p environment=production` narrows
        // what is checked to that environment rather than every one of them.
        let mut pinned = BTreeMap::new();
        for segment in &selector.pattern {
            let PathSegment::Capture(capture_name) = segment else {
                continue;
            };
            if capture_name == &selector.name {
                continue;
            }
            if let Some(value) = selection_value(root_node, capture_name) {
                pinned.insert(capture_name.clone(), value);
            }
        }

        let mut matches = Vec::new();
        collect_pattern_matches(
            Rc::clone(root_node),
            &selector.pattern,
            &mut BTreeMap::new(),
            &pinned,
            &mut matches,
        );

        // The same name in two environments is two configurations, not one, so
        // each is named by as much of its path as it takes to tell them apart.
        let mut occurrences: BTreeMap<&String, usize> = BTreeMap::new();
        for (_, match_captures) in &matches {
            if let Some(value) = match_captures.get(&selector.name) {
                *occurrences.entry(value).or_insert(0) += 1;
            }
        }

        let mut extended = Vec::new();
        for PartialCombination { values, settled } in &combinations {
            let mut seen = BTreeSet::new();

            for (_, match_captures) in &matches {
                let Some(value) = match_captures.get(&selector.name) else {
                    continue;
                };

                // A selection has to agree with what the combination has
                // already settled, which is what keeps a node in one
                // environment from being paired with a mode from another.
                let disagrees = match_captures.iter().any(|(capture_name, capture_value)| {
                    settled
                        .get(capture_name)
                        .is_some_and(|already| already != capture_value)
                });
                if disagrees {
                    continue;
                }

                let selection_text = match occurrences.get(value) {
                    Some(&count) if count > 1 => {
                        describe_match_path(&selector.pattern, match_captures)
                    }
                    _ => value.clone(),
                };
                if !seen.insert(selection_text.clone()) {
                    continue;
                }

                let mut extended_values = values.clone();
                extended_values.push((selector.name.clone(), selection_text));

                let mut extended_settled = settled.clone();
                extended_settled.extend(match_captures.clone());

                extended.push(PartialCombination {
                    values: extended_values,
                    settled: extended_settled,
                });
            }
        }

        // A selector nothing matches leaves the combinations as they were,
        // rather than wiping out every configuration there was to check.
        if !extended.is_empty() {
            combinations = extended;
        }
    }

    combinations
        .into_iter()
        .filter(|combination| !combination.values.is_empty())
        .map(|combination| SelectionCombination {
            values: combination.values,
        })
        .collect()
}

/// A combination being built up one selector at a time, together with every
/// capture the selections so far have settled.
#[derive(Default)]
struct PartialCombination {
    values: Vec<(String, String)>,
    settled: BTreeMap<String, String>,
}

/// The value naming what to build, if the configuration has settled on one.
fn selection_value(root_node: &TreeNodeReference, selector_name: &str) -> Option<String> {
    let field = root_node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key.as_deref() == Some(selector_name))
        .cloned()?;

    let field_borrowed = field.borrow();
    match field_borrowed.value.as_ref()? {
        // Still a template: the build comes back once it has resolved.
        Value::String(text) if text.contains("{{") || text.contains("{%") => None,
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(boolean) => Some(boolean.to_string()),
        _ => None,
    }
}

/// The concrete path one match sits at, which is how a configuration is named
/// when its short name would name more than one.
fn describe_match_path(
    pattern: &[PathSegment],
    captures: &BTreeMap<String, String>,
) -> String {
    pattern
        .iter()
        .map(|segment| match segment {
            PathSegment::Literal(name) => name.clone(),
            PathSegment::Capture(name) => captures.get(name).cloned().unwrap_or_default(),
        })
        .collect::<Vec<String>>()
        .join(".")
}

fn describe_pattern(pattern: &[PathSegment]) -> String {
    pattern
        .iter()
        .map(|segment| match segment {
            PathSegment::Literal(name) => name.clone(),
            PathSegment::Capture(name) => format!("{{{}}}", name),
        })
        .collect::<Vec<String>>()
        .join(".")
}

/// Walks the tree collecting every place the pattern matches, together with
/// what each capture bound to there.
fn collect_pattern_matches(
    node: TreeNodeReference,
    remaining_pattern: &[PathSegment],
    bindings: &mut BTreeMap<String, String>,
    constraints: &BTreeMap<String, String>,
    matches: &mut Vec<(TreeNodeReference, BTreeMap<String, String>)>,
) {
    let Some((segment, rest)) = remaining_pattern.split_first() else {
        matches.push((node, bindings.clone()));
        return;
    };

    match segment {
        PathSegment::Literal(name) => {
            let child = node
                .borrow()
                .children
                .iter()
                .find(|child| child.borrow().key.as_deref() == Some(name.as_str()))
                .cloned();
            if let Some(child) = child {
                collect_pattern_matches(child, rest, bindings, constraints, matches);
            }
        }
        PathSegment::Capture(capture_name) => {
            let children = node.borrow().children.clone();
            for child in children {
                let child_key = child.borrow().key.clone().unwrap_or_default();
                if child_key.starts_with('$') {
                    continue;
                }
                if let Some(required) = constraints.get(capture_name)
                    && required != &child_key
                {
                    continue;
                }

                bindings.insert(capture_name.clone(), child_key);
                collect_pattern_matches(child, rest, bindings, constraints, matches);
                bindings.remove(capture_name);
            }
        }
    }
}

/// Renders one layer template, with the captures in scope as names of their own
/// and each selector's match in scope under the selector's name.
fn resolve_layer_path(
    layer_template: &str,
    scope_node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    captures: &BTreeMap<String, String>,
    matched: &BTreeMap<String, TreeNodeReference>,
) -> Result<String, TemplateError> {
    // A selector's own name is bound to the node it matched, not to the text
    // of the capture: the node prints as that text anyway, and this is what
    // lets `{{ node.node_type }}` read a field off it.
    let bindings: BTreeMap<String, Value> = captures
        .iter()
        .filter(|(name, _)| !matched.contains_key(name.as_str()))
        .map(|(name, value)| (name.clone(), Value::String(value.clone())))
        .collect();

    let scope = TemplateScope::for_node(scope_node, root_node)
        .with_bindings(bindings)
        .with_node_bindings(matched.clone());
    render_template_text(layer_template, &scope)
}

// =====================================================================
// MERGING
// =====================================================================

/// Merges a layer's fields onto the configuration root, recording which root
/// fields it touched.
fn overlay_children(
    source_node: &TreeNodeReference,
    target_node: &TreeNodeReference,
    merged_keys: &mut BTreeSet<String>,
) {
    let source_children = source_node.borrow().children.clone();

    for source_child in source_children {
        let child_key = source_child.borrow().key.clone().unwrap_or_default();

        // A directive comes along with the object it was written on. Most of
        // them are only evaluated once the merge has happened — that is the
        // point of them, since what an `$inherit` reads is not settled until
        // the configuration has been assembled — so dropping them here would
        // quietly lose the line. It is not a field of the output, though, so
        // it never counts as one a layer contributed.
        if !child_key.starts_with('$') {
            merged_keys.insert(child_key.clone());
        }
        overlay_node(&source_child, target_node);
    }
}

/// Writes one field of a layer onto the field of the same name.
///
/// Objects are merged field by field, so a layer only has to say what it
/// changes. Everything else replaces what was there — a list included, which
/// is why a layer that gives a list gives the whole list.
fn overlay_node(source_node: &TreeNodeReference, target_parent: &TreeNodeReference) {
    let child_key = source_node.borrow().key.clone();

    let existing_child = target_parent
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key == child_key)
        .cloned();

    let Some(existing_child) = existing_child else {
        let cloned = clone_tree_node_recursively(Rc::clone(source_node), Rc::clone(target_parent));
        target_parent.borrow_mut().children.push(cloned);
        return;
    };

    let source_is_object = {
        let source_borrowed = source_node.borrow();
        !source_borrowed.children.is_empty() && !source_borrowed.is_list_type()
    };
    let target_is_object = {
        let target_borrowed = existing_child.borrow();
        !target_borrowed.children.is_empty() && !target_borrowed.is_list_type()
    };

    if source_is_object && target_is_object {
        let mut ignored = BTreeSet::new();
        overlay_children(source_node, &existing_child, &mut ignored);
        return;
    }

    replace_node_contents(source_node, &existing_child);
}

fn replace_node_contents(source_node: &TreeNodeReference, target_node: &TreeNodeReference) {
    let replacement = clone_tree_node_recursively(
        Rc::clone(source_node),
        target_node
            .borrow()
            .parent
            .as_ref()
            .and_then(|weak_parent| weak_parent.upgrade())
            .unwrap_or_else(|| Rc::clone(target_node)),
    );

    let replacement_borrowed = replacement.borrow();
    let mut target_borrowed = target_node.borrow_mut();

    target_borrowed.value = replacement_borrowed.value.clone();
    target_borrowed.origin = replacement_borrowed.origin.clone();
    target_borrowed.span = replacement_borrowed.span.clone();
    target_borrowed.template_resolved = replacement_borrowed.template_resolved;
    target_borrowed.file_root = replacement_borrowed.file_root;
    target_borrowed.children = replacement_borrowed.children.clone();

    drop(target_borrowed);
    let adopted_children = target_node.borrow().children.clone();
    for child in adopted_children {
        child.borrow_mut().parent = Some(Rc::downgrade(target_node));
    }
}

// =====================================================================
// OUTPUT
// =====================================================================

/// Prunes the scaffolding once it has done its job. Whatever the layers
/// contributed always stays: it is the thing that was asked for.
fn apply_output_rule(
    output: &OutputRule,
    root_node: &TreeNodeReference,
    merged_keys: &BTreeSet<String>,
) {
    match output {
        OutputRule::KeepEverything => {}
        OutputRule::Drop(paths) => {
            for path in paths {
                remove_path(root_node, path);
            }
        }
        OutputRule::Keep(paths) => {
            let kept: BTreeSet<&str> = paths
                .iter()
                .map(|path| path.split('.').next().unwrap_or(path))
                .collect();

            root_node.borrow_mut().children.retain(|child| {
                let child_key = child.borrow().key.clone().unwrap_or_default();
                child_key.starts_with('$')
                    || kept.contains(child_key.as_str())
                    || merged_keys.contains(&child_key)
            });
        }
    }
}

fn remove_path(root_node: &TreeNodeReference, path: &str) {
    let segments: Vec<&str> = path.split('.').filter(|part| !part.is_empty()).collect();
    let Some((last_segment, parent_segments)) = segments.split_last() else {
        return;
    };

    let mut current_node = Rc::clone(root_node);
    for segment in parent_segments {
        let child = current_node
            .borrow()
            .children
            .iter()
            .find(|child| child.borrow().key.as_deref() == Some(*segment))
            .cloned();
        match child {
            Some(child) => current_node = child,
            None => return,
        }
    }

    current_node
        .borrow_mut()
        .children
        .retain(|child| child.borrow().key.as_deref() != Some(*last_segment));
}

/// A placeholder root used when a build has no structure at all, so the rest
/// of the pipeline does not have to care whether one was declared.
pub fn empty_structure_document() -> StructureDocument {
    StructureDocument {
        selectors: Vec::new(),
        layers: Vec::new(),
        output: OutputRule::KeepEverything,
    }
}

/// Seeds a selection into the tree, so `-s node=cop01` is exactly a value the
/// configuration could have given itself.
pub fn seed_selection_value(root_node: &TreeNodeReference, name: &str, value: &str) {
    let existing = root_node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key.as_deref() == Some(name))
        .cloned();

    let field = existing.unwrap_or_else(|| {
        let new_field = new_tree_node_reference(CoalTree {
            key: Some(name.to_string()),
            parent: Some(Rc::downgrade(root_node)),
            origin: Some("<command-line>".to_string()),
            ..Default::default()
        });
        root_node.borrow_mut().children.push(Rc::clone(&new_field));
        new_field
    });

    let mut field_borrowed = field.borrow_mut();
    field_borrowed.value = Some(Value::String(value.to_string()));
    field_borrowed.children.clear();
    field_borrowed.origin = Some("<command-line>".to_string());
    field_borrowed.template_resolved = true;
}
