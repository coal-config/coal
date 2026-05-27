use clap::Parser;
use coal::coal_tree::{tree_to_yaml, CoalTree};
use coal::diagnostics::DiagnosticError;
use coal::shared_logic::find_target_node_using_reference_path;
use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::vfs::VirtualFileSystem;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

// 1. Capture the dynamic release version or fall back to Cargo.toml
const VERSION: &str = match option_env!("RELEASE_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser, Debug)]
// 2. Pass the VERSION constant explicitly here
#[command(name = "coal", version = VERSION, about = "Coalescence (COAL) configuration builder and validator", long_about = None)]
struct CommandLineArguments {
    #[arg(short = 'v', long = "validate")]
    validate_boolean: bool,

    #[arg(short = 'a', long = "analyze")]
    analyze_path: Option<String>,
}

fn main() {
    let parsed_arguments = CommandLineArguments::parse();
    let vfs = Arc::new(Mutex::new(VirtualFileSystem::new()));

    match build_entire_coal_configuration_tree(
        "main.yaml",
        "schema",
        Arc::clone(&vfs),
        parsed_arguments.validate_boolean,
    ) {
        Ok((fully_built_root_coal_tree_node, generated_diagnostic_errors_array)) => {
            if let Some(ref path) = parsed_arguments.analyze_path {
                analyze_target_path(path, fully_built_root_coal_tree_node);
                return;
            }

            print_formatted_yaml_configuration(Rc::clone(&fully_built_root_coal_tree_node));

            if parsed_arguments.validate_boolean {
                process_and_display_validation_diagnostics(generated_diagnostic_errors_array);
            }
        }
        Err(e) => {
            handle_fatal_error(e);
        }
    }
}

fn analyze_target_path(path: &str, fully_built_root_coal_tree_node: Rc<RefCell<CoalTree>>) {
    let formatted_path = if path.starts_with("ROOT.") {
        path.to_string()
    } else {
        format!("ROOT.{}", path)
    };

    if let Some(node) = find_target_node_using_reference_path(
        &formatted_path,
        Rc::clone(&fully_built_root_coal_tree_node),
        Rc::clone(&fully_built_root_coal_tree_node),
    ) {
        display_analyzed_node_details(path, node);
    } else {
        eprintln!("Error: Path '{}' not found in the resolved configuration.", path);
    }
}

fn display_analyzed_node_details(path: &str, node: Rc<RefCell<CoalTree>>) {
    let borrowed_node = node.borrow();
    println!("--- Field Analysis: {} ---", path);
    println!("Key:              {:?}", borrowed_node.key);
    println!("Value:            {:?}", borrowed_node.value);
    println!("Field Type:       {:?}", borrowed_node.field_type);
    println!("Origin File:      {:?}", borrowed_node.origin);
    println!("Schema Origin:    {:?}", borrowed_node.schema_origin);
    println!("Optional:         {}", borrowed_node.optional);
    println!("Allowed:          {}", borrowed_node.allowed);
    println!("Default Value:    {:?}", borrowed_node.default_value);
    println!("Allowed Values:   {:?}", borrowed_node.allowed_values);
    if let Some(span) = &borrowed_node.span {
        println!("Physical Span:    Line {}, Col {}", span.line, span.col);
    } else {
        println!("Physical Span:    None");
    }
}

fn print_formatted_yaml_configuration(fully_built_root_coal_tree_node: Rc<RefCell<CoalTree>>) {
    let final_yaml_value = tree_to_yaml(fully_built_root_coal_tree_node);
    let formatted_yaml_string = serde_yaml::to_string(&final_yaml_value).unwrap();
    println!("{}", formatted_yaml_string);
}

fn process_and_display_validation_diagnostics(
    generated_diagnostic_errors_array: Vec<DiagnosticError>,
) {
    if generated_diagnostic_errors_array.is_empty() {
        eprintln!("Success: No validation errors found in the configuration tree.");
    } else {
        eprintln!(
            "Found {} validation errors:\n",
            generated_diagnostic_errors_array.len()
        );
        for diagnostic_error_object in generated_diagnostic_errors_array {
            let location_string = if let Some(span) = &diagnostic_error_object.span_option {
                format!("Line {}, Col {}", span.line, span.col)
            } else {
                String::from("Unknown Location")
            };

            eprintln!(
                "{} at [{}]",
                diagnostic_error_object.error_message_string, location_string
            );
        }
        std::process::exit(1);
    }
}

fn handle_fatal_error(error: Box<dyn std::error::Error>) {
    eprintln!("\n❌ Fatal Error: {}\n", error);
    std::process::exit(1);
}