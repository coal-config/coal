use crate::coal_tree::{CoalTree, Span};
use crate::build_includes::process_all_include_functions_depth_first;
use crate::build_ast::process_abstract_syntax_tree_functions_depth_first;
use crate::apply_schemas::{load_all_schema_files_from_directory, apply_schema_definition_to_tree_node};
use crate::diagnostics::{generate_diagnostics_for_entire_tree, DiagnosticError};
use crate::shared_logic::{
    build_tree_from_marked_node, marked_node_to_serde_value,
    resolve_all_jinja_values_in_tree, resolve_id_directives_depth_first,
    synchronize_node_values_depth_first, clean_up_system_directives,
    prune_empty_mapping_nodes, sort_tree_alphabetically
};
use crate::type_aliases::{VfsRef, TreeRef, new_tree_ref};
use std::collections::HashMap;
use std::error::Error;
use std::rc::Rc;
use std::sync::Arc;

pub fn build_entire_coal_configuration_tree(
    entry_path: &str,
    schema_directory_path: &str,
    vfs: VfsRef,
    validate: bool,
) -> Result<(TreeRef, Vec<DiagnosticError>), Box<dyn Error>> {
    let root = parse_initial_root(entry_path, Arc::clone(&vfs))?;

    // A single counter map is shared across all ID phases so that a counter
    // name (e.g. "db_counters") keeps incrementing regardless of whether it
    // is referenced via $id_now, $id, or $id_end.
    let mut id_counters = HashMap::new();

    // Phase 1: Expansion (Includes). "$id_now" directives are resolved here,
    // as soon as each node is detected during the depth-first expansion.
    process_all_include_functions_depth_first(Rc::clone(&root), Rc::clone(&root), Arc::clone(&vfs), &mut id_counters)?;

    // Phase 1.5: "$id" directives resolve once all includes have been spliced
    // in, but before any AST directives clone or morph the structure.
    resolve_id_directives_depth_first(Rc::clone(&root), Rc::clone(&root), "$id", &mut id_counters);

    // Phase 2: AST Directives Expansion ($inherit, $each, $parent)
    process_abstract_syntax_tree_functions_depth_first(Rc::clone(&root), Rc::clone(&root))?;

    // Phase 2.5: "$id_end" directives resolve only once the whole tree has
    // been built, so every clone produced by $each/$inherit gets a unique ID.
    resolve_id_directives_depth_first(Rc::clone(&root), Rc::clone(&root), "$id_end", &mut id_counters);

    // Phase 3: Processing (Sync and Jinja)
    synchronize_node_values_depth_first(Rc::clone(&root));
    resolve_all_jinja_values_in_tree(Rc::clone(&root), Rc::clone(&root));

    prune_empty_mapping_nodes(Rc::clone(&root));

    // Phase 4: Validation
    let mut diagnostic_errors = Vec::new();
    if validate {
        execute_validation_pipeline(Rc::clone(&root), vfs, schema_directory_path, &mut diagnostic_errors)?; // UPDATED
    }

    // Phase 5: Final Cleanup
    clean_up_system_directives(Rc::clone(&root));

    // Phase 6: Default alphabetical ordering of the tree (and thus of any
    // printed output). List items keep their numeric order.
    sort_tree_alphabetically(Rc::clone(&root));

    Ok((root, diagnostic_errors))
}

fn execute_validation_pipeline(
    root: TreeRef,
    vfs: VfsRef,
    schema_dir_path: &str,
    errors: &mut Vec<DiagnosticError>
) -> Result<(), Box<dyn Error>> {
    let schemas = load_all_schema_files_from_directory(schema_dir_path, vfs).unwrap_or_default(); // UPDATED
    if let Some(main_schema) = schemas.get("main") {
        apply_schema_definition_to_tree_node(Rc::clone(&root), main_schema, "main", Rc::clone(&root), &schemas, &HashMap::new())?;
    }
    generate_diagnostics_for_entire_tree(root, errors);
    Ok(())
}

fn parse_initial_root(path: &str, vfs: VfsRef) -> Result<TreeRef, Box<dyn Error>> {
    let content = vfs.lock().unwrap().get_file_content(path)?;
    let marked = marked_yaml::parse_yaml(0, &content)
        .map_err(|e| format!("YAML Parse Error in '{}': {}", path, e))?;

    let root = new_tree_ref(CoalTree {
        key: Some("ROOT".to_string()),
        value: Some(marked_node_to_serde_value(&marked)),
        origin: Some(path.to_string()),
        span: Some(Span { line: 1, col: 1 }),
        ..Default::default()
    });

    build_tree_from_marked_node(&marked, Rc::clone(&root), path);
    Ok(root)
}
