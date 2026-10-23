use coal::coal_tree::tree_to_yaml;
use coal::tree_builder::{BuildScope, compile_configurations};
use coal::type_aliases::new_virtual_file_system_reference;
use coal::virtual_file_system::VirtualFileSystem;
use serde_yaml::Value;
use std::fs;
use std::path::{Path, PathBuf};

// The build fixtures are checked by integration_tests.rs through the tree
// builder. `coal` and coal-lsp do not call that: they go through
// `compile_configurations`, which first works out which configurations a
// `_structure.yaml` describes. These tests run the same fixtures down that
// path, so a fixture can no longer pass in the harness and fail in `coal`.
//
// One difference is by design. Where a structure is declared and nothing
// settles the selection, `coal` builds the first configuration the structure
// describes rather than the tree as written, and says which. Such a case is
// expected to come back with a selection; every other case has to produce its
// `_expected.yaml`.

fn build_case_directories() -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = fs::read_dir("tests/build")
        .expect("tests/build exists")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("_expected.yaml").is_file())
        .collect();
    directories.sort();
    assert!(!directories.is_empty(), "tests/build holds no cases");
    directories
}

fn expected_output(directory: &Path) -> Value {
    let text = fs::read_to_string(directory.join("_expected.yaml")).unwrap();
    serde_yaml::from_str(text.trim_start_matches('\u{feff}')).unwrap()
}

fn check_every_case(scope: BuildScope) {
    let mut failures = Vec::new();
    let mut chosen_by_coal = Vec::new();

    for directory in build_case_directories() {
        let name = directory.file_name().unwrap().to_string_lossy().to_string();
        let built = compile_configurations(
            directory.to_str().unwrap(),
            new_virtual_file_system_reference(VirtualFileSystem::new()),
            scope,
            false,
            &[],
            &[],
        );

        let configurations = match built {
            Ok(configurations) => configurations,
            Err(error) => {
                failures.push(format!("--- {} ---\nfailed to build: {}", name, error));
                continue;
            }
        };

        let first = &configurations[0];
        if first.selection.is_some() {
            chosen_by_coal.push(name);
            continue;
        }

        let actual = tree_to_yaml(first.root_node.clone());
        let expected = expected_output(&directory);
        if actual != expected {
            failures.push(format!(
                "--- {} ---\nexpected:\n{}\nactual:\n{}",
                name,
                serde_yaml::to_string(&expected).unwrap(),
                serde_yaml::to_string(&actual).unwrap()
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "\n{} build cases differ when built the way coal builds them ({:?}):\n\n{}\n",
        failures.len(),
        scope,
        failures.join("\n\n")
    );

    // Only a structure with nothing settled leaves the choice to coal.
    assert_eq!(chosen_by_coal, vec!["structure_not_selected".to_string()]);
}

#[test]
fn build_cases_through_the_command_line_path() {
    check_every_case(BuildScope::FirstCombination);
}

#[test]
fn build_cases_through_the_editor_path() {
    check_every_case(BuildScope::EveryCombination);
}
