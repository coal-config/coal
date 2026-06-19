use coal::coal_tree::tree_to_yaml;
use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::type_aliases::new_virtual_file_system_reference;
use coal::virtual_file_system::VirtualFileSystem;
use serde_yaml::Value;

/// Compiles a single in-memory `main.yaml` (no schema) and returns either the
/// resolved output or the fatal error message.
fn compile(main_yaml: &str) -> Result<Value, String> {
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());
    virtual_file_system
        .lock()
        .unwrap()
        .set_file_content("main.yaml", main_yaml.to_string());

    build_entire_coal_configuration_tree("main.yaml", "schema", virtual_file_system, false, &[])
        .map(|(root_node, _)| tree_to_yaml(root_node))
        .map_err(|error| error.to_string())
}

// =====================================================================
// Jinja cycles are non-fatal: the loop is broken and the template left as-is.
// =====================================================================

#[test]
fn jinja_self_reference_is_left_unresolved_without_hanging() {
    let output =
        compile("a: \"{{ ROOT.a }}\"\nok: hello\n").expect("a self-reference must not be fatal");

    assert_eq!(output["a"], Value::String("{{ ROOT.a }}".to_string()));
    assert_eq!(output["ok"], Value::String("hello".to_string()));
}

#[test]
fn jinja_mutual_reference_is_left_unresolved_without_hanging() {
    // This shape previously overflowed the stack. The build must terminate and
    // leave unresolved templates behind rather than loop forever.
    let output = compile("b: \"{{ ROOT.c }}\"\nc: \"{{ ROOT.b }}\"\n")
        .expect("a mutual reference must not be fatal");

    assert!(matches!(&output["b"], Value::String(text) if text.contains("{{")));
    assert!(matches!(&output["c"], Value::String(text) if text.contains("{{")));
}

// =====================================================================
// Structural cycles are fatal: they cannot produce output and would otherwise
// recurse or loop forever.
// =====================================================================

#[test]
fn inherit_cycle_is_a_fatal_error() {
    let error = compile("a:\n  $inherit: ROOT.b\nb:\n  $inherit: ROOT.a\n")
        .expect_err("an $inherit cycle must be fatal");
    assert!(error.contains("cycle"), "unexpected error: {}", error);
}

#[test]
fn override_cycle_is_a_fatal_error() {
    let error = compile("a:\n  $override: ROOT.b\nb:\n  $override: ROOT.a\n")
        .expect_err("an $override cycle must be fatal");
    assert!(error.contains("cycle"), "unexpected error: {}", error);
}

#[test]
fn extend_cycle_is_a_fatal_error() {
    let error = compile("a:\n  $extend: ROOT.b\nb:\n  $extend: ROOT.a\n")
        .expect_err("an $extend cycle must be fatal");
    assert!(error.contains("cycle"), "unexpected error: {}", error);
}

#[test]
fn include_cycle_is_a_fatal_error() {
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());
    {
        let mut files = virtual_file_system.lock().unwrap();
        files.set_file_content("main.yaml", "x:\n  $include: \"./b.yaml\"\n".to_string());
        files.set_file_content("b.yaml", "y:\n  $include: \"./a.yaml\"\n".to_string());
        files.set_file_content("a.yaml", "z:\n  $include: \"./b.yaml\"\n".to_string());
    }

    let result = build_entire_coal_configuration_tree(
        "main.yaml",
        "schema",
        virtual_file_system,
        false,
        &[],
    );
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
