use clap::Parser;
use std::sync::{Arc, Mutex};
use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::coal_tree::tree_to_yaml;
use coal::vfs::VirtualFileSystem;
use coal::shared_logic::find_target_node_using_reference_path; // NEW IMPORT
use std::rc::Rc;

#[derive(Parser, Debug)]
#[command(name = "coal", version, about = "Coalescence (COAL) configuration builder and validator", long_about = None)]
struct CommandLineArguments {
    /// Apply schemas and output validation errors to stderr
    #[arg(short = 'v', long = "validate")]
    validate_boolean: bool,

    /// NEW: Path of a specific field to analyze (e.g., 'processes.process_1.field_1')
    #[arg(short = 'a', long = "analyze")]
    analyze_path: Option<String>,
}

fn main() {
    let parsed_arguments = CommandLineArguments::parse();
    let vfs = Arc::new(Mutex::new(VirtualFileSystem::new()));

    match build_entire_coal_configuration_tree("main.yaml", "schema", Arc::clone(&vfs), parsed_arguments.validate_boolean) {
        Ok((fully_built_root_coal_tree_node, generated_diagnostic_errors_array)) => {

            // NEW: If analyze is requested, print the node details and exit early
            if let Some(ref path) = parsed_arguments.analyze_path {
                let formatted_path = if path.starts_with("ROOT.") { path.clone() } else { format!("ROOT.{}", path) };

                if let Some(node) = find_target_node_using_reference_path(&formatted_path, Rc::clone(&fully_built_root_coal_tree_node), Rc::clone(&fully_built_root_coal_tree_node)) {
                    let b = node.borrow();
                    println!("--- Field Analysis: {} ---", path);
                    println!("Key:              {:?}", b.key);
                    println!("Value:            {:?}", b.value);
                    println!("Field Type:       {:?}", b.field_type);
                    println!("Origin File:      {:?}", b.origin);
                    println!("Schema Origin:    {:?}", b.schema_origin);
                    println!("Optional:         {}", b.optional);
                    println!("Allowed:          {}", b.allowed);
                    println!("Default Value:    {:?}", b.default_value);
                    println!("Allowed Values:   {:?}", b.allowed_values);
                    if let Some(span) = &b.span {
                        println!("Physical Span:    Line {}, Col {}", span.line, span.col);
                    } else {
                        println!("Physical Span:    None");
                    }
                } else {
                    eprintln!("Error: Path '{}' not found in the resolved configuration.", path);
                }
                return;
            }

            let final_yaml_value = tree_to_yaml(Rc::clone(&fully_built_root_coal_tree_node));
            let formatted_yaml_string = serde_yaml::to_string(&final_yaml_value).unwrap();
            println!("{}", formatted_yaml_string);

            if parsed_arguments.validate_boolean {
                if generated_diagnostic_errors_array.is_empty() {
                    eprintln!("Success: No validation errors found in the configuration tree.");
                } else {
                    eprintln!("Found {} validation errors:\n", generated_diagnostic_errors_array.len());
                    for diagnostic_error_object in generated_diagnostic_errors_array {
                        let location_string = if let Some(span) = &diagnostic_error_object.span_option {
                            format!("Line {}, Col {}", span.line, span.col)
                        } else {
                            String::from("Unknown Location")
                        };

                        eprintln!("{} at [{}]", diagnostic_error_object.error_message_string, location_string);
                    }
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("\n❌ Fatal Error: {}\n", e);
            std::process::exit(1);
        }
    }
}