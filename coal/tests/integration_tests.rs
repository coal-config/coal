use coal::coal_tree::tree_to_yaml;
use coal::diagnostics::DiagnosticError;
use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::type_aliases::{TreeNodeReference, new_virtual_file_system_reference};
use coal::virtual_file_system::VirtualFileSystem;
use serde_yaml::Value;
use std::fs;
use std::path::{Path, PathBuf};

// =====================================================================
// TEST RUNNER
//
// Each subdirectory of a suite is one test case containing:
//   main.yaml             the configuration entry point
//   schema/               schema definitions (validation suite)
//   expected.yaml         the expected compiled output
//                         (required for build cases, optional otherwise)
//   expected_errors.yaml  the expected diagnostics as a list of
//                         {type, path, contains} entries; an empty list
//                         asserts the configuration validates cleanly
//                         (required for validation cases)
// =====================================================================

/// Runs every test case in the suite and reports all failures at once.
fn run_directory_tests(test_suite_directory: &str, validate: bool) {
    let test_case_directories = collect_test_case_directories(test_suite_directory);
    let mut failures = Vec::new();

    for test_case_directory in &test_case_directories {
        let test_name = test_case_directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        match run_single_test_case(test_case_directory, validate) {
            Ok(()) => println!("  [PASSED] {}", test_name),
            Err(failure) => {
                println!("  [FAILED] {}", test_name);
                failures.push(format!("--- {} ---\n{}", test_name, failure));
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "\n{} of {} test cases failed in '{}':\n\n{}\n",
            failures.len(),
            test_case_directories.len(),
            test_suite_directory,
            failures.join("\n\n")
        );
    }
}

/// A suite that is missing or empty is a test setup bug, not a pass.
fn collect_test_case_directories(test_suite_directory: &str) -> Vec<PathBuf> {
    let suite_path = Path::new(test_suite_directory);
    assert!(
        suite_path.is_dir(),
        "Test suite directory '{}' does not exist",
        test_suite_directory
    );

    let mut test_case_directories: Vec<PathBuf> = fs::read_dir(suite_path)
        .expect("Failed to read test suite directory")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    test_case_directories.sort();

    assert!(
        !test_case_directories.is_empty(),
        "Test suite directory '{}' contains no test cases",
        test_suite_directory
    );

    test_case_directories
}

fn run_single_test_case(test_case_directory: &Path, validate: bool) -> Result<(), String> {
    let main_yaml_path = test_case_directory.join("main.yaml");
    let schema_directory = test_case_directory.join("schema");
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());

    let (root_node, diagnostic_errors) = build_entire_coal_configuration_tree(
        main_yaml_path.to_str().unwrap(),
        schema_directory.to_str().unwrap(),
        virtual_file_system,
        validate,
    )
    .map_err(|error| format!("failed to build: {}", error))?;

    if validate {
        check_diagnostics_match_expected(test_case_directory, &diagnostic_errors)?;
    }

    let expected_output_is_required = !validate;
    check_output_matches_expected(test_case_directory, root_node, expected_output_is_required)?;

    Ok(())
}

fn check_output_matches_expected(
    test_case_directory: &Path,
    root_node: TreeNodeReference,
    required: bool,
) -> Result<(), String> {
    let expected_yaml_path = test_case_directory.join("expected.yaml");

    let expected_content = match fs::read_to_string(&expected_yaml_path) {
        Ok(content) => content,
        Err(_) if !required => return Ok(()),
        Err(_) => return Err("missing 'expected.yaml'".to_string()),
    };

    let expected_value: Value = serde_yaml::from_str(&expected_content)
        .map_err(|error| format!("invalid YAML in expected.yaml: {}", error))?;
    let actual_value = tree_to_yaml(root_node);

    if actual_value != expected_value {
        return Err(format!(
            "output mismatch\nexpected:\n{}\nactual:\n{}",
            serde_yaml::to_string(&expected_value).unwrap(),
            serde_yaml::to_string(&actual_value).unwrap()
        ));
    }
    Ok(())
}

fn check_diagnostics_match_expected(
    test_case_directory: &Path,
    actual_errors: &[DiagnosticError],
) -> Result<(), String> {
    let expected_errors_path = test_case_directory.join("expected_errors.yaml");
    let expected_content = fs::read_to_string(&expected_errors_path)
        .map_err(|_| "missing 'expected_errors.yaml'".to_string())?;

    let expected_errors: Vec<Value> = serde_yaml::from_str(&expected_content)
        .map_err(|error| format!("invalid YAML in expected_errors.yaml: {}", error))?;

    if actual_errors.len() != expected_errors.len() {
        return Err(format!(
            "error count mismatch: expected {}, got {}\nactual errors:\n{}",
            expected_errors.len(),
            actual_errors.len(),
            format_actual_errors(actual_errors)
        ));
    }

    for (index, expected_error) in expected_errors.iter().enumerate() {
        check_single_diagnostic(index, expected_error, &actual_errors[index])?;
    }
    Ok(())
}

fn check_single_diagnostic(
    index: usize,
    expected_error: &Value,
    actual_error: &DiagnosticError,
) -> Result<(), String> {
    let expected_type = string_field(expected_error, "type");
    let expected_path = string_field(expected_error, "path");
    let expected_contains = string_field(expected_error, "contains");

    if actual_error.error_type != expected_type {
        return Err(format!(
            "error #{}: type mismatch: expected '{}', got '{}'",
            index, expected_type, actual_error.error_type
        ));
    }
    if actual_error.field_path != expected_path {
        return Err(format!(
            "error #{}: path mismatch: expected '{}', got '{}'",
            index, expected_path, actual_error.field_path
        ));
    }
    if !actual_error.message.contains(expected_contains) {
        return Err(format!(
            "error #{}: message did not contain '{}': got '{}'",
            index, expected_contains, actual_error.message
        ));
    }
    Ok(())
}

fn string_field<'a>(value: &'a Value, field_name: &str) -> &'a str {
    value
        .get(field_name)
        .and_then(|field| field.as_str())
        .unwrap_or("")
}

fn format_actual_errors(actual_errors: &[DiagnosticError]) -> String {
    if actual_errors.is_empty() {
        return "  (none)".to_string();
    }
    actual_errors
        .iter()
        .map(|error| {
            format!(
                "  type: {} path: {} message: {}",
                error.error_type, error.field_path, error.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// =====================================================================
// TEST ENTRY POINTS
// =====================================================================

#[test]
fn test_all_build_cases() {
    run_directory_tests("tests/build", false);
}

#[test]
fn test_all_validation_cases() {
    run_directory_tests("tests/validation", true);
}
