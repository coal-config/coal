//! Reporting what validating every configuration found.
//!
//! A structure turns one directory into many configurations, so one mistake in
//! a shared file is found once per configuration that reaches it. What the
//! report has to do is say that once, at the place it is written, and say how
//! far it reaches — naming the selection responsible for it where there is
//! one.

use coal::tree_builder::{BuildScope, CompiledConfiguration, compile_configurations};
use coal::type_aliases::new_virtual_file_system_reference;
use coal::validation_report::{GroupedFinding, group_findings};
use coal::virtual_file_system::VirtualFileSystem;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Two nodes and two modes across one environment, so the directory describes
/// four configurations. Everything is sound until a test breaks one part of it.
fn four_configurations(extra: Vec<(&str, &str)>) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = vec![
        (
            "_structure.yaml",
            "selectors:\n  - environments.{environment}.nodes.{node}\n  - \
             environments.{environment}.modes.{mode}\nlayers:\n  - \"node_types.{{ node.node_type \
             }}\"\n  - \"environments.{{ environment }}\"\n  - \"{node}\"\n  - \
             \"{mode}\"\ndrop:\n  - environments\n  - nodes\n  - modes\n",
        ),
        ("node_types/cop.yaml", "role: control\n"),
        ("environments/production/_self.yaml", "region: eu\n"),
        (
            "environments/production/nodes/cop01.yaml",
            "node_type: cop\nworkers: 2\n",
        ),
        (
            "environments/production/nodes/cop02.yaml",
            "node_type: cop\nworkers: 3\n",
        ),
        ("environments/production/modes/operational.yaml", "live: true\n"),
        ("environments/production/modes/test.yaml", "live: false\n"),
        (
            "_schema/_self.yaml",
            "node: string\nmode: string\nnode_type: string\nrole: string\nregion: string\nlive: \
             bool\nworkers: integer\nnode_types:\n  $each:\n    role: string\n",
        ),
    ]
    .into_iter()
    .map(|(path, content)| (path.to_string(), content.to_string()))
    .collect();

    for (path, content) in extra {
        match files.iter_mut().find(|(existing, _)| existing == path) {
            Some(entry) => entry.1 = content.to_string(),
            None => files.push((path.to_string(), content.to_string())),
        }
    }
    files
}

fn configuration_directory(files: &[(String, String)], test_name: &str) -> PathBuf {
    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    let directory = std::env::temp_dir().join(format!(
        "coal-report-{}-{}-{}",
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

fn compile(
    files: &[(String, String)],
    scope: BuildScope,
    test_name: &str,
) -> Vec<CompiledConfiguration> {
    let directory = configuration_directory(files, test_name);
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());

    let compiled = compile_configurations(
        directory.to_str().unwrap(),
        virtual_file_system,
        scope,
        true,
        &[],
        &[],
    )
    .expect("build should succeed");

    let _ = fs::remove_dir_all(&directory);
    compiled
}

fn findings(files: &[(String, String)], test_name: &str) -> Vec<GroupedFinding> {
    group_findings(&compile(files, BuildScope::EveryCombination, test_name))
}

fn finding_about(findings: &[GroupedFinding], text: &str) -> GroupedFinding {
    findings
        .iter()
        .find(|finding| finding.detail.contains(text))
        .unwrap_or_else(|| {
            panic!(
                "expected a finding mentioning '{}', found: {:?}",
                text,
                findings
                    .iter()
                    .map(|finding| finding.detail.clone())
                    .collect::<Vec<String>>()
            )
        })
        .clone()
}

#[test]
fn a_mistake_every_configuration_shares_is_reported_once() {
    // `region` is written in the environment, which every configuration
    // layers, so all four run into the same line of the same file.
    let files = four_configurations(vec![(
        "environments/production/_self.yaml",
        "region:\n  - eu\n",
    )]);
    let found = findings(&files, "shared");

    let finding = finding_about(&found, "expected string");
    assert_eq!(finding.affected.len(), 4);
    assert_eq!(finding.total_configurations, 4);
    assert!(finding.reaches_everything());
    assert_eq!(
        finding.describe_reach(),
        Some("in all 4 configurations".to_string())
    );

    // Once, not once per configuration.
    let occurrences = found
        .iter()
        .filter(|candidate| candidate.detail.contains("expected string"))
        .count();
    assert_eq!(occurrences, 1);
}

#[test]
fn a_finding_is_reported_in_the_file_it_is_written_in() {
    let files = four_configurations(vec![(
        "environments/production/_self.yaml",
        "region:\n  - eu\n",
    )]);
    let finding = finding_about(&findings(&files, "origin"), "expected string");

    assert!(
        finding.file_origin.replace('\\', "/").ends_with("environments/production/_self.yaml"),
        "expected the environment file, got {}",
        finding.file_origin
    );
    assert_eq!(finding.span.expect("a span").line, 1);
}

#[test]
fn a_mistake_one_selection_causes_names_that_selection() {
    // Only the test mode gets a `live` the schema refuses, so exactly the two
    // configurations built with it run into it.
    let files = four_configurations(vec![(
        "environments/production/modes/test.yaml",
        "live: \"no\"\n",
    )]);
    let finding = finding_about(&findings(&files, "one-selector"), "expected bool");

    assert_eq!(finding.affected.len(), 2);
    assert_eq!(
        finding.responsible_selections,
        vec![("mode".to_string(), "test".to_string())]
    );
    assert_eq!(
        finding.describe_reach(),
        Some("in 2 of 4 configurations, whenever mode = test".to_string())
    );
}

#[test]
fn a_mistake_two_selections_cause_names_both_of_them() {
    // The node writes a `workers` the schema refuses and the operational mode
    // writes over it, so only cop02 in test mode is left holding it.
    let files = four_configurations(vec![
        (
            "environments/production/nodes/cop02.yaml",
            "node_type: cop\nworkers: \"many\"\n",
        ),
        (
            "environments/production/modes/operational.yaml",
            "live: true\nworkers: 4\n",
        ),
    ]);
    let finding = finding_about(&findings(&files, "two-selectors"), "expected integer");

    assert_eq!(finding.affected.len(), 1);
    assert_eq!(
        finding.responsible_selections,
        vec![
            ("node".to_string(), "cop02".to_string()),
            ("mode".to_string(), "test".to_string())
        ]
    );
    assert_eq!(
        finding.describe_reach(),
        Some("in 1 of 4 configurations, whenever node = cop02 and mode = test".to_string())
    );
}

#[test]
fn a_finding_names_every_path_the_selections_found_it_under() {
    // The node type is layered into every configuration, and each one reaches
    // the same line under its own path.
    let files = four_configurations(vec![("node_types/cop.yaml", "role:\n  - control\n")]);
    let finding = finding_about(&findings(&files, "paths"), "expected string");

    assert!(
        finding.field_paths.len() >= 2,
        "expected several paths, got {:?}",
        finding.field_paths
    );
    assert!(finding.describe_fields().contains("and"));
}

#[test]
fn a_part_the_structure_drops_is_not_validated() {
    // `environments` is dropped, so the scaffolding under it is not part of
    // any configuration and the schema has nothing to say about it. Without
    // the drop the same field would be refused.
    let sound = four_configurations(vec![]);
    assert!(
        findings(&sound, "dropped").is_empty(),
        "the sound configuration should validate cleanly: {:?}",
        findings(&sound, "dropped")
            .iter()
            .map(|finding| finding.detail.clone())
            .collect::<Vec<String>>()
    );

    let kept: Vec<(String, String)> = sound
        .iter()
        .map(|(path, content)| match path.as_str() {
            "_structure.yaml" => (path.clone(), content.replace("drop:\n  - environments\n", "drop:\n")),
            _ => (path.clone(), content.clone()),
        })
        .collect();
    let found = findings(&kept, "kept");
    assert!(
        found.iter().any(|finding| finding.detail.contains("not allowed")),
        "keeping the scaffolding should surface it: {:?}",
        found
            .iter()
            .map(|finding| finding.detail.clone())
            .collect::<Vec<String>>()
    );
}

#[test]
fn building_without_a_selection_produces_the_first_configuration() {
    let files = four_configurations(vec![]);
    let compiled = compile(&files, BuildScope::FirstCombination, "first");

    assert_eq!(compiled.len(), 1);
    let combination = compiled[0]
        .selection
        .as_ref()
        .expect("the build chose a configuration and should say which");
    assert_eq!(combination.values.len(), 2);
    assert_eq!(compiled[0].describe_selection(), "cop01, operational");
}

#[test]
fn a_configuration_with_no_structure_is_reported_without_a_reach() {
    let files: Vec<(String, String)> = vec![
        ("app.yaml".to_string(), "workers: \"many\"\n".to_string()),
        (
            "_schema/_self.yaml".to_string(),
            "app:\n  workers: integer\n".to_string(),
        ),
    ];
    let found = findings(&files, "no-structure");

    let finding = finding_about(&found, "expected integer");
    assert_eq!(finding.total_configurations, 1);
    assert_eq!(finding.describe_reach(), None);
}
