use crate::coal_tree::{CoalTree, Span};
use crate::type_aliases::TreeNodeReference;
use serde_yaml::Value;

/// A single validation finding produced by scanning the tree after schema
/// application.
#[derive(Debug)]
pub struct DiagnosticError {
    pub error_type: String,
    pub field_path: String,
    pub file_origin: String,
    pub message: String,
    pub span: Option<Span>,
}

/// Walks the entire tree and collects a diagnostic for every schema
/// violation: disallowed fields, missing mandatory fields, type mismatches,
/// and values outside their allowed list.
pub fn generate_diagnostics_for_entire_tree(
    current_node: TreeNodeReference,
    diagnostic_errors: &mut Vec<DiagnosticError>,
) {
    {
        let node_borrowed = current_node.borrow();
        let path = node_borrowed.get_path();
        let origin = node_borrowed
            .origin
            .clone()
            .unwrap_or_else(|| "Unknown Origin".to_string());

        check_schema_definition_valid(&node_borrowed, &path, &origin, diagnostic_errors);
        check_field_allowed(&node_borrowed, &path, &origin, diagnostic_errors);
        check_mandatory_field_present(&node_borrowed, &path, &origin, diagnostic_errors);

        if let Some(ref value) = node_borrowed.value {
            check_value_type(&node_borrowed, value, &path, &origin, diagnostic_errors);
            check_value_allowed(&node_borrowed, value, &path, &origin, diagnostic_errors);
        }
    }

    let children = current_node.borrow().children.clone();
    for child in children {
        generate_diagnostics_for_entire_tree(child, diagnostic_errors);
    }
}

fn check_schema_definition_valid(
    node: &CoalTree,
    path: &str,
    origin: &str,
    diagnostic_errors: &mut Vec<DiagnosticError>,
) {
    if let Some(ref schema_error) = node.schema_error {
        diagnostic_errors.push(create_diagnostic_error(
            "SchemaError",
            path,
            origin,
            format!("schema: {}", schema_error),
            node.span.clone(),
        ));
    }
}

fn check_field_allowed(
    node: &CoalTree,
    path: &str,
    origin: &str,
    diagnostic_errors: &mut Vec<DiagnosticError>,
) {
    if !node.allowed {
        diagnostic_errors.push(create_diagnostic_error(
            "FieldNotAllowed",
            path,
            origin,
            format!(
                "field: {} -> not allowed",
                node.key.as_deref().unwrap_or("")
            ),
            node.span.clone(),
        ));
    }
}

fn check_mandatory_field_present(
    node: &CoalTree,
    path: &str,
    origin: &str,
    diagnostic_errors: &mut Vec<DiagnosticError>,
) {
    if node.value.is_none() && !node.optional {
        let schema_name = node.schema_origin.as_deref().unwrap_or("Unknown Schema");
        let expected_type = node.field_type.as_deref().unwrap_or("any");
        diagnostic_errors.push(create_diagnostic_error(
            "MissingMandatoryField",
            path,
            origin,
            format!(
                "[{}] value: None -> expected {}",
                schema_name, expected_type
            ),
            node.span.clone(),
        ));
    }
}

fn check_value_type(
    node: &CoalTree,
    value: &Value,
    path: &str,
    origin: &str,
    diagnostic_errors: &mut Vec<DiagnosticError>,
) {
    if let Some(ref expected_type) = node.field_type
        && !value_matches_type(value, expected_type)
    {
        diagnostic_errors.push(create_diagnostic_error(
            "InvalidType",
            path,
            origin,
            format!(
                "type: {} -> expected {}",
                describe_value_type(value),
                expected_type
            ),
            node.span.clone(),
        ));
    }
}

fn check_value_allowed(
    node: &CoalTree,
    value: &Value,
    path: &str,
    origin: &str,
    diagnostic_errors: &mut Vec<DiagnosticError>,
) {
    if let Some(ref allowed_values) = node.allowed_values
        && !value_is_in_allowed_list(value, allowed_values)
    {
        diagnostic_errors.push(create_diagnostic_error(
            "ValueNotAllowed",
            path,
            origin,
            format!("value: {:?} -> not in allowed list", value),
            node.span.clone(),
        ));
    }
}

fn value_matches_type(value: &Value, expected_type: &str) -> bool {
    match expected_type {
        "string" => value.is_string() || value.is_number() || value.is_bool(),
        "integer" => value.is_i64() || value.is_u64(),
        "float" => value.is_f64(),
        "number" => value.is_number(),
        "bool" => value.is_bool(),
        "list" => value.is_sequence(),
        "object" => value.is_mapping(),
        "any" => true,
        _ => false,
    }
}

fn value_is_in_allowed_list(value: &Value, allowed_values: &[Value]) -> bool {
    if allowed_values.contains(value) {
        return true;
    }

    let value_text = scalar_value_as_text(value);
    if value_text.is_empty() {
        return false;
    }

    allowed_values
        .iter()
        .any(|allowed| value_text.trim() == scalar_value_as_text(allowed).trim())
}

fn scalar_value_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        _ => String::new(),
    }
}

fn describe_value_type(value: &Value) -> &'static str {
    if value.is_string() {
        "string"
    } else if value.is_i64() || value.is_u64() {
        "integer"
    } else if value.is_f64() {
        "float"
    } else if value.is_bool() {
        "bool"
    } else if value.is_sequence() {
        "list"
    } else if value.is_mapping() {
        "object"
    } else {
        "null"
    }
}

fn create_diagnostic_error(
    error_type: &str,
    path: &str,
    origin: &str,
    message: String,
    span: Option<Span>,
) -> DiagnosticError {
    DiagnosticError {
        error_type: error_type.to_string(),
        field_path: path.to_string(),
        file_origin: origin.to_string(),
        message: format!("[{}] {} {}", origin, path, message),
        span,
    }
}
