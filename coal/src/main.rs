use clap::Parser;
use coal::coal_tree::tree_to_yaml;
use coal::diagnostics::DiagnosticError;
use coal::parameter_overrides::{ParameterOverride, parse_parameter_override};
use coal::reference_resolution::find_target_node_using_reference_path;
use coal::tree_builder::{
    BuildScope, CompiledConfiguration, SelectionArgument, compile_configurations,
};
use coal::type_aliases::{TreeNodeReference, new_virtual_file_system_reference};
use coal::validation_report::{GroupedFinding, group_findings};
use coal::virtual_file_system::VirtualFileSystem;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

/// The directory a build reads when none is named: the one it was started in.
const DEFAULT_CONFIGURATION_DIRECTORY: &str = ".";

#[derive(Parser, Debug)]
#[command(
    name = "coal",
    version,
    about = "Coalescence (COAL) configuration builder and validator",
    long_about = None
)]
struct CommandLineArguments {
    /// Check the configuration against the schema instead of printing it.
    /// With a _structure.yaml and no selection, every configuration the
    /// structure describes is built and checked, and each finding is reported
    /// once, where it is written.
    #[arg(short = 'v', long = "validate")]
    validate: bool,

    /// Dump the internal node properties of a specific field (e.g., processes.cms)
    #[arg(short = 'a', long = "analyze")]
    analyze_path: Option<String>,

    /// Build one configuration out of the many the tree describes, as declared
    /// in _structure.yaml. Repeat it once per selector.
    /// Example: -s cop01 -s operational, or -s environments.production.nodes.cop01
    /// when the short name would name more than one place
    #[arg(short = 's', long = "select", value_name = "[NAME=]VALUE")]
    selections: Vec<String>,

    /// Pin a value that the configuration cannot override (repeatable).
    /// Example: -p environment.paths.config=/home/config
    #[arg(short = 'p', long = "param", value_name = "PATH=VALUE")]
    parameters: Vec<String>,

    /// Optional path to print a specific part of the configuration tree (e.g., environments.production)
    #[arg(index = 1)]
    print_path: Option<String>,
}

fn main() {
    let arguments = CommandLineArguments::parse();
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());

    let parameter_overrides = match parse_all_parameter_overrides(&arguments.parameters) {
        Ok(parameter_overrides) => parameter_overrides,
        Err(error) => report_fatal_error(error),
    };
    let selections = parse_all_selections(&arguments.selections);

    // Printing wants one configuration and validating wants all of them. Where
    // the selections already say which, both get that one.
    let scope = match arguments.validate {
        true => BuildScope::EveryCombination,
        false => BuildScope::FirstCombination,
    };

    let build_result = compile_configurations(
        DEFAULT_CONFIGURATION_DIRECTORY,
        Arc::clone(&virtual_file_system),
        scope,
        arguments.validate,
        &parameter_overrides,
        &selections,
    );

    let configurations = match build_result {
        Ok(configurations) => configurations,
        Err(error) => report_fatal_error(error),
    };

    if arguments.validate {
        report_validation(&configurations, &selections);
        return;
    }

    build_one_configuration(configurations, &arguments);
}

/// Prints the one configuration that was built, and says so when the build
/// chose it rather than being told.
fn build_one_configuration(
    configurations: Vec<CompiledConfiguration>,
    arguments: &CommandLineArguments,
) {
    let configuration = configurations
        .into_iter()
        .next()
        .expect("a build produces at least one configuration");

    let chosen_for_you = configuration.selection.clone();

    if let Some(ref analyze_path) = arguments.analyze_path {
        analyze_target_path(analyze_path, configuration.root_node);
        return;
    }

    print_yaml_configuration(configuration.root_node, arguments.print_path.as_deref());
    report_build_diagnostics(&configuration.diagnostics);

    // The note goes last, after the configuration it is about, because that is
    // where it is read: the build has just printed one of several things it
    // could have, and it should not be a surprise which.
    if let Some(combination) = chosen_for_you {
        let selection_arguments: Vec<String> = combination
            .values
            .iter()
            .map(|(name, value)| format!("-s {}={}", name, value))
            .collect();
        eprintln!(
            "\nNote: nothing was selected, so this is the first configuration the structure \
             describes: {}.\n      Say which one you want to be sure of it: coal {}",
            combination.describe(),
            selection_arguments.join(" ")
        );
    }
}

/// Reports what validating found, once per mistake rather than once per
/// configuration that ran into it.
fn report_validation(configurations: &[CompiledConfiguration], selections: &[SelectionArgument]) {
    let findings = group_findings(configurations);
    let checked = describe_what_was_checked(configurations, selections);

    if findings.is_empty() {
        eprintln!("Success: no problems found in {}.", checked);
        return;
    }

    eprintln!(
        "Checked {}.\n\nFound {}:\n",
        checked,
        match findings.len() {
            1 => "1 problem".to_string(),
            count => format!("{} problems", count),
        }
    );

    for finding in &findings {
        eprintln!("{}", describe_finding(finding));
    }

    std::process::exit(1);
}

fn describe_what_was_checked(
    configurations: &[CompiledConfiguration],
    selections: &[SelectionArgument],
) -> String {
    match configurations {
        [] => "nothing".to_string(),
        [only] => match (&only.selection, selections.is_empty()) {
            (Some(combination), _) => format!("1 configuration: {}", combination.describe()),
            // A build told exactly what to make knows the answer from the
            // command line, since nothing had to be worked out to get there.
            (None, false) => {
                let named: Vec<&str> = selections
                    .iter()
                    .map(|selection| selection.value.as_str())
                    .collect();
                format!("1 configuration: {}", named.join(", "))
            }
            (None, true) => "the configuration".to_string(),
        },
        many => format!("{} configurations", many.len()),
    }
}

/// Writes one finding: where it is, what field it is about, what is wrong, and
/// how far it reaches.
fn describe_finding(finding: &GroupedFinding) -> String {
    let mut lines = vec![format!(
        "{}  {}",
        display_location(finding),
        finding.error_type
    )];

    let fields = finding.describe_fields();
    lines.push(match fields.is_empty() {
        true => format!("  {}", finding.detail),
        false => format!("  {} -- {}", fields, finding.detail),
    });

    for line in finding.elaboration.lines().filter(|line| !line.trim().is_empty()) {
        lines.push(format!("  {}", line.trim_end()));
    }

    if let Some(reach) = finding.describe_reach() {
        lines.push(format!("  {}", reach));
    }

    lines.join("\n") + "\n"
}

/// Where a finding is, written the way it is worth reading: relative to the
/// directory the build was started in, since that is where the files are.
fn display_location(finding: &GroupedFinding) -> String {
    let origin = shorten_path(&finding.file_origin);
    match &finding.span {
        Some(span) => format!("{}:{}:{}", origin, span.line, span.col),
        None => origin,
    }
}

fn shorten_path(path: &str) -> String {
    let Ok(working_directory) = std::env::current_dir() else {
        return path.to_string();
    };
    match Path::new(path).strip_prefix(&working_directory) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string(),
    }
}

/// Reads the `-s` arguments. A value on its own means the structure's one
/// selectable thing; the build says so if there is more than one.
fn parse_all_selections(raw_selections: &[String]) -> Vec<SelectionArgument> {
    raw_selections
        .iter()
        .map(|raw_selection| match raw_selection.split_once('=') {
            Some((name, value)) => SelectionArgument {
                name: Some(name.trim().to_string()),
                value: value.to_string(),
            },
            None => SelectionArgument {
                name: None,
                value: raw_selection.clone(),
            },
        })
        .collect()
}

fn analyze_target_path(path: &str, root_node: TreeNodeReference) {
    if let Some(node) =
        find_target_node_using_reference_path(path, Rc::clone(&root_node), Rc::clone(&root_node))
    {
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
    println!("File Root:        {}", node_borrowed.file_root);
    println!("Schema Origin:    {:?}", node_borrowed.schema_origin);
    println!("Optional:         {}", node_borrowed.optional);
    println!("Allowed:          {}", node_borrowed.allowed);
    println!("Default Value:    {:?}", node_borrowed.default_value);
    println!("Allowed Values:   {:?}", node_borrowed.allowed_values);
    println!("Schema Error:     {:?}", node_borrowed.schema_error);
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

/// Reports the errors a build finds on its own, without being asked to
/// validate — today, references that never resolved. The configuration is
/// still printed, because the fields that did resolve are still worth having
/// while the reference is being fixed.
fn report_build_diagnostics(diagnostic_errors: &[DiagnosticError]) {
    for diagnostic_error in diagnostic_errors {
        let location = match &diagnostic_error.span {
            Some(span) => format!(
                "[{}, line {}, col {}]",
                diagnostic_error.file_origin, span.line, span.col
            ),
            None => format!("[{}]", diagnostic_error.file_origin),
        };

        match diagnostic_error.message.split_once('\n') {
            Some((first_line, remainder)) => {
                eprintln!("Error: {} {}\n{}", first_line, location, remainder)
            }
            None => eprintln!("Error: {} {}", diagnostic_error.message, location),
        }
    }
}

fn parse_all_parameter_overrides(
    raw_parameters: &[String],
) -> Result<Vec<ParameterOverride>, Box<dyn std::error::Error>> {
    raw_parameters
        .iter()
        .map(|raw_parameter| parse_parameter_override(raw_parameter))
        .collect()
}

fn report_fatal_error(error: Box<dyn std::error::Error>) -> ! {
    eprintln!("\n❌ Fatal Error: {}\n", error);
    std::process::exit(1);
}
