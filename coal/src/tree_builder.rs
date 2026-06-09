use crate::coal_tree::{CoalTree, Span};
use crate::build_includes::process_all_include_functions_depth_first;
use crate::build_ast::process_abstract_syntax_tree_functions_depth_first;
use crate::apply_schemas::{load_all_schema_files_from_directory, apply_schema_definition_to_tree_node};
use crate::diagnostics::{generate_diagnostics_for_entire_tree, DiagnosticError};
use crate::shared_logic::{
    build_tree_from_marked_node, marked_node_to_serde_value,
    resolve_all_jinja_values_in_tree, synchronize_node_values_depth_first, clean_up_system_directives,
    prune_empty_mapping_nodes
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

    // Phase 1: Expansion (Includes)
    process_all_include_functions_depth_first(Rc::clone(&root), Rc::clone(&root), Arc::clone(&vfs))?;

    let mut id_counters = HashMap::new();

    // Phase 2: AST Directives Expansion ($inherit, $each, $parent, $id)
    process_abstract_syntax_tree_functions_depth_first(Rc::clone(&root), Rc::clone(&root), &mut id_counters)?;

    // Phase 3: Processing (Sync and Jinja)
    synchronize_node_values_depth_first(Rc::clone(&root));
    resolve_all_jinja_values_in_tree(Rc::clone(&root), Rc::clone(&root));

    // prune_empty_mapping_nodes(Rc::clone(&root));

    // Phase 4: Validation
    let mut diagnostic_errors = Vec::new();
    if validate {
        execute_validation_pipeline(Rc::clone(&root), vfs, schema_directory_path, &mut diagnostic_errors)?; // UPDATED
    }

    // Phase 5: Final Cleanup
    clean_up_system_directives(Rc::clone(&root));

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
