//! Regression tests: system directives ('$'-prefixed keys) must never leak
//! into allowed values, no matter which build phase left them in the tree.
//! Allowed values are resolved during validation (before the final directive
//! cleanup), so unprocessed directives — e.g. an unconsumed '$keys', or a
//! mid-edit '$override' while the language server rebuilds on every
//! keystroke — used to show up as completion suggestions.
use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::type_aliases::{TreeNodeReference, new_virtual_file_system_reference};
use coal::virtual_file_system::VirtualFileSystem;
use serde_yaml::Value;
use std::path::Path;
use std::rc::Rc;

fn collect_directive_allowed_values(
    node: TreeNodeReference,
    findings: &mut Vec<(String, Vec<String>)>,
) {
    {
        let node_borrowed = node.borrow();
        if let Some(allowed_values) = &node_borrowed.allowed_values {
            let directive_values: Vec<String> = allowed_values
                .iter()
                .filter_map(|value| match value {
                    Value::String(text) if text.starts_with('$') => Some(text.clone()),
                    _ => None,
                })
                .collect();
            if !directive_values.is_empty() {
                findings.push((node_borrowed.get_path(), directive_values));
            }
        }
    }
    let children = node.borrow().children.clone();
    for child in children {
        collect_directive_allowed_values(child, findings);
    }
}

fn assert_no_directive_allowed_values(project_directory: &Path) {
    let vfs = new_virtual_file_system_reference(VirtualFileSystem::new());

    let (root, _diagnostics) = build_entire_coal_configuration_tree(
        project_directory.to_str().unwrap(),
        vfs,
        true,
        &[],
        &[],
    )
    .expect("project should build");

    let mut findings = Vec::new();
    collect_directive_allowed_values(Rc::clone(&root), &mut findings);

    assert!(
        findings.is_empty(),
        "system directives leaked into allowed values:\n{}",
        findings
            .iter()
            .map(|(path, values)| format!("  {} -> {:?}", path, values))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// A fixture whose referenced mappings still contain unprocessed directives
/// at validation time ('$keys' without '$select', and an allowed-values list
/// carrying a directive-keyed child). None of them may be suggested.
#[test]
fn unprocessed_directives_never_become_allowed_values() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("directives_not_in_allowed_values");
    assert_no_directive_allowed_values(&fixture);
}

/// The full real-world project must stay clean as well.
#[test]
fn project_data_has_no_directive_allowed_values() {
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("project_data");
    if !project.is_dir() {
        return; // repo checkout without the sample project
    }
    assert_no_directive_allowed_values(&project);
}
