use clap::Parser;
use coal::coal_tree::tree_to_yaml;
use coal::diagnostics::DiagnosticError;
use coal::reference_resolution::find_target_node_using_reference_path;
use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::type_aliases::{TreeNodeReference, new_virtual_file_system_reference};
use coal::virtual_file_system::VirtualFileSystem;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "coal",
    version,
    about = "Coalescence (COAL) configuration builder and validator",
    long_about = None
)]
struct CommandLineArguments {
    /// Validate the compiled tree against the schema definitions
    #[arg(short = 'v', long = "validate")]
    validate: bool,

    /// Dump the internal node properties of a specific field (e.g., processes.cms)
    #[arg(short = 'a', long = "analyze")]
    analyze_path: Option<String>,

    /// Optional path to print a specific part of the configuration tree (e.g., environments.production)
    #[arg(index = 1)]
    print_path: Option<String>,
}

fn main() {
    let arguments = CommandLineArguments::parse();
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());

    let build_result = build_entire_coal_configuration_tree(
        "main.yaml",
        "schema",
        Arc::clone(&virtual_file_system),
        arguments.validate,
    );

    match build_result {
        Ok((root_node, diagnostic_errors)) => {
            if let Some(ref analyze_path) = arguments.analyze_path {
                analyze_target_path(analyze_path, root_node);
                return;
            }

            print_yaml_configuration(root_node, arguments.print_path.as_deref());

            if arguments.validate {
                report_validation_diagnostics(diagnostic_errors);
            }
        }
        Err(error) => {
            report_fatal_error(error);
        }
    }
}

fn analyze_target_path(path: &str, root_node: TreeNodeReference) {
    let absolute_path = if path.starts_with("ROOT.") {
        path.to_string()
    } else {
        format!("ROOT.{}", path)
    };

    if let Some(node) = find_target_node_using_reference_path(
        &absolute_path,
        Rc::clone(&root_node),
        Rc::clone(&root_node),
    ) {
        display_analyzed_node_details(path, node);
    } else {
        eprintln!(
            "Error: Path '{}' not found in the resolved configuration.",
            path
        );
    }
}

fn display_analyzed_node_details(path: &str, node: TreeNodeReference) {
    let node_borrowed = node.borrow();
    println!("--- Field Analysis: {} ---", path);
    println!("Key:              {:?}", node_borrowed.key);
    println!("Value:            {:?}", node_borrowed.value);
    println!("Field Type:       {:?}", node_borrowed.field_type);
    println!("Origin File:      {:?}", node_borrowed.origin);
    println!("Schema Origin:    {:?}", node_borrowed.schema_origin);
    println!("Optional:         {}", node_borrowed.optional);
    println!("Allowed:          {}", node_borrowed.allowed);
    println!("Default Value:    {:?}", node_borrowed.default_value);
    println!("Allowed Values:   {:?}", node_borrowed.allowed_values);
    if let Some(span) = &node_borrowed.span {
        println!("Physical Span:    Line {}, Col {}", span.line, span.col);
    } else {
        println!("Physical Span:    None");
    }
}

fn print_yaml_configuration(root_node: TreeNodeReference, print_path: Option<&str>) {
    let full_yaml_value = tree_to_yaml(root_node);

    let value_to_print = match print_path {
        Some(path) => extract_value_at_path(&full_yaml_value, path),
        None => full_yaml_value,
    };

    let formatted_yaml = serde_yaml::to_string(&value_to_print).unwrap();
    println!("{}", formatted_yaml);
}

/// Walks the compiled YAML down a dotted path and returns the nested value,
/// exiting with an error when any segment is missing.
fn extract_value_at_path(full_yaml_value: &serde_yaml::Value, path: &str) -> serde_yaml::Value {
    let mut current_value = full_yaml_value.clone();

    for segment in path.split('.') {
        let next_value = match current_value {
            serde_yaml::Value::Mapping(ref mapping) => mapping.get(segment).cloned(),
            _ => None,
        };

        match next_value {
            Some(value) => current_value = value,
            None => {
                eprintln!("Error: Path '{}' not found for printing.", path);
                std::process::exit(1);
            }
        }
    }

    current_value
}

fn report_validation_diagnostics(diagnostic_errors: Vec<DiagnosticError>) {
    if diagnostic_errors.is_empty() {
        eprintln!("Success: No validation errors found in the configuration tree.");
        return;
    }

    eprintln!("Found {} validation errors:\n", diagnostic_errors.len());
    for diagnostic_error in diagnostic_errors {
        let location = match &diagnostic_error.span {
            Some(span) => format!("Line {}, Col {}", span.line, span.col),
            None => String::from("Unknown Location"),
        };

        eprintln!("{} at [{}]", diagnostic_error.message, location);
    }
    std::process::exit(1);
}

fn report_fatal_error(error: Box<dyn std::error::Error>) {
    eprintln!("\n❌ Fatal Error: {}\n", error);
    std::process::exit(1);
}
