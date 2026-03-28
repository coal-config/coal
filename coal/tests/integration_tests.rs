use coal::coal_tree::tree_to_yaml;
use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::vfs::VirtualFileSystem;
use serde_yaml::Value;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

// --- CORE TEST RUNNER ---
fn run_directory_tests(test_suite_dir: &str, validate: bool) {
    let suite_path = Path::new(test_suite_dir);

    // Skip if the user hasn't created the directory yet
    if !suite_path.exists() {
        println!("Skipping: Directory '{}' not found.", test_suite_dir);
        return;
    }

    // Iterate through every subfolder in the suite directory
    for entry in fs::read_dir(suite_path).expect("Failed to read test suite directory") {
        let entry = entry.unwrap();
        let test_case_dir = entry.path();

        if test_case_dir.is_dir() {
            let test_name = test_case_dir.file_name().unwrap().to_str().unwrap();
            println!("Running Test Case: {}", test_name);

            // Dynamically resolve the paths for this specific test folder
            let main_yaml_path = test_case_dir.join("main.yaml").to_str().unwrap().to_string();
            let schemas_dir = test_case_dir.join("schemas").to_str().unwrap().to_string();

            let vfs = Arc::new(Mutex::new(VirtualFileSystem::new()));

            let result = build_entire_coal_configuration_tree(
                &main_yaml_path,
                &schemas_dir,
                vfs,
                validate,
            );


            if !validate {
                let (root, _) = result.unwrap_or_else(|e| panic!("Test '{}' failed to execute: {}", test_name, e));

                let expected_yaml_path = test_case_dir.join("expected.yaml");
                let expected_content = fs::read_to_string(&expected_yaml_path)
                    .unwrap_or_else(|_| panic!("Missing 'expected.yaml' in test '{}'", test_name));

                let expected_value: Value = serde_yaml::from_str(&expected_content)
                    .expect("Invalid YAML in expected.yaml");
                let actual_value = tree_to_yaml(Rc::clone(&root));

                assert_eq!(
                    actual_value, expected_value,
                    "\n[FAILED] Output mismatch in test: {}\n", test_name
                );
                println!("  ✅ [PASSED] {}", test_name);

            } else {
                let (_, errors) = result.unwrap_or_else(|e| panic!("Test '{}' failed to execute: {}", test_name, e));

                let expected_errors_path = test_case_dir.join("expected_errors.yaml");
                let expected_content = fs::read_to_string(&expected_errors_path)
                    .unwrap_or_else(|_| panic!("Missing 'expected_errors.yaml' in test '{}'", test_name));

                let expected_list: Vec<Value> = serde_yaml::from_str(&expected_content)
                    .expect("Invalid YAML in expected_errors.yaml");

                assert_eq!(
                    errors.len(), expected_list.len(),
                    "\n[FAILED] Error count mismatch in test: {}. Expected {}, got {}\n",
                    test_name, expected_list.len(), errors.len()
                );

                for (i, expected_val) in expected_list.iter().enumerate() {
                    let actual = &errors[i];
                    let expected_type = expected_val.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let expected_path = expected_val.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let expected_contains = expected_val.get("contains").and_then(|v| v.as_str()).unwrap_or("");

                    assert_eq!(actual.error_type_string, expected_type, "Type mismatch in {}", test_name);
                    assert_eq!(actual.field_path_string, expected_path, "Path mismatch in {}", test_name);
                    assert!(
                        actual.error_message_string.contains(expected_contains),
                        "Message in {} did not contain '{}'. Got: '{}'",
                        test_name, expected_contains, actual.error_message_string
                    );
                }

                println!("  ✅ [PASSED] {}", test_name);
            }
        }
    }
}

// --- TEST ENTRY POINTS ---

#[test]
fn test_all_build_cases() {
    run_directory_tests("tests/build", false);
}

#[test]
fn test_all_validation_cases() {
    run_directory_tests("tests/validation", true);
}