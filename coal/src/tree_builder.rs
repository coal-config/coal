use crate::coal_tree::{CoalTree, Span};
use crate::diagnostics::{DiagnosticError, generate_diagnostics_for_entire_tree};
use crate::identifier_directives::{IdentifierCounters, resolve_identifier_directives_depth_first};
use crate::include_directives::process_include_directives_depth_first;
use crate::parameter_overrides::{ParameterOverride, apply_parameter_overrides};
use crate::reference_resolution::resolve_all_jinja_values_in_tree;
use crate::schema_application::{
    apply_schema_definition_to_tree_node, load_all_schema_files_from_directory,
};
use crate::structural_directives::process_structural_directives_depth_first;
use crate::tree_finalization::{
    prune_empty_mapping_nodes, remove_system_directives_from_tree, sort_tree_alphabetically,
    synchronize_node_values_depth_first,
};
use crate::tree_operations::build_tree_from_marked_node;
use crate::type_aliases::{TreeNodeReference, VirtualFileSystemReference, new_tree_node_reference};
use crate::yaml_values::convert_marked_yaml_node_to_value;
use std::collections::HashMap;
use std::error::Error;
use std::rc::Rc;
use std::sync::Arc;

/// Runs the full compilation pipeline: command-line parameters, include
/// expansion, structural directives, value synchronization, Jinja resolution,
/// optional schema validation, directive cleanup, and alphabetical ordering.
/// Returns the fully built tree together with any validation diagnostics.
///
/// `parameter_overrides` are values supplied on the command line. They are
/// seeded into the tree before includes and structural directives run, so a
/// value such as an `environment` can drive the rest of the build — for
/// example through Jinja references inside `$include` paths or `$inherit`
/// targets. Once seeded they behave like any value declared in the
/// configuration files.
pub fn build_entire_coal_configuration_tree(
    entry_path: &str,
    schema_directory_path: &str,
    virtual_file_system: VirtualFileSystemReference,
    validate: bool,
    parameter_overrides: &[ParameterOverride],
) -> Result<(TreeNodeReference, Vec<DiagnosticError>), Box<dyn Error>> {
    let root_node = parse_root_document(entry_path, Arc::clone(&virtual_file_system))?;

    // Phase 0: Seed command-line parameters before anything else so they can
    // be referenced (via Jinja) by include paths and inheritance targets
    // while the tree is still being assembled.
    apply_parameter_overrides(Rc::clone(&root_node), parameter_overrides);

    // Phase 1: Expansion (includes).
    process_include_directives_depth_first(
        Rc::clone(&root_node),
        Rc::clone(&root_node),
        Arc::clone(&virtual_file_system),
    )?;

    // A single counter map is shared between "$id" and "$id_after", so a
    // counter name keeps incrementing regardless of which variant uses it.
    let mut identifier_counters = IdentifierCounters::new();

    // Phase 2: Structural directives ($inherit, $each, $parent, $id).
    // "$id" tags resolve during this phase; whenever a node is the target of
    // $inherit/$override, its "$id" tags are resolved just before cloning so
    // the inheritor receives the actual ID value instead of the tag.
    process_structural_directives_depth_first(
        Rc::clone(&root_node),
        Rc::clone(&root_node),
        &mut identifier_counters,
    )?;

    // Phase 3: Processing (value synchronization and Jinja).
    synchronize_node_values_depth_first(Rc::clone(&root_node));
    resolve_all_jinja_values_in_tree(Rc::clone(&root_node), Rc::clone(&root_node));

    // Phase 3.5: "$id_after" resolves at the very end, once the entire tree
    // is built and all Jinja templates (including any inside the id tag
    // itself) are resolved. Every occurrence — including every clone made by
    // $each/$inherit/$override — receives its own unique number here.
    resolve_identifier_directives_depth_first(
        Rc::clone(&root_node),
        Rc::clone(&root_node),
        "$id_after",
        &mut identifier_counters,
    );
    synchronize_node_values_depth_first(Rc::clone(&root_node));

    prune_empty_mapping_nodes(Rc::clone(&root_node));

    // Phase 4: Validation.
    let mut diagnostic_errors = Vec::new();
    if validate {
        run_validation_phase(
            Rc::clone(&root_node),
            virtual_file_system,
            schema_directory_path,
            &mut diagnostic_errors,
        )?;
    }

    // Phase 5: Final cleanup of unprocessed system directives.
    remove_system_directives_from_tree(Rc::clone(&root_node));

    // Phase 6: Default alphabetical ordering of the tree (and thus of any
    // printed output). List items keep their numeric order.
    sort_tree_alphabetically(Rc::clone(&root_node));

    Ok((root_node, diagnostic_errors))
}

fn run_validation_phase(
    root_node: TreeNodeReference,
    virtual_file_system: VirtualFileSystemReference,
    schema_directory_path: &str,
    diagnostic_errors: &mut Vec<DiagnosticError>,
) -> Result<(), Box<dyn Error>> {
    // Schema loading problems (unreadable directory, invalid YAML) must fail
    // the build instead of silently validating against nothing.
    let schemas = load_all_schema_files_from_directory(schema_directory_path, virtual_file_system)?;

    let Some(main_schema) = schemas.get("main") else {
        // Validation was requested but there is nothing to validate against.
        // Report it as a diagnostic so both the CLI (non-zero exit) and the
        // language server surface it.
        diagnostic_errors.push(DiagnosticError {
            error_type: "SchemaError".to_string(),
            field_path: String::new(),
            file_origin: schema_directory_path.to_string(),
            message: format!(
                "[{}] no 'main' schema found in schema directory -> nothing to validate against",
                schema_directory_path
            ),
            span: Some(Span { line: 1, col: 1 }),
        });
        return Ok(());
    };

    apply_schema_definition_to_tree_node(
        Rc::clone(&root_node),
        main_schema,
        "main",
        Rc::clone(&root_node),
        &schemas,
        &HashMap::new(),
    )?;

    generate_diagnostics_for_entire_tree(root_node, diagnostic_errors);
    Ok(())
}

fn parse_root_document(
    entry_path: &str,
    virtual_file_system: VirtualFileSystemReference,
) -> Result<TreeNodeReference, Box<dyn Error>> {
    let content = virtual_file_system
        .lock()
        .unwrap()
        .get_file_content(entry_path)?;
    let marked_root = marked_yaml::parse_yaml(0, &content)
        .map_err(|error| format!("YAML Parse Error in '{}': {}", entry_path, error))?;

    let root_node = new_tree_node_reference(CoalTree {
        key: Some("ROOT".to_string()),
        value: Some(convert_marked_yaml_node_to_value(&marked_root)),
        origin: Some(entry_path.to_string()),
        span: Some(Span { line: 1, col: 1 }),
        ..Default::default()
    });

    build_tree_from_marked_node(&marked_root, Rc::clone(&root_node), entry_path);
    Ok(root_node)
}
