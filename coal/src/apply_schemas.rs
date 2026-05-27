use crate::coal_tree::CoalTree;
use crate::shared_logic::{find_target_node_using_reference_path, deep_merge_yaml_values};
use crate::vfs::VirtualFileSystem;
use regex::Regex;
use serde_yaml::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

static REGEX_JINJA_ALLOWED: OnceLock<Regex> = OnceLock::new();

fn get_jinja_regex() -> &'static Regex {
    REGEX_JINJA_ALLOWED.get_or_init(|| Regex::new(r"\{\{\s*([^}]+)\s*\}\}").unwrap())
}

pub fn load_all_schema_files_from_directory(
    schemas_directory_path_string: &str,
    vfs: Arc<Mutex<VirtualFileSystem>>,
) -> Result<HashMap<String, Value>, Box<dyn Error>> {
    let mut raw_schemas_dictionary = HashMap::new();
    let directory_path_object = Path::new(schemas_directory_path_string);

    if directory_path_object.exists() && directory_path_object.is_dir() {
        let directory_entries_iterator = fs::read_dir(directory_path_object)
            .map_err(|e| Box::<dyn Error>::from(format!("Could not read schemas directory '{}': {}", schemas_directory_path_string, e)))?;

        for directory_entry_result in directory_entries_iterator {
            let directory_entry_object = directory_entry_result?;
            let file_path_object = directory_entry_object.path();

            if file_path_object.is_file() {
                let file_extension_option = file_path_object.extension().and_then(|extension| extension.to_str());
                if file_extension_option == Some("schema") || file_extension_option == Some("yaml") {
                    let schema_name_string = file_path_object.file_stem().unwrap().to_str().unwrap().to_string();
                    let file_path_string = file_path_object.to_str().unwrap();

                    let schema_content_string = vfs.lock().unwrap().get_file_content(file_path_string)?;
                    let parsed_yaml_value_object: Value = serde_yaml::from_str(&schema_content_string)
                        .map_err(|e| Box::<dyn Error>::from(format!("Invalid YAML in schema '{}': {}", file_path_string, e)))?;

                    raw_schemas_dictionary.insert(schema_name_string, parsed_yaml_value_object);
                }
            }
        }
    }

    let mut fully_resolved_schemas = HashMap::new();
    for (schema_name, raw_schema_value) in &raw_schemas_dictionary {
        let mut seen_schemas = std::collections::HashSet::new();
        let resolved_schema = resolve_all_schema_mixins_recursively(
            raw_schema_value,
            &raw_schemas_dictionary,
            &mut seen_schemas
        );
        fully_resolved_schemas.insert(schema_name.clone(), resolved_schema);
    }

    Ok(fully_resolved_schemas)
}

fn resolve_all_schema_mixins_recursively(
    val: &Value,
    loaded_schemas: &HashMap<String, Value>,
    seen_schemas: &mut std::collections::HashSet<String>,
) -> Value {
    match val {
        Value::Mapping(map) => {
            let mut resolved_map = serde_yaml::Mapping::new();
            for (k, v) in map {
                resolved_map.insert(k.clone(), resolve_all_schema_mixins_recursively(v, loaded_schemas, seen_schemas));
            }

            if let Some(Value::String(external_name)) = resolved_map.remove(&Value::String("$schema".to_string())) {
                if !seen_schemas.contains(&external_name) {
                    seen_schemas.insert(external_name.clone());

                    let mut base_schema_merged = Value::Mapping(serde_yaml::Mapping::new());
                    if let Some(external_schema) = loaded_schemas.get(&external_name) {
                        base_schema_merged = resolve_all_schema_mixins_recursively(external_schema, loaded_schemas, seen_schemas);
                    }

                    seen_schemas.remove(&external_name);
                    return deep_merge_yaml_values(&base_schema_merged, &Value::Mapping(resolved_map));
                }
            }
            Value::Mapping(resolved_map)
        }
        Value::Sequence(seq) => {
            let mut resolved_seq = Vec::new();
            for item in seq {
                resolved_seq.push(resolve_all_schema_mixins_recursively(item, loaded_schemas, seen_schemas));
            }
            Value::Sequence(resolved_seq)
        }
        _ => val.clone(),
    }
}

pub fn apply_schema_definition_to_tree_node(
    target_tree_node: Rc<RefCell<CoalTree>>,
    schema_definition_value: &Value,
    schema_origin_name_string: &str,
    root_tree_node: Rc<RefCell<CoalTree>>,
    loaded_schemas_dictionary: &HashMap<String, Value>,
    passed_down_declarations_dictionary: &HashMap<String, Value>
) -> Result<(), Box<dyn Error>> {

    {
        let mut target_node_borrowed_mutably = target_tree_node.borrow_mut();
        target_node_borrowed_mutably.schema_origin = Some(schema_origin_name_string.to_string());
        target_node_borrowed_mutably.allowed = false;
    }

    match schema_definition_value {
        Value::String(simple_type_string) => apply_string_schema(
            target_tree_node,
            simple_type_string,
            schema_origin_name_string,
            root_tree_node,
            loaded_schemas_dictionary,
            passed_down_declarations_dictionary
        )?,
        Value::Sequence(schema_sequence_array) => apply_sequence_schema(
            target_tree_node,
            schema_sequence_array,
            schema_origin_name_string,
            root_tree_node,
            loaded_schemas_dictionary,
            passed_down_declarations_dictionary
        )?,
        Value::Mapping(schema_mapping_object) => apply_mapping_schema(
            target_tree_node,
            schema_mapping_object,
            schema_origin_name_string,
            root_tree_node,
            loaded_schemas_dictionary,
            passed_down_declarations_dictionary
        )?,
        _ => {}
    }

    Ok(())
}

fn apply_string_schema(
    target_tree_node: Rc<RefCell<CoalTree>>,
    simple_type_string: &str,
    schema_origin_name_string: &str,
    root_tree_node: Rc<RefCell<CoalTree>>,
    loaded_schemas_dictionary: &HashMap<String, Value>,
    passed_down_declarations_dictionary: &HashMap<String, Value>
) -> Result<(), Box<dyn Error>> {
    let mut target_node_borrowed_mutably = target_tree_node.borrow_mut();

    if simple_type_string.starts_with('[') && simple_type_string.ends_with(']') {
        target_node_borrowed_mutably.field_type = Some("list".to_string());
        target_node_borrowed_mutably.allowed = true;

        let inner_schema_string = &simple_type_string[1..simple_type_string.len() - 1];
        let inner_schema_value = Value::String(inner_schema_string.to_string());

        let children_array = target_node_borrowed_mutably.children.clone();
        drop(target_node_borrowed_mutably);

        for child_node_reference in children_array {
            apply_schema_definition_to_tree_node(
                Rc::clone(&child_node_reference),
                &inner_schema_value,
                schema_origin_name_string,
                Rc::clone(&root_tree_node),
                loaded_schemas_dictionary,
                passed_down_declarations_dictionary
            )?;
        }
    } else if simple_type_string.contains("{{") {
        target_node_borrowed_mutably.field_type = Some("string".to_string());
        target_node_borrowed_mutably.allowed = true;

        drop(target_node_borrowed_mutably);

        if let Some(resolved_values_array) = resolve_jinja_reference_for_allowed_values_if_applicable(
            simple_type_string,
            Rc::clone(&target_tree_node),
            Rc::clone(&root_tree_node)
        ) {
            target_tree_node.borrow_mut().allowed_values = Some(resolved_values_array);
        }
    } else {
        target_node_borrowed_mutably.field_type = Some(simple_type_string.to_string());
        target_node_borrowed_mutably.allowed = true;
    }
    
    Ok(())
}

fn apply_sequence_schema(
    target_tree_node: Rc<RefCell<CoalTree>>,
    schema_sequence_array: &[Value],
    schema_origin_name_string: &str,
    root_tree_node: Rc<RefCell<CoalTree>>,
    loaded_schemas_dictionary: &HashMap<String, Value>,
    passed_down_declarations_dictionary: &HashMap<String, Value>
) -> Result<(), Box<dyn Error>> {
    let mut target_node_borrowed_mutably = target_tree_node.borrow_mut();

    let mut all_elements_are_strings_boolean = true;
    let mut extracted_allowed_values_array = Vec::new();

    for sequence_item_value in schema_sequence_array {
        if let Value::String(string_value) = sequence_item_value {
            extracted_allowed_values_array.push(Value::String(string_value.clone()));
        } else {
            all_elements_are_strings_boolean = false;
            break;
        }
    }

    if all_elements_are_strings_boolean && !schema_sequence_array.is_empty() {
        target_node_borrowed_mutably.field_type = Some("string".to_string());
        target_node_borrowed_mutably.allowed = true;
        target_node_borrowed_mutably.allowed_values = Some(extracted_allowed_values_array);
    } else if schema_sequence_array.len() == 1 && schema_sequence_array[0].is_mapping() {
        target_node_borrowed_mutably.field_type = Some("list".to_string());
        target_node_borrowed_mutably.allowed = true;

        let inner_mapping_schema_value = &schema_sequence_array[0];
        let children_array = target_node_borrowed_mutably.children.clone();
        drop(target_node_borrowed_mutably);

        for child_node_reference in children_array {
            apply_schema_definition_to_tree_node(
                Rc::clone(&child_node_reference),
                inner_mapping_schema_value,
                schema_origin_name_string,
                Rc::clone(&root_tree_node),
                loaded_schemas_dictionary,
                passed_down_declarations_dictionary
            )?;
        }
    }
    
    Ok(())
}

fn apply_mapping_schema(
    target_tree_node: Rc<RefCell<CoalTree>>,
    schema_mapping_object: &serde_yaml::Mapping,
    schema_origin_name_string: &str,
    root_tree_node: Rc<RefCell<CoalTree>>,
    loaded_schemas_dictionary: &HashMap<String, Value>,
    passed_down_declarations_dictionary: &HashMap<String, Value>
) -> Result<(), Box<dyn Error>> {
    if let Some(Value::String(use_target_string)) = schema_mapping_object.get(&Value::String("$use".to_string())) {
        if let Some(declared_subschema_value) = passed_down_declarations_dictionary.get(use_target_string) {
            return apply_schema_definition_to_tree_node(
                Rc::clone(&target_tree_node),
                declared_subschema_value,
                schema_origin_name_string,
                Rc::clone(&root_tree_node),
                loaded_schemas_dictionary,
                passed_down_declarations_dictionary
            );
        }
    }

    {
        let mut target_node_borrowed_mutably = target_tree_node.borrow_mut();
        target_node_borrowed_mutably.field_type = Some("object".to_string());
        target_node_borrowed_mutably.allowed = true;
    }

    let mut allows_any_additional_fields_boolean = false;
    if schema_mapping_object.contains_key(&Value::String("$any".to_string())) {
        allows_any_additional_fields_boolean = true;
    }

    let mut local_declarations_dictionary = passed_down_declarations_dictionary.clone();
    if let Some(Value::Mapping(declare_mapping_object)) = schema_mapping_object.get(&Value::String("$declare".to_string())) {
        for (declare_key_value, declare_schema_value) in declare_mapping_object {
            if let Value::String(declare_key_string) = declare_key_value {
                local_declarations_dictionary.insert(declare_key_string.clone(), declare_schema_value.clone());
            }
        }
    }

    let optional_fields_array = extract_optional_fields_list_from_schema(schema_mapping_object);

    inject_missing_keys_from_schema_mapping(
        schema_mapping_object,
        Rc::clone(&target_tree_node),
        schema_origin_name_string
    );

    let current_children_array = {
        let mut target_node_borrowed_mutably = target_tree_node.borrow_mut();
        for child_node_reference in &target_node_borrowed_mutably.children {
            child_node_reference.borrow_mut().allowed = false;
        }
        target_node_borrowed_mutably.children.clone()
    };

    for child_node_reference in current_children_array {
        let child_key_option = child_node_reference.borrow().key.clone();

        if let Some(child_key_string) = child_key_option {
            let schema_key_value = Value::String(child_key_string.clone());

            let mut found_schema_definition_option = schema_mapping_object.get(&schema_key_value);
            let mut is_optional_boolean = optional_fields_array.contains(&child_key_string);

            if found_schema_definition_option.is_none() {
                if let Some(Value::Mapping(optional_mapping_object)) = schema_mapping_object.get(&Value::String("$optional".to_string())) {
                    if let Some(optional_schema_definition) = optional_mapping_object.get(&schema_key_value) {
                        found_schema_definition_option = Some(optional_schema_definition);
                        is_optional_boolean = true;
                    }
                }
            }

            if let Some(child_schema_definition) = found_schema_definition_option {
                apply_schema_definition_to_tree_node(
                    Rc::clone(&child_node_reference),
                    child_schema_definition,
                    schema_origin_name_string,
                    Rc::clone(&root_tree_node),
                    loaded_schemas_dictionary,
                    &local_declarations_dictionary
                )?;

                let mut child_node_borrowed_mutably = child_node_reference.borrow_mut();
                if is_optional_boolean {
                    child_node_borrowed_mutably.optional = true;
                }
            } else if allows_any_additional_fields_boolean {
                if let Some(any_schema_definition) = schema_mapping_object.get(&Value::String("$any".to_string())) {
                    apply_schema_definition_to_tree_node(
                        Rc::clone(&child_node_reference),
                        any_schema_definition,
                        schema_origin_name_string,
                        Rc::clone(&root_tree_node),
                        loaded_schemas_dictionary,
                        &local_declarations_dictionary
                    )?;
                }
            } else if child_key_string.starts_with('$') {
                child_node_reference.borrow_mut().allowed = true;
            }
        }
    }

    evaluate_advanced_fields_for_schema_mapping(
        schema_mapping_object,
        Rc::clone(&target_tree_node),
        Rc::clone(&root_tree_node),
        schema_origin_name_string
    )?;

    evaluate_conditional_fields_for_schema_mapping(
        schema_mapping_object,
        Rc::clone(&target_tree_node),
        Rc::clone(&root_tree_node),
        schema_origin_name_string,
        loaded_schemas_dictionary,
        &local_declarations_dictionary
    )?;

    Ok(())
}

fn inject_missing_keys_from_schema_mapping(
    schema_mapping_object: &serde_yaml::Mapping,
    target_tree_node: Rc<RefCell<CoalTree>>,
    schema_origin_name_string: &str,
) {
    let mut expected_keys_array = Vec::new();

    for (mapping_key_value, _) in schema_mapping_object {
        if let Value::String(mapping_key_string) = mapping_key_value {
            if !mapping_key_string.starts_with('$') {
                expected_keys_array.push(mapping_key_string.clone());
            }
        }
    }

    if let Some(Value::Mapping(advanced_fields_mapping)) = schema_mapping_object.get(&Value::String("$advanced_fields".to_string())) {
        for (advanced_field_key, _) in advanced_fields_mapping {
            if let Value::String(advanced_field_name_string) = advanced_field_key {
                if !expected_keys_array.contains(advanced_field_name_string) {
                    expected_keys_array.push(advanced_field_name_string.clone());
                }
            }
        }
    }

    let mut target_node_borrowed_mutably = target_tree_node.borrow_mut();
    let parent_span = target_node_borrowed_mutably.span.clone();
    let parent_origin = target_node_borrowed_mutably.origin.clone();

    let existing_keys_array: Vec<String> = target_node_borrowed_mutably.children.iter()
        .filter_map(|child_reference| child_reference.borrow().key.clone())
        .collect();

    for expected_key_string in expected_keys_array {
        if !existing_keys_array.contains(&expected_key_string) {
            let missing_child_tree_node = Rc::new(RefCell::new(CoalTree {
                key: Some(expected_key_string.clone()),
                value: None,
                parent: Some(Rc::downgrade(&target_tree_node)),
                schema_origin: Some(schema_origin_name_string.to_string()),
                origin: parent_origin.clone(),
                optional: false,
                allowed: true,
                span: parent_span.clone(),
                ..Default::default()
            }));
            target_node_borrowed_mutably.children.push(Rc::clone(&missing_child_tree_node));
        }
    }
}

fn extract_optional_fields_list_from_schema(schema_mapping_object: &serde_yaml::Mapping) -> Vec<String> {
    let mut optional_fields_array = Vec::new();
    if let Some(Value::Mapping(optional_mapping)) = schema_mapping_object.get(&Value::String("$optional".to_string())) {
        for (optional_key, _) in optional_mapping {
            if let Value::String(key_string) = optional_key {
                optional_fields_array.push(key_string.clone());
            }
        }
    }
    optional_fields_array
}

fn evaluate_advanced_fields_for_schema_mapping(
    schema_mapping_object: &serde_yaml::Mapping,
    parent_tree_node: Rc<RefCell<CoalTree>>,
    root_tree_node: Rc<RefCell<CoalTree>>,
    schema_origin_name_string: &str
) -> Result<(), Box<dyn Error>> {
    if let Some(Value::Mapping(advanced_fields_mapping)) = schema_mapping_object.get(&Value::String("$advanced_fields".to_string())) {
        let parent_children_array = parent_tree_node.borrow().children.clone();

        for (advanced_field_key, advanced_field_definition) in advanced_fields_mapping {
            if let Value::String(advanced_field_name_string) = advanced_field_key {
                for child_node_reference in &parent_children_array {
                    if child_node_reference.borrow().key.as_deref() == Some(advanced_field_name_string) {

                        let mut child_node_borrowed_mutably = child_node_reference.borrow_mut();
                        child_node_borrowed_mutably.schema_origin = Some(schema_origin_name_string.to_string());
                        child_node_borrowed_mutably.allowed = true;

                        if let Value::Mapping(definition_mapping) = advanced_field_definition {
                            if let Some(Value::String(type_string)) = definition_mapping.get(&Value::String("type".to_string())) {
                                child_node_borrowed_mutably.field_type = Some(type_string.clone());
                            }

                            if let Some(default_value) = definition_mapping.get(&Value::String("default_value".to_string())) {
                                child_node_borrowed_mutably.default_value = Some(default_value.clone());
                                if child_node_borrowed_mutably.value.is_none() {
                                    child_node_borrowed_mutably.value = Some(default_value.clone());
                                }
                            }

                            if let Some(values_definition) = definition_mapping.get(&Value::String("allowed_values".to_string())) {
                                match values_definition {
                                    Value::Sequence(values_sequence_array) => {
                                        child_node_borrowed_mutably.allowed_values = Some(values_sequence_array.clone());
                                    }
                                    Value::String(jinja_reference_string) => {
                                        drop(child_node_borrowed_mutably);

                                        if let Some(resolved_values_array) = resolve_jinja_reference_for_allowed_values_if_applicable(
                                            jinja_reference_string,
                                            Rc::clone(&parent_tree_node),
                                            Rc::clone(&root_tree_node)
                                        ) {
                                            child_node_reference.borrow_mut().allowed_values = Some(resolved_values_array);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn resolve_jinja_reference_for_allowed_values_if_applicable(
    potential_jinja_string: &str,
    current_context_tree_node: Rc<RefCell<CoalTree>>,
    root_tree_node: Rc<RefCell<CoalTree>>
) -> Option<Vec<Value>> {
    let regex_pattern = get_jinja_regex();

    if let Some(regex_capture) = regex_pattern.captures(potential_jinja_string) {
        let reference_path_string = regex_capture[1].trim();
        let mut extracted_allowed_values_array = Vec::new();

        if let Some(found_target_node) = find_target_node_using_reference_path(
            reference_path_string,
            Rc::clone(&current_context_tree_node),
            Rc::clone(&root_tree_node)
        ) {
            let found_node_borrowed = found_target_node.borrow();

            if !found_node_borrowed.children.is_empty() {
                let mut is_array_like_boolean = true;

                for child_reference in &found_node_borrowed.children {
                    if child_reference.borrow().key.as_deref().unwrap_or("").parse::<usize>().is_err() {
                        is_array_like_boolean = false;
                        break;
                    }
                }

                if is_array_like_boolean {
                    for child_reference in &found_node_borrowed.children {
                        if let Some(ref actual_child_value) = child_reference.borrow().value {
                            extracted_allowed_values_array.push(actual_child_value.clone());
                        }
                    }
                } else {
                    for child_reference in &found_node_borrowed.children {
                        if let Some(ref actual_child_key) = child_reference.borrow().key {
                            extracted_allowed_values_array.push(Value::String(actual_child_key.clone()));
                        }
                    }
                }
            } else if let Some(ref raw_fallback_value) = found_node_borrowed.value {
                match raw_fallback_value {
                    Value::Sequence(sequence_array) => {
                        extracted_allowed_values_array = sequence_array.clone();
                    }
                    Value::Mapping(mapping_object) => {
                        for (key_value, _) in mapping_object {
                            extracted_allowed_values_array.push(key_value.clone());
                        }
                    }
                    _ => {}
                }
            }

            if !extracted_allowed_values_array.is_empty() {
                return Some(extracted_allowed_values_array);
            }
        }
    }
    None
}

fn evaluate_conditional_fields_for_schema_mapping(
    schema_mapping_object: &serde_yaml::Mapping,
    parent_tree_node: Rc<RefCell<CoalTree>>,
    root_tree_node: Rc<RefCell<CoalTree>>,
    schema_origin_name_string: &str,
    loaded_schemas_dictionary: &HashMap<String, Value>,
    passed_down_declarations_dictionary: &HashMap<String, Value>
) -> Result<(), Box<dyn Error>> {
    if let Some(Value::Sequence(conditional_fields_sequence_array)) = schema_mapping_object.get(&Value::String("$conditional_fields".to_string())) {
        for conditional_block_value in conditional_fields_sequence_array {
            if let Value::Mapping(conditional_mapping_object) = conditional_block_value {
                if let Some(if_condition_value) = conditional_mapping_object.get(&Value::String("$if".to_string())) {

                    let condition_is_met_boolean = evaluate_logical_condition_block_recursively(
                        if_condition_value,
                        Rc::clone(&parent_tree_node)
                    );

                    if condition_is_met_boolean {
                        if let Some(then_action_value) = conditional_mapping_object.get(&Value::String("$then".to_string())) {
                            if let Value::Mapping(then_mapping_object) = then_action_value {
                                apply_conditional_then_block_to_tree_node(
                                    then_mapping_object,
                                    Rc::clone(&parent_tree_node),
                                    Rc::clone(&root_tree_node),
                                    schema_origin_name_string,
                                    loaded_schemas_dictionary,
                                    passed_down_declarations_dictionary
                                )?;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn evaluate_logical_condition_block_recursively(
    condition_value_object: &Value,
    parent_tree_node: Rc<RefCell<CoalTree>>
) -> bool {
    match condition_value_object {
        Value::Sequence(condition_sequence_array) => {
            for sequence_item_value in condition_sequence_array {
                if !evaluate_logical_condition_block_recursively(sequence_item_value, Rc::clone(&parent_tree_node)) {
                    return false;
                }
            }
            true
        }
        Value::Mapping(condition_mapping_object) => {
            for (condition_key, condition_target_value) in condition_mapping_object {
                if let Value::String(condition_key_string) = condition_key {
                    match condition_key_string.as_str() {
                        "$or" => {
                            if let Value::Sequence(or_sequence_array) = condition_target_value {
                                let mut any_condition_met_boolean = false;
                                for or_sequence_item in or_sequence_array {
                                    if evaluate_logical_condition_block_recursively(or_sequence_item, Rc::clone(&parent_tree_node)) {
                                        any_condition_met_boolean = true;
                                        break;
                                    }
                                }
                                if !any_condition_met_boolean {
                                    return false;
                                }
                            }
                        }
                        "$and" => {
                            if !evaluate_logical_condition_block_recursively(condition_target_value, Rc::clone(&parent_tree_node)) {
                                return false;
                            }
                        }
                        _ => {
                            let mut field_matches_expected_value_boolean = false;
                            let parent_children_node_references_array = parent_tree_node.borrow().children.clone();

                            for child_node_reference in parent_children_node_references_array {
                                let child_node_borrowed = child_node_reference.borrow();
                                if child_node_borrowed.key.as_deref() == Some(condition_key_string.as_str()) {
                                    if let Some(ref child_actual_value) = child_node_borrowed.value {
                                        if child_actual_value == condition_target_value {
                                            field_matches_expected_value_boolean = true;
                                        }
                                    }
                                    break;
                                }
                            }

                            if !field_matches_expected_value_boolean {
                                return false;
                            }
                        }
                    }
                }
            }
            true
        }
        _ => false
    }
}

fn apply_conditional_then_block_to_tree_node(
    then_mapping_object: &serde_yaml::Mapping,
    parent_tree_node: Rc<RefCell<CoalTree>>,
    root_tree_node: Rc<RefCell<CoalTree>>,
    schema_origin_name_string: &str,
    loaded_schemas_dictionary: &HashMap<String, Value>,
    passed_down_declarations_dictionary: &HashMap<String, Value>
) -> Result<(), Box<dyn Error>> {

    inject_missing_keys_from_schema_mapping(
        then_mapping_object,
        Rc::clone(&parent_tree_node),
        schema_origin_name_string
    );

    let parent_children_node_references_array = parent_tree_node.borrow().children.clone();

    // 1. Extract optional fields and check for an $any allowance, just like the main mapping
    let optional_fields_array = extract_optional_fields_list_from_schema(then_mapping_object);
    let allows_any_additional_fields_boolean = then_mapping_object.contains_key(&Value::String("$any".to_string()));

    for child_node_reference in &parent_children_node_references_array {
        let child_key_option = child_node_reference.borrow().key.clone();

        if let Some(child_key_string) = child_key_option {
            let schema_key_value = Value::String(child_key_string.clone());

            let mut found_schema_definition_option = then_mapping_object.get(&schema_key_value);
            let mut is_optional_boolean = optional_fields_array.contains(&child_key_string);

            // 2. Check the $optional block if it wasn't a direct match
            if found_schema_definition_option.is_none() {
                if let Some(Value::Mapping(optional_mapping_object)) = then_mapping_object.get(&Value::String("$optional".to_string())) {
                    if let Some(optional_schema_definition) = optional_mapping_object.get(&schema_key_value) {
                        found_schema_definition_option = Some(optional_schema_definition);
                        is_optional_boolean = true;
                    }
                }
            }

            // 3. Apply the schema if found (either directly or via $optional)
            if let Some(child_schema_definition) = found_schema_definition_option {
                child_node_reference.borrow_mut().allowed = true;

                apply_schema_definition_to_tree_node(
                    Rc::clone(child_node_reference),
                    child_schema_definition,
                    schema_origin_name_string,
                    Rc::clone(&root_tree_node),
                    loaded_schemas_dictionary,
                    passed_down_declarations_dictionary
                )?;

                // Ensure the node's optional flag is set correctly for diagnostics
                if is_optional_boolean {
                    child_node_reference.borrow_mut().optional = true;
                }

            // 4. Fallback to $any if no specific schema matches, but $any is declared
            } else if allows_any_additional_fields_boolean {
                if let Some(any_schema_definition) = then_mapping_object.get(&Value::String("$any".to_string())) {
                    child_node_reference.borrow_mut().allowed = true;

                    apply_schema_definition_to_tree_node(
                        Rc::clone(child_node_reference),
                        any_schema_definition,
                        schema_origin_name_string,
                        Rc::clone(&root_tree_node),
                        loaded_schemas_dictionary,
                        passed_down_declarations_dictionary
                    )?;
                }
            }
        }
    }

    evaluate_advanced_fields_for_schema_mapping(
        then_mapping_object,
        Rc::clone(&parent_tree_node),
        Rc::clone(&root_tree_node),
        schema_origin_name_string
    )?;

    Ok(())
}