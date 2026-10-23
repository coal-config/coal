use coal::coal_tree::tree_to_yaml;
use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::type_aliases::new_virtual_file_system_reference;
use coal::virtual_file_system::VirtualFileSystem;
use serde_yaml::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Writes a one-file configuration into a directory of its own and hands back
/// the path.
///
/// A build reads a directory rather than a named entry file, so a test that
/// wants to compile a snippet has to give it somewhere to live. The snippet
/// goes in `_self.yaml`, which is how a directory states its own fields.
fn configuration_directory_holding(configuration: &str, test_name: &str) -> PathBuf {
    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    let directory = std::env::temp_dir().join(format!(
        "coal-test-{}-{}-{}",
        test_name,
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));

    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("temporary directory should be creatable");
    fs::write(directory.join("_self.yaml"), configuration)
        .expect("configuration should be writable");
    directory
}


/// Writes a configuration of several files into a directory of its own.
/// Paths are relative to that directory, so `_definitions/a.yaml` puts a file in
/// the include namespace.
fn configuration_directory_with(files: &[(&str, &str)], test_name: &str) -> PathBuf {
    let directory = configuration_directory_holding("{}\n", test_name);

    for (relative_path, content) in files {
        let file_path = directory.join(relative_path);
        fs::create_dir_all(file_path.parent().unwrap()).expect("parent should be creatable");
        fs::write(file_path, content).expect("file should be writable");
    }
    directory
}

/// Compiles a single-file configuration (no schema) and returns either the
/// resolved output or the fatal error message.
fn compile(main_yaml: &str) -> Result<Value, String> {
    let directory = configuration_directory_holding(main_yaml, "loop");
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());

    let result =
        build_entire_coal_configuration_tree(directory.to_str().unwrap(), virtual_file_system, false, &[], &[])
            .map(|(root_node, _)| tree_to_yaml(root_node))
            .map_err(|error| error.to_string());

    let _ = fs::remove_dir_all(&directory);
    result
}

// =====================================================================
// Jinja cycles are non-fatal: the loop is broken and the template left as-is.
// =====================================================================

#[test]
fn jinja_self_reference_is_left_unresolved_without_hanging() {
    let output =
        compile("a: \"{{ a }}\"\nok: hello\n").expect("a self-reference must not be fatal");

    assert_eq!(output["a"], Value::String("{{ a }}".to_string()));
    assert_eq!(output["ok"], Value::String("hello".to_string()));
}

#[test]
fn jinja_mutual_reference_is_left_unresolved_without_hanging() {
    // This shape previously overflowed the stack. The build must terminate and
    // leave unresolved templates behind rather than loop forever.
    let output =
        compile("b: \"{{ c }}\"\nc: \"{{ b }}\"\n").expect("a mutual reference must not be fatal");

    assert!(matches!(&output["b"], Value::String(text) if text.contains("{{")));
    assert!(matches!(&output["c"], Value::String(text) if text.contains("{{")));
}

// =====================================================================
// Structural cycles are fatal: they cannot produce output and would otherwise
// recurse or loop forever.
// =====================================================================

#[test]
fn extend_cycle_is_a_fatal_error() {
    let error = compile("a:\n  $extend: b\nb:\n  $extend: a\n")
        .expect_err("an $extend cycle must be fatal");
    assert!(error.contains("cycle"), "unexpected error: {}", error);
}

#[test]
fn include_cycle_is_a_fatal_error() {
    let directory = configuration_directory_with(
        &[
            ("_self.yaml", "x:\n  $include: b\n"),
            ("_definitions/b.yaml", "y:\n  $include: a\n"),
            ("_definitions/a.yaml", "z:\n  $include: b\n"),
        ],
        "include-cycle",
    );
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());

    let result = build_entire_coal_configuration_tree(
        directory.to_str().unwrap(),
        virtual_file_system,
        false,
        &[],
        &[],
    );
    let _ = fs::remove_dir_all(&directory);
    let error = result
        .err()
        .expect("an $include cycle must be fatal")
        .to_string();
    assert!(
        error.contains("$include cycle"),
        "unexpected error: {}",
        error
    );
}
