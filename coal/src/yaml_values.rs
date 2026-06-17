use marked_yaml::Node;
use serde_yaml::Value;

/// Converts a marked YAML node into a plain serde_yaml value, parsing scalar
/// text into booleans and numbers where possible.
pub fn convert_marked_yaml_node_to_value(node: &Node) -> Value {
    match node {
        Node::Scalar(scalar) => parse_scalar_value(scalar.as_str()),
        Node::Sequence(sequence) => Value::Sequence(
            sequence
                .iter()
                .map(convert_marked_yaml_node_to_value)
                .collect(),
        ),
        Node::Mapping(mapping) => {
            let mut converted_mapping = serde_yaml::Mapping::new();
            for (key, value) in mapping.iter() {
                converted_mapping.insert(
                    Value::String(key.as_str().to_string()),
                    convert_marked_yaml_node_to_value(value),
                );
            }
            Value::Mapping(converted_mapping)
        }
    }
}

/// Parses a scalar string into a typed value: a boolean, then an integer,
/// then a float, otherwise the original string. Text such as a Jinja
/// expression (`{{ ... }}`) is none of those and is preserved as a string so
/// it can be resolved later.
pub fn parse_scalar_value(text: &str) -> Value {
    if let Ok(boolean) = text.parse::<bool>() {
        Value::Bool(boolean)
    } else if let Ok(integer) = text.parse::<i64>() {
        Value::Number(integer.into())
    } else if let Ok(float) = text.parse::<f64>() {
        Value::Number(float.into())
    } else {
        Value::String(text.to_string())
    }
}

/// Replaces every "$filename" placeholder inside the value with the stem of
/// the file the value originated from.
pub fn replace_filename_placeholder_in_value(value: &mut Value, file_stem: &str) {
    if file_stem.is_empty() {
        return;
    }
    match value {
        Value::String(text) => {
            *text = text.replace("$filename", file_stem);
        }
        Value::Sequence(sequence) => {
            for item in sequence {
                replace_filename_placeholder_in_value(item, file_stem);
            }
        }
        Value::Mapping(mapping) => {
            for (_, nested_value) in mapping {
                replace_filename_placeholder_in_value(nested_value, file_stem);
            }
        }
        _ => {}
    }
}

/// Recursively merges two YAML values. Mapping entries from the override win
/// on conflict; any non-mapping override replaces the base value entirely.
pub fn deep_merge_yaml_values(base: &Value, override_value: &Value) -> Value {
    if let (Value::Mapping(base_mapping), Value::Mapping(override_mapping)) = (base, override_value)
    {
        let mut merged_mapping = base_mapping.clone();
        for (key, value) in override_mapping {
            if let Some(base_entry) = merged_mapping.get_mut(key) {
                *base_entry = deep_merge_yaml_values(base_entry, value);
            } else {
                merged_mapping.insert(key.clone(), value.clone());
            }
        }
        Value::Mapping(merged_mapping)
    } else {
        override_value.clone()
    }
}

/// Extracts a list of strings from a value that is either a single string or
/// a sequence of strings. Used by directives that accept one or many paths.
pub fn extract_string_list_from_value(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Sequence(sequence) => sequence
            .iter()
            .filter_map(|item| item.as_str().map(|text| text.to_string()))
            .collect(),
        _ => vec![],
    }
}

/// Sorts all mapping keys inside a YAML value alphabetically, recursing into
/// nested mappings and sequences.
pub fn sort_value_mapping_keys_recursively(value: &mut Value) {
    match value {
        Value::Mapping(mapping) => {
            let mut entries: Vec<(Value, Value)> = std::mem::take(mapping).into_iter().collect();
            entries.sort_by(|left, right| {
                yaml_key_as_string(&left.0).cmp(&yaml_key_as_string(&right.0))
            });

            for (key, mut entry_value) in entries {
                sort_value_mapping_keys_recursively(&mut entry_value);
                mapping.insert(key, entry_value);
            }
        }
        Value::Sequence(sequence) => {
            for item in sequence.iter_mut() {
                sort_value_mapping_keys_recursively(item);
            }
        }
        _ => {}
    }
}

fn yaml_key_as_string(key: &Value) -> String {
    match key {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        _ => String::new(),
    }
}

/// Removes every mapping entry whose key starts with '$' from the value,
/// recursing into nested mappings and sequences. Used during final cleanup so
/// internal system directives never appear in the compiled output.
pub fn strip_system_directive_keys_from_value(value: &mut Value) {
    match value {
        Value::Mapping(mapping) => {
            let directive_keys: Vec<Value> = mapping
                .keys()
                .filter(|key| matches!(key, Value::String(text) if text.starts_with('$')))
                .cloned()
                .collect();
            for key in directive_keys {
                mapping.remove(&key);
            }
            for (_, nested_value) in mapping.iter_mut() {
                strip_system_directive_keys_from_value(nested_value);
            }
        }
        Value::Sequence(sequence) => {
            for item in sequence.iter_mut() {
                strip_system_directive_keys_from_value(item);
            }
        }
        _ => {}
    }
}
