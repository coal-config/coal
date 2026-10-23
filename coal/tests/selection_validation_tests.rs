//! Validating a configuration that describes many things.
//!
//! A structure turns one directory into as many configurations as it has
//! selections, and a schema describes what comes out of a selection rather
//! than what went in. Checking one of them therefore says nothing about the
//! others, so `--validate` with nothing selected has to check them all.

use coal::parameter_overrides::parse_parameter_override;
use coal::tree_builder::{BuildScope, CompiledConfiguration, compile_configurations};
use coal::type_aliases::new_virtual_file_system_reference;
use coal::virtual_file_system::VirtualFileSystem;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A configuration of several nodes across two environments. `cop02` is given
/// a `workers` the schema will refuse; every other node is sound.
fn node_configuration(staging_workers: &str) -> Vec<(String, String)> {
    vec![
        (
            "_structure.yaml".to_string(),
            "selectors:\n  - \"environments.{environment}.nodes.{node}\"\nlayers:\n  - \"node_types.{{ node.node_type }}\"\n  - \"environments.{{ environment }}\"\ndrop:\n  - environments\n  - nodes\n"
                .to_string(),
        ),
        (
            "node_types.yaml".to_string(),
            "cop:\n  role: control\n  workers: 2\n".to_string(),
        ),
        (
            "environments/production/_self.yaml".to_string(),
            "region: eu\n".to_string(),
        ),
        (
            "environments/production/nodes/cop01.yaml".to_string(),
            "node_type: cop\n".to_string(),
        ),
        (
            "environments/staging/_self.yaml".to_string(),
            format!("region: eu\nworkers: {}\n", staging_workers),
        ),
        (
            "environments/staging/nodes/cop02.yaml".to_string(),
            "node_type: cop\n".to_string(),
        ),
        (
            "environments/staging/nodes/cop03.yaml".to_string(),
            "node_type: cop\n".to_string(),
        ),
        (
            "_schema/_self.yaml".to_string(),
            "node: string\nnode_type: string\nrole: string\nregion: string\nworkers: integer\n\
             node_types:\n  $each:\n    role: string\n    workers: integer\n"
                .to_string(),
        ),
    ]
}

fn configuration_directory(files: &[(String, String)], test_name: &str) -> PathBuf {
    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    let directory = std::env::temp_dir().join(format!(
        "coal-select-{}-{}-{}",
        test_name,
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&directory);

    for (relative_path, content) in files {
        let file_path = directory.join(relative_path);
        fs::create_dir_all(file_path.parent().unwrap()).expect("parent should be creatable");
        fs::write(file_path, content).expect("file should be writable");
    }
    directory
}

fn validate_everything(files: &[(String, String)], test_name: &str) -> Vec<CompiledConfiguration> {
    let directory = configuration_directory(files, test_name);
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());

    let compiled = compile_configurations(
        directory.to_str().unwrap(),
        virtual_file_system,
        BuildScope::EveryCombination,
        true,
        &[],
        &[],
    )
    .expect("build should succeed");

    let _ = fs::remove_dir_all(&directory);
    compiled
}

fn selections_with_errors(compiled: &[CompiledConfiguration]) -> Vec<String> {
    compiled
        .iter()
        .filter(|configuration| !configuration.diagnostics.is_empty())
        .map(|configuration| configuration.describe_selection())
        .collect()
}

/// A layer that names a root field the configuration does not have can never
/// contribute anything, however the build is run. Skipping it the way an
/// absent corner is skipped leaves the configuration silently short of
/// whatever that layer was meant to bring down.
#[test]
fn a_layer_naming_a_field_the_configuration_lacks_is_reported() {
    let mut files = node_configuration("4");
    files[0].1 = files[0]
        .1
        .replace("environments.{{ environment }}", "environment.{{ environment }}");

    let compiled = validate_everything(&files, "missing-layer-root");

    assert_eq!(
        selections_with_errors(&compiled).len(),
        compiled.len(),
        "a layer that can never contribute is a fault in every configuration it is used for"
    );

    let message = compiled[0].diagnostics[0].message.clone();
    assert!(
        message.contains("reads 'environment'") && message.contains("Did you mean 'environments'?"),
        "the report should name the field and the near miss, got: {}",
        message
    );
}

/// The same name in two environments is two configurations, not one, and a
/// second selector multiplies them. Every pairing that can actually be built
/// has to be checked, and none that cannot.
#[test]
fn every_combination_of_two_selectors_is_checked() {
    let files = vec![
        ("_structure.yaml".to_string(), "selectors:\n  - \"environments.{environment}.nodes.{node}\"\n  - \"environments.{environment}.modes.{mode}\"\nlayers:\n  - \"environments.{{ environment }}\"\n  - \"{node}\"\n  - \"{mode}\"\ndrop: environments\n".to_string()),
        (
            "environments/production/nodes/cop01.yaml".to_string(),
            "role: control
".to_string(),
        ),
        (
            "environments/production/modes/operational.yaml".to_string(),
            "workers: 12
".to_string(),
        ),
        (
            "environments/production/modes/maintenance.yaml".to_string(),
            "workers: 1
".to_string(),
        ),
        (
            "environments/staging/nodes/cop01.yaml".to_string(),
            "role: control
".to_string(),
        ),
        (
            "environments/staging/modes/operational.yaml".to_string(),
            "workers: 3
".to_string(),
        ),
        (
            "_schema/_self.yaml".to_string(),
            "role: string
workers: integer
node: string
mode: string
environment: string
$optional:
  nodes:
    $each:
      role: string
  modes:
    $each:
      workers: integer
"
                .to_string(),
        ),
    ];

    let compiled = validate_everything(&files, "two-selectors");

    let mut named: Vec<String> = compiled
        .iter()
        .map(|configuration| configuration.describe_selection())
        .collect();
    named.sort();

    // production has two modes and staging one, and cop01 exists in both — so
    // three pairings, each named by as much of its path as it takes to tell it
    // from the others.
    assert_eq!(
        named,
        vec![
            "environments.production.nodes.cop01, environments.production.modes.operational"
                .to_string(),
            "environments.production.nodes.cop01, maintenance".to_string(),
            "environments.staging.nodes.cop01, environments.staging.modes.operational".to_string(),
        ]
    );
    assert!(
        selections_with_errors(&compiled).is_empty(),
        "every pairing should build cleanly, got {:?}",
        selections_with_errors(&compiled)
    );
}

#[test]
fn every_selection_is_compiled_and_checked() {
    let compiled = validate_everything(&node_configuration("4"), "all");

    let mut selections: Vec<String> = compiled
        .iter()
        .map(|configuration| configuration.describe_selection())
        .collect();
    selections.sort();

    assert_eq!(selections, vec!["cop01", "cop02", "cop03"]);
    assert!(
        selections_with_errors(&compiled).is_empty(),
        "a sound configuration must validate for every node"
    );
}

#[test]
fn only_the_configurations_a_mistake_reaches_are_reported() {
    // The bad value lives in the staging environment, which cop02 and cop03
    // both inherit and cop01 does not.
    let compiled = validate_everything(&node_configuration("\"lots\""), "one-bad");

    let mut failing = selections_with_errors(&compiled);
    failing.sort();
    assert_eq!(failing, vec!["cop02", "cop03"]);
}

#[test]
fn a_finding_names_the_file_the_mistake_is_in() {
    let compiled = validate_everything(&node_configuration("\"lots\""), "origin");

    let failing = compiled
        .iter()
        .find(|configuration| configuration.describe_selection() == "cop02")
        .expect("cop02 should have been compiled");

    let origin = &failing.diagnostics[0].file_origin;
    assert!(
        origin.replace('\\', "/").ends_with("environments/staging/_self.yaml"),
        "the finding should point at the environment file the value came from, got '{}'",
        origin
    );
}

#[test]
fn a_pinned_capture_narrows_what_is_checked() {
    // Pinning the environment leaves only the nodes inside it to check.
    let directory = configuration_directory(&node_configuration("4"), "pinned");
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());

    let compiled = compile_configurations(
        directory.to_str().unwrap(),
        virtual_file_system,
        BuildScope::EveryCombination,
        true,
        &[parse_parameter_override("environment=production").unwrap()],
        &[],
    )
    .expect("build should succeed");
    let _ = fs::remove_dir_all(&directory);

    let selections: Vec<String> = compiled
        .iter()
        .map(|configuration| configuration.describe_selection())
        .collect();
    assert_eq!(selections, vec!["cop01"]);
}

#[test]
fn selecting_one_configuration_checks_only_that_one() {
    use coal::tree_builder::SelectionArgument;

    let directory = configuration_directory(&node_configuration("\"lots\""), "single");
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());

    let compiled = compile_configurations(
        directory.to_str().unwrap(),
        virtual_file_system,
        BuildScope::EveryCombination,
        true,
        &[],
        &[SelectionArgument {
            name: None,
            value: "cop01".to_string(),
        }],
    )
    .expect("build should succeed");
    let _ = fs::remove_dir_all(&directory);

    assert_eq!(compiled.len(), 1);
    assert!(
        compiled[0].diagnostics.is_empty(),
        "cop01 does not inherit the staging mistake: {:?}",
        compiled[0].diagnostics
    );
}
