use crate::coal_tree::Span;
use crate::definition_scopes::DefinitionScopes;
use crate::diagnostics::{DiagnosticError, generate_diagnostics_for_entire_tree};
use crate::directory_loader::{is_each_template, load_configuration_directory};
use crate::identifier_directives::{IdentifierCounters, resolve_identifier_directives_depth_first};
use crate::include_directives::{
    describe_unresolved_include, process_include_directives_depth_first,
};
use crate::jinja_engine::{ResolutionStage, freeze_tree_for_reading};
use crate::parameter_overrides::{ParameterOverride, apply_parameter_overrides};
use crate::reference_resolution::{
    UnresolvedTemplate, resolve_all_jinja_values_in_tree, resolve_resolvable_jinja_values,
};
use crate::schema_application::{
    SchemaBlock, SchemaEnvironment, apply_schema_definition_to_tree_node,
    load_schema_block_from_directory,
};
use crate::structural_directives::{
    apply_each_templates_depth_first, apply_ignore_lists_depth_first,
    describe_unresolved_directive, process_structural_directives_depth_first,
};
use crate::structure_selection::{
    SelectionCombination, StructureDocument, apply_structure_selection, empty_structure_document,
    enumerate_selection_combinations, parse_structure_document, resolve_selection_argument,
    seed_selection_value,
};
use crate::tree_finalization::{
    remove_system_directives_from_tree, sort_tree_alphabetically,
    synchronize_node_values_depth_first,
};
use crate::tree_operations::clone_tree_root;
use crate::type_aliases::{TreeNodeReference, VirtualFileSystemReference};


use std::error::Error;
use std::rc::Rc;
use std::sync::Arc;

/// How many of the configurations a structure describes a build is for.
///
/// A directory with a `_structure.yaml` is not one configuration but many, and
/// what a caller wants out of it differs: printing wants one, validating wants
/// all of them, and an editor wants both at different moments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildScope {
    /// Exactly what the selections say. With none given and a structure
    /// declared, nothing is selected and the tree is compiled as written.
    AsSelected,
    /// The first configuration the structure describes, when nothing was
    /// selected. This is one real configuration rather than the tree as
    /// written, which is what makes it worth printing.
    FirstCombination,
    /// Every configuration the structure describes. A schema describes what
    /// comes out of a selection, so checking one says nothing about the rest.
    EveryCombination,
}

/// What to build, as asked for on the command line.
///
/// The name is the selector being answered. It may be left out whenever the
/// value settles it on its own — because only one selector could mean it, or
/// because only one of them holds anything of that name — which is what lets
/// `-s cop01` stand for `-s node=cop01`.
#[derive(Debug, Clone)]
pub struct SelectionArgument {
    pub name: Option<String>,
    pub value: String,
}

/// One compiled configuration, and what was found wrong with it.
pub struct CompiledConfiguration {
    /// What was selected to produce it, when a structure named something.
    /// `None` when the configuration was compiled as written.
    ///
    /// The selections are kept as the selector/value pairs they are rather
    /// than as one line of text, because a report has to be able to say that a
    /// finding follows one of them — "whenever mode = test" — and that is a
    /// question about the pairs.
    pub selection: Option<SelectionCombination>,
    pub root_node: TreeNodeReference,
    pub diagnostics: Vec<DiagnosticError>,
}

impl CompiledConfiguration {
    /// How the configuration is named in a report.
    pub fn describe_selection(&self) -> String {
        match &self.selection {
            Some(combination) => combination.describe(),
            None => "the configuration".to_string(),
        }
    }
}

/// Everything a build needs that does not change from one selection to the
/// next: the directory as it was read, the include namespace, the structure
/// rules, the schema, and the tree as far as it can be grown before a
/// selection is known.
///
/// Reading and parsing the directory is expensive and says nothing about what
/// is being built, so it is done once. So is everything the build can do
/// before the structure merges: a file's own includes, its `_each` templates,
/// and every reference that stays inside it. None of that can see a selection
/// — that is exactly what `ResolutionStage::BeforeStructureMerge` means — so
/// doing it once and copying the result gives every selection the same tree it
/// would have built for itself.
///
/// What cannot be shared is everything from the merge onwards. The structure
/// merges *before* the templates that read across files are rendered, so that
/// a field reads the value its layers settled on; a tree resolved any further
/// would have frozen one selection's answers into all of them.
struct SharedBuild<'a> {
    loaded_root: TreeNodeReference,
    definition_scopes: DefinitionScopes,
    structure: StructureDocument,
    schema_block: Option<SchemaBlock>,
    /// The pins, kept for the builds that do not start from a shared tree.
    /// Where there is one they have already been applied to it.
    parameter_overrides: &'a [ParameterOverride],
    /// The tree grown as far as it goes without a selection, ready to be
    /// copied. Absent when there is no structure, since then there is no merge
    /// to stop before and a single build does the whole thing.
    before_merge: Option<TreeBeforeMerge>,
}

/// The shared tree, and the identifier counters as they stood when it settled.
///
/// The counters travel with the tree because `$id` hands out numbers as the
/// tree grows: a selection that carried on from the shared tree with a fresh
/// counter map would start numbering again and hand out numbers already given.
struct TreeBeforeMerge {
    root_node: TreeNodeReference,
    identifier_counters: IdentifierCounters,
}

/// Compiles the configuration directory, once or once per selection.
///
/// What comes out is decided by the scope. `AsSelected` builds the one
/// configuration the selections name, or the tree as written when they name
/// nothing. `FirstCombination` builds one real configuration where the
/// selections left it open, which is what makes it worth printing.
/// `EveryCombination` builds them all, which is what validating a
/// configuration that describes many things means: a schema describes what
/// comes *out* of a selection, so a mistake in one node's environment says
/// nothing about another's.
///
/// A directory with no `_structure.yaml` describes exactly one configuration,
/// so every scope means the same thing there and one is built.
pub fn compile_configurations(
    configuration_directory: &str,
    virtual_file_system: VirtualFileSystemReference,
    scope: BuildScope,
    validate: bool,
    parameter_overrides: &[ParameterOverride],
    selections: &[SelectionArgument],
) -> Result<Vec<CompiledConfiguration>, Box<dyn Error>> {
    let shared = load_shared_build(
        configuration_directory,
        virtual_file_system,
        validate,
        parameter_overrides,
    )?;

    let structure_decides = !shared.structure.is_empty() && selections.is_empty();

    let available = match (scope, structure_decides) {
        (BuildScope::EveryCombination, true) => available_combinations(&shared, None)?,
        (BuildScope::FirstCombination, true) => available_combinations(&shared, Some(1))?,
        _ => AvailableCombinations::List(Vec::new()),
    };

    let combinations = match available {
        AvailableCombinations::List(combinations) => combinations,
        // The configuration answered every question the structure asks, so
        // the build that found that out already is the configuration. Without
        // a schema to check it against there is nothing left to do.
        AvailableCombinations::ChosenByConfiguration(build) if !validate => {
            return Ok(vec![CompiledConfiguration {
                selection: None,
                root_node: build.root_node,
                diagnostics: build.diagnostics,
            }]);
        }
        AvailableCombinations::ChosenByConfiguration(_) => Vec::new(),
    };

    if combinations.is_empty() {
        let (root_node, diagnostics) = run_single_build(&shared, selections, validate)?;
        return Ok(vec![CompiledConfiguration {
            selection: None,
            root_node,
            diagnostics,
        }]);
    }

    Ok(combinations
        .into_iter()
        .map(|combination| compile_one_combination(&shared, combination, validate))
        .collect())
}

/// What a structure leaves to choose when nothing was selected.
enum AvailableCombinations {
    /// The configurations it describes, each one a choice still to be made.
    List(Vec<SelectionCombination>),
    /// None: the configuration settled the selection itself (`node.yaml`
    /// holding `cop01`, say), and here is the build that merged it.
    ChosenByConfiguration(SingleBuild),
}

/// The configurations the structure describes, at most `limit` of them.
fn available_combinations(
    shared: &SharedBuild,
    limit: Option<usize>,
) -> Result<AvailableCombinations, Box<dyn Error>> {
    // What can be selected is worked out from the tree as written, once the
    // includes and templates that might have created a selectable thing have
    // run. Only the list of names survives that build.
    let unselected = run_build(shared, &[], false)?;

    // If the structure merged anyway, the configuration made the selection
    // itself. That build is then the configuration rather than the tree as
    // written: its layers are applied and whatever `drop` or `keep` removes —
    // the very paths the selectable things sit at — is gone. Enumerating it
    // would find nothing, and there is nothing left to choose.
    if unselected.structure_merged {
        return Ok(AvailableCombinations::ChosenByConfiguration(unselected));
    }

    let mut available = enumerate_selection_combinations(&shared.structure, &unselected.root_node);

    if available.is_empty() {
        return Err(format!(
            "The structure declares {} but nothing matches, so there is no configuration to \
             build.",
            shared.structure.selector_names().join(", ")
        )
        .into());
    }

    if let Some(limit) = limit {
        available.truncate(limit);
    }
    Ok(AvailableCombinations::List(available))
}

/// Compiles the one configuration a combination names.
///
/// A combination that cannot be built at all — an ambiguous match, say — is a
/// finding about that combination rather than the end of the run, since the
/// others still have to be checked.
fn compile_one_combination(
    shared: &SharedBuild,
    combination: SelectionCombination,
    validate: bool,
) -> CompiledConfiguration {
    let selection: Vec<SelectionArgument> = combination
        .values
        .iter()
        .map(|(name, value)| SelectionArgument {
            name: Some(name.clone()),
            value: value.clone(),
        })
        .collect();

    match run_single_build(shared, &selection, validate) {
        Ok((root_node, diagnostics)) => CompiledConfiguration {
            selection: Some(combination),
            root_node,
            diagnostics,
        },
        Err(error) => CompiledConfiguration {
            selection: Some(combination),
            root_node: clone_tree_root(Rc::clone(&shared.loaded_root)),
            diagnostics: vec![DiagnosticError {
                error_type: "BuildError".to_string(),
                field_path: String::new(),
                file_origin: "_structure.yaml".to_string(),
                message: error.to_string(),
                detail: error.to_string(),
                span: Some(Span { line: 1, col: 1 }),
            }],
        },
    }
}

/// Reads everything a build needs that a selection cannot change, and grows
/// the tree as far as it goes without one.
fn load_shared_build<'a>(
    configuration_directory: &str,
    virtual_file_system: VirtualFileSystemReference,
    validate: bool,
    parameter_overrides: &'a [ParameterOverride],
) -> Result<SharedBuild<'a>, Box<dyn Error>> {
    let loaded =
        load_configuration_directory(configuration_directory, Arc::clone(&virtual_file_system))?;

    let structure = match &loaded.structure_document {
        Some(document) => parse_structure_document(document)?,
        None => empty_structure_document(),
    };

    // Schema loading problems (unreadable directory, invalid YAML) must fail
    // the build instead of silently validating against nothing.
    let schema_block = match (&loaded.schema_directory, validate) {
        (Some(schema_directory), true) => {
            load_schema_block_from_directory(schema_directory, virtual_file_system)?
        }
        _ => None,
    };

    // Without a structure there is no merge to stop before: a single build
    // does the whole thing, and there is nothing worth sharing.
    let before_merge = match structure.is_empty() {
        true => None,
        false => Some(grow_tree_before_merge(
            &loaded.root_node,
            &loaded.definition_scopes,
            parameter_overrides,
        )?),
    };

    Ok(SharedBuild {
        loaded_root: loaded.root_node,
        definition_scopes: loaded.definition_scopes,
        structure,
        schema_block,
        parameter_overrides,
        before_merge,
    })
}

/// Grows a copy of the loaded tree as far as it goes with nothing selected.
///
/// Every step here is one no selection can change: the parameter overrides,
/// which are the same for the whole run, and then the expansion rounds held to
/// `BeforeStructureMerge`, where a file may resolve what it says about itself
/// but nothing may read across files. Whatever a selection would have done to
/// this tree, it would have done the same thing, so it is done once.
fn grow_tree_before_merge(
    loaded_root: &TreeNodeReference,
    definition_scopes: &DefinitionScopes,
    parameter_overrides: &[ParameterOverride],
) -> Result<TreeBeforeMerge, Box<dyn Error>> {
    let root_node = clone_tree_root(Rc::clone(loaded_root));
    apply_parameter_overrides(Rc::clone(&root_node), parameter_overrides);

    let mut identifier_counters = IdentifierCounters::new();
    // Held before the merge, so the structure cannot have merged here.
    expand_tree(
        Rc::clone(&root_node),
        definition_scopes,
        None,
        &mut identifier_counters,
    )?;

    Ok(TreeBeforeMerge {
        root_node,
        identifier_counters,
    })
}

/// One run of the pipeline.
struct SingleBuild {
    root_node: TreeNodeReference,
    diagnostics: Vec<DiagnosticError>,
    /// Whether the structure merged: something — a `-s`, a pin, or the
    /// configuration's own root field — said what to build.
    structure_merged: bool,
}

/// Runs the whole pipeline once, for one selection.
fn run_single_build(
    shared: &SharedBuild,
    selections: &[SelectionArgument],
    validate: bool,
) -> Result<(TreeNodeReference, Vec<DiagnosticError>), Box<dyn Error>> {
    let build = run_build(shared, selections, validate)?;
    Ok((build.root_node, build.diagnostics))
}

/// Runs the whole pipeline once, for one selection, saying whether the
/// structure merged.
///
/// Where a structure was declared this carries on from the shared tree rather
/// than starting over: everything up to the merge has already been done, and
/// the rounds below simply find nothing left to do before the structure
/// merges. Where there is none, the whole pipeline runs here.
fn run_build(
    shared: &SharedBuild,
    selections: &[SelectionArgument],
    validate: bool,
) -> Result<SingleBuild, Box<dyn Error>> {
    // A single counter map is shared between "$id" and "$id_after", so a
    // counter name keeps incrementing regardless of which variant uses it. It
    // is carried over from the shared tree so a number is never handed out
    // twice.
    let (root_node, mut identifier_counters) = match &shared.before_merge {
        Some(before_merge) => (
            clone_tree_root(Rc::clone(&before_merge.root_node)),
            before_merge.identifier_counters.clone(),
        ),
        None => {
            // No shared tree means no structure, and so nowhere the pins could
            // already have been applied.
            let root_node = clone_tree_root(Rc::clone(&shared.loaded_root));
            apply_parameter_overrides(Rc::clone(&root_node), shared.parameter_overrides);
            (root_node, IdentifierCounters::new())
        }
    };

    // Phase 0: Seed command-line values before anything else so they can be
    // read (via Jinja) by include paths and structure layers while the tree is
    // still being assembled.
    seed_selections(&root_node, &shared.structure, selections)?;

    // Phases 1 to 3, interleaved: expand the tree, resolve what the expansion
    // made resolvable, and come back for whatever was waiting.
    let structure_merged = expand_tree(
        Rc::clone(&root_node),
        &shared.definition_scopes,
        Some(&shared.structure),
        &mut identifier_counters,
    )?;

    // Phase 3: Processing (value synchronization and Jinja).
    synchronize_node_values_depth_first(Rc::clone(&root_node));
    let unresolved_templates =
        resolve_all_jinja_values_in_tree(Rc::clone(&root_node), Rc::clone(&root_node));

    // Phase 3.5: "$id_after" resolves at the very end, once the entire tree
    // is built and all Jinja templates (including any inside the id tag
    // itself) are resolved. Every occurrence — including every copy made by
    // an `_each` template or a structure layer — receives its own unique
    // number here.
    let assigned_late_identifiers = resolve_identifier_directives_depth_first(
        Rc::clone(&root_node),
        Rc::clone(&root_node),
        "$id_after",
        &mut identifier_counters,
    );

    // Only worth walking the tree again if that actually wrote something.
    if assigned_late_identifiers {
        synchronize_node_values_depth_first(Rc::clone(&root_node));
    }

    // A template that never resolved is a fault in the configuration, not a
    // note in passing, so it is reported like any other — whether or not the
    // build was asked to validate, since it has nothing to do with a schema.
    let mut diagnostic_errors: Vec<DiagnosticError> = unresolved_templates
        .iter()
        .map(diagnostic_for_unresolved_template)
        .collect();

    // Phase 4: Validation.
    if validate {
        run_validation_phase(
            Rc::clone(&root_node),
            shared.schema_block.as_ref(),
            &mut diagnostic_errors,
        )?;
    }

    // Phase 5: Final cleanup of unprocessed system directives.
    remove_system_directives_from_tree(Rc::clone(&root_node));

    // Phase 6: Default alphabetical ordering of the tree (and thus of any
    // printed output). List items keep their numeric order.
    sort_tree_alphabetically(Rc::clone(&root_node));

    Ok(SingleBuild {
        root_node,
        diagnostics: diagnostic_errors,
        structure_merged,
    })
}

/// Compiles one configuration, for callers that only ever want one.
///
/// With a structure declared and nothing selected this is the tree as written,
/// not a configuration the structure describes. Use `compile_configurations`
/// with `BuildScope::FirstCombination` for one of those.
pub fn build_entire_coal_configuration_tree(
    configuration_directory: &str,
    virtual_file_system: VirtualFileSystemReference,
    validate: bool,
    parameter_overrides: &[ParameterOverride],
    selections: &[SelectionArgument],
) -> Result<(TreeNodeReference, Vec<DiagnosticError>), Box<dyn Error>> {
    let shared = load_shared_build(
        configuration_directory,
        virtual_file_system,
        validate,
        parameter_overrides,
    )?;
    run_single_build(&shared, selections, validate)
}

/// Writes each `-s` value into the tree as the ordinary root field the
/// structure reads it from.
///
/// A selection is any right-aligned part of a selector's path, so the long
/// form settles the captures it names along the way: `-s
/// environments.production.nodes.cop01` pins `environment` as well as picking
/// the node. Those pins are ordinary root fields too, which is exactly what a
/// layer reading `{{ environment }}` needs.
fn seed_selections(
    root_node: &TreeNodeReference,
    structure: &StructureDocument,
    selections: &[SelectionArgument],
) -> Result<(), Box<dyn Error>> {
    for selection in selections {
        let resolved =
            resolve_selection_argument(structure, selection.name.as_deref(), &selection.value, root_node)?;

        seed_selection_value(root_node, &resolved.name, &resolved.value);
        for (pinned_name, pinned_value) in &resolved.pins {
            seed_selection_value(root_node, pinned_name, pinned_value);
        }
    }
    Ok(())
}

/// How many times the build may go round expanding and resolving. Every round
/// either expands something or ends the loop, so this only bounds a chain of
/// directives thousands of files long.
const MAX_EXPANSION_ROUNDS: usize = 256;

/// Grows the tree until it stops growing.
///
/// A directive can name what it needs with a reference — `$include:
/// "environments.{{ environment }}"` — and the field it reads may itself
/// arrive from something that has not been expanded yet: an `_each` template,
/// an earlier include, or the structure merge. Doing this in fixed phases
/// would mean the reference is looked up exactly once, at whichever moment the
/// phase happened to reach it.
///
/// So the expanding phases run together instead. Each round expands every
/// include whose path is known, applies every `_each` template, evaluates
/// every structural directive whose target is known, merges the structure once
/// its selection has settled, and resolves every value whose references are
/// resolved; anything that is not ready is left alone and tried again next
/// round. The loop ends when a round changes nothing, which means either
/// everything is expanded or what is left can never be.
///
/// A `_structure.yaml` splits those rounds in two. Until it has merged, the
/// tree is not yet the configuration being built — the selection has not been
/// moved to the root and no layer has been applied — so a reference reaching
/// outside its own file would be answered with a value the merge is about to
/// replace. Those references, and the `$inherit` and `$ignore` paths that are
/// written with them, are held back until the merge has happened. Only what a
/// file says about itself resolves before it, because nothing merged onto the
/// configuration changes that.
///
/// # Stopping before the merge
///
/// Passing no structure runs only the rounds that come before the merge and
/// stops there, leaving the tree ready for a selection to be seeded into it.
/// That is what makes the work shareable: those rounds cannot see a selection,
/// so their result is the same whichever one follows. Nothing is tidied away
/// at the end of such a run — the `_each` templates are still needed by the
/// layers, and a directive still waiting for the merge is not yet a mistake.
///
/// Returns whether the structure merged.
fn expand_tree(
    root_node: TreeNodeReference,
    definition_scopes: &DefinitionScopes,
    structure: Option<&StructureDocument>,
    identifier_counters: &mut IdentifierCounters,
) -> Result<bool, Box<dyn Error>> {
    let mut structure_applied = false;
    let stops_before_merge = structure.is_none();

    // With no structure to wait for, the tree as loaded is already the
    // configuration and everything may resolve at once.
    let mut stage = match structure {
        Some(structure) if !structure.is_empty() => ResolutionStage::BeforeStructureMerge,
        Some(_) => ResolutionStage::AfterStructureMerge,
        None => ResolutionStage::BeforeStructureMerge,
    };

    for _ in 0..MAX_EXPANSION_ROUNDS {
        let included = process_include_directives_depth_first(
            Rc::clone(&root_node),
            Rc::clone(&root_node),
            definition_scopes,
        )?;

        let each_applied = apply_each_templates_depth_first(
            Rc::clone(&root_node),
            Rc::clone(&root_node),
            identifier_counters,
        );

        // After `_each`, so a field is left out whether or not a template had
        // something to say about it.
        let ignored =
            apply_ignore_lists_depth_first(Rc::clone(&root_node), Rc::clone(&root_node), stage);

        // "$id" tags resolve during this phase.
        let expanded = process_structural_directives_depth_first(
            Rc::clone(&root_node),
            Rc::clone(&root_node),
            identifier_counters,
            stage,
        )?;

        // The structure is merged exactly once. Everything it brings in is
        // then expanded by the rounds that follow, so a layer may itself hold
        // includes, `_each` templates and references.
        let structured = match (structure, structure_applied) {
            (Some(structure), false) => {
                structure_applied = apply_structure_selection(structure, Rc::clone(&root_node))?;
                structure_applied
            }
            _ => false,
        };

        // The moment the configuration is the one being built, everything that
        // was waiting for it may go ahead — starting with this round's own
        // resolution pass.
        if structured {
            stage = ResolutionStage::AfterStructureMerge;
        }

        let resolved =
            resolve_resolvable_jinja_values(Rc::clone(&root_node), Rc::clone(&root_node), stage);

        if !included && !each_applied && !ignored && !expanded && !structured && !resolved {
            // A run that stops before the merge is finished the moment the
            // rounds settle: what is left is waiting for a selection, which is
            // the caller's next move rather than this run's business.
            if stops_before_merge {
                return Ok(false);
            }

            // A structure that never merged is a structure with nothing
            // selected: `coal` with no `-s` builds the configuration as
            // written. There is no merge coming, so what was waiting for one
            // is released and the loop carries on.
            if stage == ResolutionStage::BeforeStructureMerge {
                stage = ResolutionStage::AfterStructureMerge;
                continue;
            }
            break;
        }
    }

    if stops_before_merge {
        return Ok(false);
    }

    // The stencils have done their work. Clearing them now keeps every later
    // phase — final Jinja, `$id_after`, validation — from having to know they
    // were ever there.
    remove_each_templates(&root_node);
    report_directives_that_never_resolved(&root_node)?;
    Ok(structure_applied)
}

fn remove_each_templates(node: &TreeNodeReference) {
    node.borrow_mut()
        .children
        .retain(|child| !is_each_template(child));

    let children = node.borrow().children.clone();
    for child in children {
        remove_each_templates(&child);
    }
}

/// Fails the build when a directive was still waiting for a reference that
/// never arrived, saying which reference rather than reporting the template
/// text as a missing name.
fn report_directives_that_never_resolved(
    root_node: &TreeNodeReference,
) -> Result<(), Box<dyn Error>> {
    let mut waiting = Vec::new();
    collect_waiting_directives(root_node, root_node, &mut waiting);

    if waiting.is_empty() {
        return Ok(());
    }

    Err(format!(
        "The build could not work out what these directives point at, because the fields they read never got a value:\n  {}",
        waiting.join("\n  ")
    )
    .into())
}

fn collect_waiting_directives(
    node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    waiting: &mut Vec<String>,
) {
    // An optional include that never became possible is simply skipped, which
    // is what "optional" promises.
    let is_optional_include = node
        .borrow()
        .key
        .as_deref()
        .is_some_and(|key| key.starts_with("$include") && key.ends_with("_optional"));

    if !is_optional_include {
        if let Some(description) = describe_unresolved_include(node) {
            waiting.push(description);
        }
        if let Some(description) = describe_unresolved_directive(node, root_node) {
            waiting.push(description);
        }
    }

    let children = node.borrow().children.clone();
    for child in children {
        if is_each_template(&child) {
            continue;
        }
        collect_waiting_directives(&child, root_node, waiting);
    }
}

/// Reports the head of an unresolved reference chain where every other
/// finding is reported: on the field itself, in the file it is written in, so
/// the language server can put it on the line that has to change.
fn diagnostic_for_unresolved_template(unresolved: &UnresolvedTemplate) -> DiagnosticError {
    DiagnosticError {
        error_type: "UnresolvedTemplate".to_string(),
        field_path: unresolved.field_path.clone(),
        file_origin: unresolved
            .origin
            .clone()
            .unwrap_or_else(|| "Unknown Origin".to_string()),
        message: unresolved.message(),
        // The reason on its own, without the field it was found on or the
        // fields it holds up: those are the same template read once per
        // selection, and naming them would make one mistake look like several.
        detail: unresolved.reason.clone(),
        span: unresolved.span.clone(),
    }
}

fn run_validation_phase(
    root_node: TreeNodeReference,
    schema_block: Option<&SchemaBlock>,
    diagnostic_errors: &mut Vec<DiagnosticError>,
) -> Result<(), Box<dyn Error>> {
    let Some(schema_block) = schema_block else {
        // Validation was requested but there is nothing to validate against.
        // Report it as a diagnostic so both the CLI (non-zero exit) and the
        // language server surface it.
        let detail = "the configuration directory has no '_schema' folder -> nothing to \
                      validate against"
            .to_string();
        diagnostic_errors.push(DiagnosticError {
            error_type: "SchemaError".to_string(),
            field_path: String::new(),
            file_origin: "_schema".to_string(),
            message: format!("[_schema] {}", detail),
            detail,
            span: Some(Span { line: 1, col: 1 }),
        });
        return Ok(());
    };

    let schema_environment = SchemaEnvironment::new(Rc::clone(&root_node)).with_schema(
        schema_block.unresolved_root.clone(),
        schema_block.definitions.clone(),
        schema_block.origins.clone(),
    );
    // Validation only records its findings on the nodes; it never changes a
    // value. Freezing the tree lets every template read during validation
    // convert each field once and reuse an answer it already worked out.
    let _frozen_tree = freeze_tree_for_reading(&root_node);

    // The root of the schema is the root of the configuration, so the path
    // through the schema starts empty and every step names a field.
    apply_schema_definition_to_tree_node(
        Rc::clone(&root_node),
        &schema_block.root,
        "",
        &schema_environment,
    )?;

    generate_diagnostics_for_entire_tree(root_node, diagnostic_errors);
    diagnostic_errors.extend(schema_environment.into_missing_field_diagnostics());
    Ok(())
}
