use crate::coal_tree::{CoalTree, Span};
use serde_yaml::Value;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug)]
pub struct DiagnosticError {
    pub error_type_string: String,
    pub field_path_string: String,
    pub file_origin_string: String,
    pub error_message_string: String,
    pub span_option: Option<Span>,
}

pub fn generate_diagnostics_for_entire_tree(
    current_node: Rc<RefCell<CoalTree>>,
    diagnostic_errors: &mut Vec<DiagnosticError>,
) {
    let node_borrowed = current_node.borrow();
    let path = node_borrowed.get_path();
    let origin = node_borrowed.origin.clone().unwrap_or_else(|| "Unknown Origin".to_string());

    check_field_allowed_status(&node_borrowed, &path, &origin, diagnostic_errors);
    check_mandatory_field_presence(&node_borrowed, &path, &origin, diagnostic_errors);

    if let Some(ref value) = node_borrowed.value {
        validate_constraints(&node_borrowed, value, &path, &origin, diagnostic_errors);
    }

    let children_to_process = node_borrowed.children.clone();
    drop(node_borrowed);

    for child in children_to_process {
        generate_diagnostics_for_entire_tree(child, diagnostic_errors);
    }
}

fn check_field_allowed_status(
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
            format!("field: {} -> not allowed", node.key.as_deref().unwrap_or("")),
            node.span.clone(),
        ));
    }
}

fn check_mandatory_field_presence(
    node: &CoalTree,
    path: &str,
    origin: &str,
    diagnostic_errors: &mut Vec<DiagnosticError>,
) {
    if node.value.is_none() && !node.optional {
        let schema = node.schema_origin.as_deref().unwrap_or("Unknown Schema");
        let expected = node.field_type.as_deref().unwrap_or("any");
        diagnostic_errors.push(create_diagnostic_error(
            "MissingMandatoryField",
            path,
            origin,
            format!("[{}] value: None -> expected {}", schema, expected),
            node.span.clone(),
        ));
    }
}

fn validate_constraints(node: &CoalTree, value: &Value, path: &str, origin: &str, errors: &mut Vec<DiagnosticError>) {
    if let Some(ref expected_type) = node.field_type {
        if !is_type_valid(value, expected_type) {
            errors.push(create_diagnostic_error("InvalidType", path, origin, format!("type: {} -> expected {}", get_type_name(value), expected_type), node.span.clone()));
        }
    }

    if let Some(ref allowed_list) = node.allowed_values {
        if !is_value_allowed(value, allowed_list) {
            errors.push(create_diagnostic_error("ValueNotAllowed", path, origin, format!("value: {:?} -> not in allowed list", value), node.span.clone()));
        }
    }
}

fn is_type_valid(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string() || value.is_number() | value.is_bool(),
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

fn is_value_allowed(value: &Value, allowed_list: &[Value]) -> bool {
    let value_string = format_value(value);
    allowed_list.iter().any(|item| value == item || (!value_string.is_empty() && value_string.trim() == format_value(item).trim()))
}

fn format_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn get_type_name(value: &Value) -> &'static str {
    if value.is_string() { "string" }
    else if value.is_i64() || value.is_u64() { "integer" }
    else if value.is_f64() { "float" }
    else if value.is_bool() { "bool" }
    else if value.is_sequence() { "list" }
    else if value.is_mapping() { "object" }
    else { "null" }
}

fn create_diagnostic_error(error_type: &str, path: &str, origin: &str, message: String, span: Option<Span>) -> DiagnosticError {
    DiagnosticError {
        error_type_string: error_type.to_string(),
        field_path_string: path.to_string(),
        file_origin_string: origin.to_string(),
        error_message_string: format!("[{}] {} {}", origin, path, message),
        span_option: span,
    }
}