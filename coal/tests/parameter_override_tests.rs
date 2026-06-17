use coal::coal_tree::tree_to_yaml;
use coal::parameter_overrides::{ParameterOverride, parse_parameter_override};
use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::type_aliases::new_virtual_file_system_reference;
use coal::virtual_file_system::VirtualFileSystem;
use serde_yaml::Value;

/// Compiles a configuration held entirely in memory (no disk access, no
/// schema) with the given command-line overrides and returns the resolved
/// output as a YAML value.
fn compile_with_overrides(main_yaml: &str, overrides: &[ParameterOverride]) -> Value {
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());
    virtual_file_system
        .lock()
        .unwrap()
        .set_file_content("main.yaml", main_yaml.to_string());

    let (root_node, _) = build_entire_coal_configuration_tree(
        "main.yaml",
        "schema",
        virtual_file_system,
        false,
        overrides,
    )
    .expect("build should succeed");

    tree_to_yaml(root_node)
}

fn expected(yaml: &str) -> Value {
    serde_yaml::from_str(yaml).expect("valid expected YAML")
}

fn override_for(path: &str, value: &str) -> ParameterOverride {
    parse_parameter_override(&format!("{}={}", path, value)).expect("valid override")
}

#[test]
fn pin_overrides_an_existing_value() {
    let output = compile_with_overrides("name: original\n", &[override_for("name", "pinned")]);
    assert_eq!(output, expected("name: pinned\n"));
}

#[test]
fn pin_creates_a_missing_nested_path() {
    let output = compile_with_overrides(
        "environment:\n  region: eu\n",
        &[override_for("environment.paths.config", "/home/config")],
    );
    assert_eq!(
        output,
        expected("environment:\n  region: eu\n  paths:\n    config: /home/config\n")
    );
}

#[test]
fn pin_value_is_parsed_like_a_scalar() {
    let output = compile_with_overrides(
        "settings:\n  placeholder: x\n",
        &[
            override_for("settings.port", "8080"),
            override_for("settings.enabled", "true"),
        ],
    );
    let settings = &output["settings"];
    assert_eq!(settings["port"], Value::Number(8080.into()));
    assert_eq!(settings["enabled"], Value::Bool(true));
}

#[test]
fn jinja_inside_a_pin_is_resolved() {
    // A pin is applied before the Jinja phase, so an expression in its value
    // resolves against the rest of the tree.
    let output = compile_with_overrides(
        "base: hello\ntarget: original\n",
        &[override_for("target", "{{ ROOT.base }}")],
    );
    assert_eq!(output, expected("base: hello\ntarget: hello\n"));
}

#[test]
fn jinja_reference_to_a_pinned_node_sees_the_pinned_value() {
    // A reference elsewhere in the tree must observe the pinned value, not the
    // value originally written in the file.
    let output = compile_with_overrides(
        "base: original\nderived: \"{{ ROOT.base }}\"\n",
        &[override_for("base", "pinned")],
    );
    assert_eq!(output, expected("base: pinned\nderived: pinned\n"));
}

#[test]
fn parameter_behaves_as_a_local_value_under_inheritance() {
    // A seeded parameter acts like a value written in the file: $inherit keeps
    // the local value, so app.port stays the seeded 9999 rather than the
    // inherited 80.
    let output = compile_with_overrides(
        "defaults:\n  port: 80\napp:\n  $inherit: ROOT.defaults\n",
        &[override_for("app.port", "9999")],
    );
    assert_eq!(
        output,
        expected("defaults:\n  port: 80\napp:\n  port: 9999\n")
    );
}

#[test]
fn parameter_is_overridden_by_override_directive_like_a_local_value() {
    // $override deliberately lets the target win over local values. A seeded
    // parameter is treated like any local value, so the target's 80 wins.
    let output = compile_with_overrides(
        "defaults:\n  port: 80\napp:\n  $override: ROOT.defaults\n",
        &[override_for("app.port", "9999")],
    );
    assert_eq!(
        output,
        expected("defaults:\n  port: 80\napp:\n  port: 80\n")
    );
}

#[test]
fn pin_drives_an_inheritance_target() {
    // The $inherit target is built from a Jinja reference to the pinned
    // `environment`, so the pin must be present before the structural phase.
    let output = compile_with_overrides(
        "prod_defaults:\n  replicas: 5\ndev_defaults:\n  replicas: 1\n\
         environment: dev\napp:\n  $inherit: \"ROOT.{{ ROOT.environment }}_defaults\"\n",
        &[override_for("environment", "prod")],
    );
    assert_eq!(output["environment"], Value::String("prod".to_string()));
    assert_eq!(output["app"]["replicas"], Value::Number(5.into()));
}

#[test]
fn pin_drives_an_include_path() {
    // The $include path is built from a Jinja reference to the pinned
    // `environment`, so the pin must be present before the include phase.
    let virtual_file_system = new_virtual_file_system_reference(VirtualFileSystem::new());
    {
        let mut files = virtual_file_system.lock().unwrap();
        files.set_file_content(
            "main.yaml",
            "environment: dev\nactive:\n  $include: \"./{{ ROOT.environment }}.yaml\"\n"
                .to_string(),
        );
        files.set_file_content("prod.yaml", "tier: production\n".to_string());
        files.set_file_content("dev.yaml", "tier: development\n".to_string());
    }

    let (root_node, _) = build_entire_coal_configuration_tree(
        "main.yaml",
        "schema",
        virtual_file_system,
        false,
        &[override_for("environment", "prod")],
    )
    .expect("build should succeed");

    let output = tree_to_yaml(root_node);
    assert_eq!(output["environment"], Value::String("prod".to_string()));
    assert_eq!(
        output["active"]["tier"],
        Value::String("production".to_string())
    );
}

#[test]
fn pin_replaces_an_existing_object_with_a_scalar() {
    let output = compile_with_overrides(
        "service:\n  host: localhost\n  port: 80\n",
        &[override_for("service", "disabled")],
    );
    assert_eq!(output, expected("service: disabled\n"));
}

#[test]
fn multiple_pins_are_all_applied() {
    let output = compile_with_overrides(
        "a: 1\nb: 2\nc: 3\n",
        &[override_for("a", "10"), override_for("c", "30")],
    );
    let expected_value = expected("a: 10\nb: 2\nc: 30\n");
    assert_eq!(output, expected_value);
}

#[test]
fn root_prefix_on_the_path_is_accepted() {
    let output = compile_with_overrides("name: original\n", &[override_for("ROOT.name", "pinned")]);
    assert_eq!(output, expected("name: pinned\n"));
}

#[test]
fn value_may_contain_equals_signs() {
    let parsed = parse_parameter_override("url=http://example.com?a=b&c=d").unwrap();
    assert_eq!(parsed.path_segments, vec!["url".to_string()]);
    assert_eq!(
        parsed.value,
        Value::String("http://example.com?a=b&c=d".to_string())
    );
}

#[test]
fn missing_equals_is_an_error() {
    assert!(parse_parameter_override("noequals").is_err());
}

#[test]
fn empty_path_is_an_error() {
    assert!(parse_parameter_override("=value").is_err());
    assert!(parse_parameter_override("a..b=value").is_err());
}
