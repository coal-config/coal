use crate::coal_tree::CoalTree;
use crate::reference_resolution::{find_target_node_using_reference_path, jinja_expression_regex};
use crate::type_aliases::{TreeNodeReference, VirtualFileSystemReference, new_tree_node_reference};
use crate::yaml_values::deep_merge_yaml_values;
use serde_yaml::{Mapping, Value};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::Path;
use std::rc::Rc;

/// Schema definitions keyed by schema name (the file stem).
pub type SchemaCollection = HashMap<String, Value>;

/// Reusable sub-schemas declared with "$declare", keyed by declaration name.
type SchemaDeclarations = HashMap<String, Value>;

// =====================================================================
// SCHEMA LOADING
// =====================================================================

/// Loads every ".schema" and ".yaml" file from the schema directory, then
/// resolves all "$schema" mixins so each entry is a fully merged definition.
pub fn load_all_schema_files_from_directory(
    schema_directory_path: &str,
    virtual_file_system: VirtualFileSystemReference,
) -> Result<SchemaCollection, Box<dyn Error>> {
    let raw_schemas = read_raw_schema_files(schema_directory_path, virtual_file_system)?;

    let mut resolved_schemas = SchemaCollection::new();
    for (schema_name, raw_schema) in &raw_schemas {
        let mut visited_schema_names = HashSet::new();
        let resolved_schema =
            resolve_schema_mixins_recursively(raw_schema, &raw_schemas, &mut visited_schema_names);
        resolved_schemas.insert(schema_name.clone(), resolved_schema);
    }

    Ok(resolved_schemas)
}

fn read_raw_schema_files(
    schema_directory_path: &str,
    virtual_file_system: VirtualFileSystemReference,
) -> Result<SchemaCollection, Box<dyn Error>> {
    let mut raw_schemas = SchemaCollection::new();
    let directory_path = Path::new(schema_directory_path);

    if !directory_path.is_dir() {
        return Ok(raw_schemas);
    }

    let directory_entries = fs::read_dir(directory_path).map_err(|error| {
        Box::<dyn Error>::from(format!(
            "Could not read schemas directory '{}': {}",
            schema_directory_path, error
        ))
    })?;

    for entry in directory_entries {
        let file_path = entry?.path();
        if !file_path.is_file() || !is_schema_file(&file_path) {
            continue;
        }

        let schema_name = file_path.file_stem().unwrap().to_str().unwrap().to_string();
        let file_path_text = file_path.to_str().unwrap();

        let schema_content = virtual_file_system
            .lock()
            .unwrap()
            .get_file_content(file_path_text)?;
        let parsed_schema: Value = serde_yaml::from_str(&schema_content).map_err(|error| {
            Box::<dyn Error>::from(format!(
                "Invalid YAML in schema '{}': {}",
                file_path_text, error
            ))
        })?;

        raw_schemas.insert(schema_name, parsed_schema);
    }

    Ok(raw_schemas)
}

fn is_schema_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("schema") | Some("yaml")
    )
}

/// Deep-merges every "$schema" mixin reference into the schema that declares
/// it. The visited set breaks circular mixin chains.
fn resolve_schema_mixins_recursively(
    schema_value: &Value,
    loaded_schemas: &SchemaCollection,
    visited_schema_names: &mut HashSet<String>,
) -> Value {
    match schema_value {
        Value::Mapping(mapping) => {
            let mut resolved_mapping = Mapping::new();
            for (key, value) in mapping {
                resolved_mapping.insert(
                    key.clone(),
                    resolve_schema_mixins_recursively(value, loaded_schemas, visited_schema_names),
                );
            }

            if let Some(Value::String(mixin_name)) = resolved_mapping.remove("$schema")
                && !visited_schema_names.contains(&mixin_name)
            {
                visited_schema_names.insert(mixin_name.clone());

                let mut mixin_schema = Value::Mapping(Mapping::new());
                if let Some(referenced_schema) = loaded_schemas.get(&mixin_name) {
                    mixin_schema = resolve_schema_mixins_recursively(
                        referenced_schema,
                        loaded_schemas,
                        visited_schema_names,
                    );
                }

                visited_schema_names.remove(&mixin_name);
                return deep_merge_yaml_values(&mixin_schema, &Value::Mapping(resolved_mapping));
            }
            Value::Mapping(resolved_mapping)
        }
        Value::Sequence(sequence) => Value::Sequence(
            sequence
                .iter()
                .map(|item| {
                    resolve_schema_mixins_recursively(item, loaded_schemas, visited_schema_names)
                })
                .collect(),
        ),
        _ => schema_value.clone(),
    }
}

// =====================================================================
// SCHEMA APPLICATION ENTRY POINT
// =====================================================================

/// Applies a schema definition to a tree node, marking the node's expected
/// type, optionality, allowed values, and defaults so the diagnostics pass
/// can report violations.
pub fn apply_schema_definition_to_tree_node(
    target_node: TreeNodeReference,
    schema_definition: &Value,
    schema_origin_name: &str,
    root_node: TreeNodeReference,
    loaded_schemas: &SchemaCollection,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.schema_origin = Some(schema_origin_name.to_string());
        target_borrowed.allowed = false;
    }

    match schema_definition {
        Value::String(type_text) => apply_scalar_type_schema(
            target_node,
            type_text,
            schema_origin_name,
            root_node,
            loaded_schemas,
            declarations,
        ),
        Value::Sequence(schema_sequence) => apply_sequence_schema(
            target_node,
            schema_sequence,
            schema_origin_name,
            root_node,
            loaded_schemas,
            declarations,
        ),
        Value::Mapping(schema_mapping) => apply_mapping_schema(
            target_node,
            schema_mapping,
            schema_origin_name,
            root_node,
            loaded_schemas,
            declarations,
        ),
        _ => {
            mark_schema_error(
                &target_node,
                "invalid schema definition -> expected a type name, allowed-value list, or mapping"
                    .to_string(),
            );
            Ok(())
        }
    }
}

/// Records a problem with the schema definition on the node. The field stays
/// allowed (the configuration itself is not at fault) and the problem is
/// reported as a "SchemaError" diagnostic.
fn mark_schema_error(target_node: &TreeNodeReference, schema_error: String) {
    let mut target_borrowed = target_node.borrow_mut();
    target_borrowed.allowed = true;
    target_borrowed.schema_error = Some(schema_error);
}

/// Type names a scalar schema definition may use.
const KNOWN_TYPE_NAMES: [&str; 8] = [
    "string", "integer", "float", "number", "bool", "list", "object", "any",
];

fn is_known_type_name(type_text: &str) -> bool {
    KNOWN_TYPE_NAMES.contains(&type_text)
}

// =====================================================================
// SCALAR TYPE SCHEMAS ("string", "[integer]", "{{ ROOT.clusters }}")
// =====================================================================

fn apply_scalar_type_schema(
    target_node: TreeNodeReference,
    type_text: &str,
    schema_origin_name: &str,
    root_node: TreeNodeReference,
    loaded_schemas: &SchemaCollection,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    if type_text.starts_with('[') && type_text.ends_with(']') {
        apply_bracketed_list_schema(
            target_node,
            type_text,
            schema_origin_name,
            root_node,
            loaded_schemas,
            declarations,
        )
    } else if type_text.contains("{{") {
        apply_jinja_allowed_values_schema(target_node, type_text, root_node);
        Ok(())
    } else if is_known_type_name(type_text) {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some(type_text.to_string());
        target_borrowed.allowed = true;
        Ok(())
    } else {
        mark_schema_error(
            &target_node,
            format!(
                "unknown type '{}' -> expected one of {:?}",
                type_text, KNOWN_TYPE_NAMES
            ),
        );
        Ok(())
    }
}

/// Handles the "[element_type]" form: the node itself must be a list, and
/// the inner type is applied to every element.
fn apply_bracketed_list_schema(
    target_node: TreeNodeReference,
    type_text: &str,
    schema_origin_name: &str,
    root_node: TreeNodeReference,
    loaded_schemas: &SchemaCollection,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    let children = {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some("list".to_string());
        target_borrowed.allowed = true;
        target_borrowed.children.clone()
    };

    let element_type_text = &type_text[1..type_text.len() - 1];
    let element_schema = Value::String(element_type_text.to_string());

    for child in children {
        apply_schema_definition_to_tree_node(
            child,
            &element_schema,
            schema_origin_name,
            Rc::clone(&root_node),
            loaded_schemas,
            declarations,
        )?;
    }
    Ok(())
}

/// Handles the "{{ reference }}" form: the node is a string whose allowed
/// values come from the referenced part of the configuration tree.
fn apply_jinja_allowed_values_schema(
    target_node: TreeNodeReference,
    reference_text: &str,
    root_node: TreeNodeReference,
) {
    {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some("string".to_string());
        target_borrowed.allowed = true;
    }

    match resolve_allowed_values_from_jinja_reference(
        reference_text,
        Rc::clone(&target_node),
        root_node,
    ) {
        Some(allowed_values) => {
            target_node.borrow_mut().allowed_values = Some(allowed_values);
        }
        None => {
            target_node.borrow_mut().schema_error = Some(format!(
                "could not resolve allowed values from '{}' -> the referenced path is missing or empty",
                reference_text
            ));
        }
    }
}

// =====================================================================
// SEQUENCE SCHEMAS (allowed-value lists and typed element mappings)
// =====================================================================

/// A sequence schema takes one of three forms:
/// - a single type name or mapping (e.g. `[integer]` or `[{port: integer}]`)
///   declaring a list whose elements all match that definition,
/// - a list of scalar values declaring the allowed values of the field,
/// - anything else, which is reported as a schema error.
fn apply_sequence_schema(
    target_node: TreeNodeReference,
    schema_sequence: &[Value],
    schema_origin_name: &str,
    root_node: TreeNodeReference,
    loaded_schemas: &SchemaCollection,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    if let Some(element_schema) = sequence_element_schema(schema_sequence) {
        return apply_list_element_schema(
            target_node,
            element_schema,
            schema_origin_name,
            root_node,
            loaded_schemas,
            declarations,
        );
    }

    if let Some(allowed_values) = collect_allowed_scalar_values(schema_sequence) {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some("string".to_string());
        target_borrowed.allowed = true;
        target_borrowed.allowed_values = Some(allowed_values);
        return Ok(());
    }

    mark_schema_error(
        &target_node,
        "invalid sequence schema -> expected a single element type (e.g. [integer]) or a list of allowed scalar values".to_string(),
    );
    Ok(())
}

/// Returns the element definition when the sequence declares a typed list:
/// a single mapping element, or a single string element naming a known type
/// (so the unquoted YAML form `ports: [integer]` works like `"[integer]"`).
fn sequence_element_schema(schema_sequence: &[Value]) -> Option<&Value> {
    if schema_sequence.len() != 1 {
        return None;
    }

    match &schema_sequence[0] {
        Value::Mapping(_) => Some(&schema_sequence[0]),
        Value::String(type_text) if is_known_type_name(type_text) => Some(&schema_sequence[0]),
        _ => None,
    }
}

fn apply_list_element_schema(
    target_node: TreeNodeReference,
    element_schema: &Value,
    schema_origin_name: &str,
    root_node: TreeNodeReference,
    loaded_schemas: &SchemaCollection,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    let children = {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some("list".to_string());
        target_borrowed.allowed = true;
        target_borrowed.children.clone()
    };

    for child in children {
        apply_schema_definition_to_tree_node(
            child,
            element_schema,
            schema_origin_name,
            Rc::clone(&root_node),
            loaded_schemas,
            declarations,
        )?;
    }
    Ok(())
}

fn collect_allowed_scalar_values(schema_sequence: &[Value]) -> Option<Vec<Value>> {
    if schema_sequence.is_empty() {
        return None;
    }

    let mut allowed_values = Vec::new();
    for item in schema_sequence {
        match item {
            Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                allowed_values.push(item.clone())
            }
            _ => return None,
        }
    }
    Some(allowed_values)
}

// =====================================================================
// MAPPING SCHEMAS
// =====================================================================

fn apply_mapping_schema(
    target_node: TreeNodeReference,
    schema_mapping: &Mapping,
    schema_origin_name: &str,
    root_node: TreeNodeReference,
    loaded_schemas: &SchemaCollection,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    if let Some(declared_schema) = look_up_use_declaration(schema_mapping, declarations) {
        return apply_schema_definition_to_tree_node(
            target_node,
            &declared_schema,
            schema_origin_name,
            root_node,
            loaded_schemas,
            declarations,
        );
    }

    {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some("object".to_string());
        target_borrowed.allowed = true;
    }

    let local_declarations = collect_local_declarations(schema_mapping, declarations);

    inject_missing_schema_fields(schema_mapping, Rc::clone(&target_node), schema_origin_name);

    mark_all_children_not_allowed(&target_node);

    apply_field_schemas_to_children(
        schema_mapping,
        Rc::clone(&target_node),
        schema_origin_name,
        Rc::clone(&root_node),
        loaded_schemas,
        &local_declarations,
    )?;

    apply_advanced_field_definitions(
        schema_mapping,
        Rc::clone(&target_node),
        Rc::clone(&root_node),
        schema_origin_name,
    )?;

    apply_conditional_field_blocks(
        schema_mapping,
        target_node,
        root_node,
        schema_origin_name,
        loaded_schemas,
        &local_declarations,
    )?;

    Ok(())
}

/// Resolves a "$use" reference to its "$declare"d sub-schema, if present.
fn look_up_use_declaration(
    schema_mapping: &Mapping,
    declarations: &SchemaDeclarations,
) -> Option<Value> {
    if let Some(Value::String(declaration_name)) = schema_mapping.get("$use") {
        return declarations.get(declaration_name).cloned();
    }
    None
}

/// Merges "$declare" entries from this schema mapping into the declarations
/// inherited from enclosing schemas.
fn collect_local_declarations(
    schema_mapping: &Mapping,
    inherited_declarations: &SchemaDeclarations,
) -> SchemaDeclarations {
    let mut local_declarations = inherited_declarations.clone();
    if let Some(Value::Mapping(declared_schemas)) = schema_mapping.get("$declare") {
        for (declaration_key, declaration_schema) in declared_schemas {
            if let Value::String(declaration_name) = declaration_key {
                local_declarations.insert(declaration_name.clone(), declaration_schema.clone());
            }
        }
    }
    local_declarations
}

fn mark_all_children_not_allowed(target_node: &TreeNodeReference) {
    let target_borrowed = target_node.borrow();
    for child in &target_borrowed.children {
        child.borrow_mut().allowed = false;
    }
}

/// The keys an object may carry beyond its explicitly declared fields,
/// derived from a `$keyed_by` directive.
enum AllowedKeys {
    /// No `$keyed_by` directive: the `$any` catch-all (if present) governs
    /// which keys are permitted, preserving the original behavior.
    Unrestricted,
    /// Keys must be drawn from this set.
    Restricted(HashSet<String>),
    /// A `$keyed_by` directive was present but could not be resolved. A
    /// SchemaError has been recorded; key checking is skipped so the build
    /// does not also flag every key as disallowed.
    Unresolved,
}

/// Matches each child of the target node against the schema mapping and
/// applies the corresponding definition. Precedence for each child:
///   1. an explicitly declared field (or an `$optional` field),
///   2. otherwise, if the key is permitted, the `$any` value schema (or `any`
///      when `$keyed_by` is given without `$any`),
///   3. otherwise a '$'-prefixed key is passed through for later phases,
///   4. otherwise the key is left disallowed (reported as FieldNotAllowed).
///
/// A `$keyed_by` directive restricts which non-declared keys are permitted to
/// a fixed list, or to the names drawn from a referenced list/dict. This is
/// shared between regular mapping schemas and "$then" blocks of conditional
/// fields.
fn apply_field_schemas_to_children(
    schema_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_origin_name: &str,
    root_node: TreeNodeReference,
    loaded_schemas: &SchemaCollection,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    let optional_field_names = extract_optional_field_names(schema_mapping);
    let any_field_schema = schema_mapping.get("$any");
    let allowed_keys = resolve_allowed_key_set(schema_mapping, &target_node, &root_node);
    let children = target_node.borrow().children.clone();

    for child in children {
        let child_key = match child.borrow().key.clone() {
            Some(key) => key,
            None => continue,
        };

        let (field_schema, is_optional) =
            look_up_field_schema(schema_mapping, &child_key, &optional_field_names);

        if let Some(field_schema) = field_schema {
            apply_schema_definition_to_tree_node(
                Rc::clone(&child),
                &field_schema,
                schema_origin_name,
                Rc::clone(&root_node),
                loaded_schemas,
                declarations,
            )?;
            if is_optional {
                child.borrow_mut().optional = true;
            }
            continue;
        }

        if key_is_permitted(&child_key, &allowed_keys, any_field_schema.is_some()) {
            if let Some(any_field_schema) = any_field_schema {
                apply_schema_definition_to_tree_node(
                    Rc::clone(&child),
                    any_field_schema,
                    schema_origin_name,
                    Rc::clone(&root_node),
                    loaded_schemas,
                    declarations,
                )?;
            } else {
                child.borrow_mut().allowed = true;
            }
        } else if child_key.starts_with('$') {
            child.borrow_mut().allowed = true;
        }
    }
    Ok(())
}

fn key_is_permitted(child_key: &str, allowed_keys: &AllowedKeys, has_any_schema: bool) -> bool {
    match allowed_keys {
        AllowedKeys::Unrestricted => has_any_schema,
        AllowedKeys::Restricted(permitted_keys) => permitted_keys.contains(child_key),
        AllowedKeys::Unresolved => true,
    }
}

/// Computes the permitted key set from a `$keyed_by` directive. The directive
/// accepts a list of literal keys or a `"{{ reference }}"` expression that
/// resolves to a list (its values) or a dict (its keys) — the same names that
/// `"{{ reference }}"` produces as allowed values for a scalar field.
fn resolve_allowed_key_set(
    schema_mapping: &Mapping,
    target_node: &TreeNodeReference,
    root_node: &TreeNodeReference,
) -> AllowedKeys {
    let keys_definition = match schema_mapping.get("$keyed_by") {
        Some(definition) => definition,
        None => return AllowedKeys::Unrestricted,
    };

    match keys_definition {
        Value::Sequence(key_sequence) => {
            AllowedKeys::Restricted(key_sequence.iter().filter_map(value_as_key).collect())
        }
        Value::String(reference_text) if reference_text.contains("{{") => {
            match resolve_allowed_values_from_jinja_reference(
                reference_text,
                Rc::clone(target_node),
                Rc::clone(root_node),
            ) {
                Some(values) => {
                    AllowedKeys::Restricted(values.iter().filter_map(value_as_key).collect())
                }
                None => {
                    target_node.borrow_mut().schema_error = Some(format!(
                        "could not resolve $keyed_by from '{}' -> the referenced path is missing or empty",
                        reference_text
                    ));
                    AllowedKeys::Unresolved
                }
            }
        }
        Value::String(single_key) => AllowedKeys::Restricted(HashSet::from([single_key.clone()])),
        _ => {
            target_node.borrow_mut().schema_error = Some(
                "invalid $keyed_by -> expected a list of keys or a \"{{ reference }}\" expression"
                    .to_string(),
            );
            AllowedKeys::Unresolved
        }
    }
}

fn value_as_key(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(boolean) => Some(boolean.to_string()),
        _ => None,
    }
}

/// Finds the schema definition for a field, checking direct declarations
/// first and the "$optional" block second. Returns the definition together
/// with whether the field is optional.
fn look_up_field_schema(
    schema_mapping: &Mapping,
    field_name: &str,
    optional_field_names: &[String],
) -> (Option<Value>, bool) {
    let field_key = Value::String(field_name.to_string());

    if let Some(field_schema) = schema_mapping.get(&field_key) {
        let is_optional = optional_field_names.contains(&field_name.to_string());
        return (Some(field_schema.clone()), is_optional);
    }

    if let Some(Value::Mapping(optional_mapping)) = schema_mapping.get("$optional")
        && let Some(optional_schema) = optional_mapping.get(&field_key)
    {
        return (Some(optional_schema.clone()), true);
    }

    (None, false)
}

fn extract_optional_field_names(schema_mapping: &Mapping) -> Vec<String> {
    let mut optional_field_names = Vec::new();
    if let Some(Value::Mapping(optional_mapping)) = schema_mapping.get("$optional") {
        for (optional_key, _) in optional_mapping {
            if let Value::String(field_name) = optional_key {
                optional_field_names.push(field_name.clone());
            }
        }
    }
    optional_field_names
}

/// Creates placeholder children for every field the schema declares but the
/// configuration does not provide, so missing mandatory fields show up in
/// diagnostics (and "$advanced_fields" defaults have a node to fill).
fn inject_missing_schema_fields(
    schema_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_origin_name: &str,
) {
    let expected_field_names = collect_expected_field_names(schema_mapping);

    let mut target_borrowed = target_node.borrow_mut();
    let parent_span = target_borrowed.span.clone();
    let parent_origin = target_borrowed.origin.clone();

    let existing_field_names: Vec<String> = target_borrowed
        .children
        .iter()
        .filter_map(|child| child.borrow().key.clone())
        .collect();

    for expected_field_name in expected_field_names {
        if existing_field_names.contains(&expected_field_name) {
            continue;
        }

        let placeholder_node = new_tree_node_reference(CoalTree {
            key: Some(expected_field_name),
            value: None,
            parent: Some(Rc::downgrade(&target_node)),
            schema_origin: Some(schema_origin_name.to_string()),
            origin: parent_origin.clone(),
            optional: false,
            allowed: true,
            span: parent_span.clone(),
            ..Default::default()
        });
        target_borrowed.children.push(placeholder_node);
    }
}

fn collect_expected_field_names(schema_mapping: &Mapping) -> Vec<String> {
    let mut expected_field_names = Vec::new();

    for (key, _) in schema_mapping {
        if let Value::String(field_name) = key
            && !field_name.starts_with('$')
        {
            expected_field_names.push(field_name.clone());
        }
    }

    if let Some(Value::Mapping(advanced_fields)) = schema_mapping.get("$advanced_fields") {
        for (key, _) in advanced_fields {
            if let Value::String(field_name) = key
                && !expected_field_names.contains(field_name)
            {
                expected_field_names.push(field_name.clone());
            }
        }
    }

    expected_field_names
}

// =====================================================================
// $advanced_fields
// =====================================================================

/// Applies "$advanced_fields" definitions (type, default_value,
/// allowed_values) to the matching children of the target node.
fn apply_advanced_field_definitions(
    schema_mapping: &Mapping,
    target_node: TreeNodeReference,
    root_node: TreeNodeReference,
    schema_origin_name: &str,
) -> Result<(), Box<dyn Error>> {
    let advanced_fields = match schema_mapping.get("$advanced_fields") {
        Some(Value::Mapping(advanced_fields)) => advanced_fields,
        _ => return Ok(()),
    };

    let children = target_node.borrow().children.clone();

    for (field_key, field_definition) in advanced_fields {
        let field_name = match field_key {
            Value::String(field_name) => field_name,
            _ => continue,
        };

        for child in &children {
            if child.borrow().key.as_deref() == Some(field_name) {
                apply_advanced_definition_to_node(
                    child,
                    field_definition,
                    &target_node,
                    &root_node,
                    schema_origin_name,
                );
            }
        }
    }
    Ok(())
}

fn apply_advanced_definition_to_node(
    node: &TreeNodeReference,
    field_definition: &Value,
    context_node: &TreeNodeReference,
    root_node: &TreeNodeReference,
    schema_origin_name: &str,
) {
    {
        let mut node_borrowed = node.borrow_mut();
        node_borrowed.schema_origin = Some(schema_origin_name.to_string());
        node_borrowed.allowed = true;
    }

    let definition_mapping = match field_definition {
        Value::Mapping(definition_mapping) => definition_mapping,
        _ => return,
    };

    if let Some(Value::String(type_text)) = definition_mapping.get("type") {
        node.borrow_mut().field_type = Some(type_text.clone());
    }

    if let Some(default_value) = definition_mapping.get("default_value") {
        let mut node_borrowed = node.borrow_mut();
        node_borrowed.default_value = Some(default_value.clone());
        if node_borrowed.value.is_none() {
            node_borrowed.value = Some(default_value.clone());
        }
    }

    match definition_mapping.get("allowed_values") {
        Some(Value::Sequence(allowed_values)) => {
            node.borrow_mut().allowed_values = Some(allowed_values.clone());
        }
        Some(Value::String(jinja_reference)) => {
            match resolve_allowed_values_from_jinja_reference(
                jinja_reference,
                Rc::clone(context_node),
                Rc::clone(root_node),
            ) {
                Some(allowed_values) => {
                    node.borrow_mut().allowed_values = Some(allowed_values);
                }
                None => {
                    node.borrow_mut().schema_error = Some(format!(
                        "could not resolve allowed values from '{}' -> the referenced path is missing or empty",
                        jinja_reference
                    ));
                }
            }
        }
        _ => {}
    }
}

// =====================================================================
// JINJA-DRIVEN ALLOWED VALUES
// =====================================================================

/// Resolves a "{{ reference }}" expression in a schema into the list of
/// allowed values found at the referenced configuration node. Returns None
/// when the expression or its target cannot be resolved.
fn resolve_allowed_values_from_jinja_reference(
    reference_text: &str,
    context_node: TreeNodeReference,
    root_node: TreeNodeReference,
) -> Option<Vec<Value>> {
    let captures = jinja_expression_regex().captures(reference_text)?;
    let reference_path = captures[1].trim();

    let target_node =
        find_target_node_using_reference_path(reference_path, context_node, root_node)?;

    let allowed_values = collect_allowed_values_from_node(&target_node);
    if allowed_values.is_empty() {
        None
    } else {
        Some(allowed_values)
    }
}

/// Extracts allowed values from a node: list children contribute their
/// values, mapping children contribute their keys. Leaf nodes fall back to
/// their raw YAML value.
fn collect_allowed_values_from_node(target_node: &TreeNodeReference) -> Vec<Value> {
    let target_borrowed = target_node.borrow();

    if !target_borrowed.children.is_empty() {
        let all_children_have_numeric_keys = target_borrowed.children.iter().all(|child| {
            child
                .borrow()
                .key
                .as_deref()
                .unwrap_or("")
                .parse::<usize>()
                .is_ok()
        });

        return if all_children_have_numeric_keys {
            target_borrowed
                .children
                .iter()
                .filter_map(|child| child.borrow().value.clone())
                .collect()
        } else {
            target_borrowed
                .children
                .iter()
                .filter_map(|child| child.borrow().key.clone().map(Value::String))
                .collect()
        };
    }

    match &target_borrowed.value {
        Some(Value::Sequence(sequence)) => sequence.clone(),
        Some(Value::Mapping(mapping)) => mapping.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

// =====================================================================
// $conditional_fields
// =====================================================================

/// Evaluates every "$conditional_fields" block: when an "$if" condition
/// matches the target node's data, the "$then" sub-schema is applied on top.
fn apply_conditional_field_blocks(
    schema_mapping: &Mapping,
    target_node: TreeNodeReference,
    root_node: TreeNodeReference,
    schema_origin_name: &str,
    loaded_schemas: &SchemaCollection,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    let conditional_blocks = match schema_mapping.get("$conditional_fields") {
        Some(Value::Sequence(conditional_blocks)) => conditional_blocks,
        _ => return Ok(()),
    };

    for conditional_block in conditional_blocks {
        let Value::Mapping(conditional_mapping) = conditional_block else {
            continue;
        };
        let Some(if_condition) = conditional_mapping.get("$if") else {
            continue;
        };

        if !evaluate_condition_block(if_condition, Rc::clone(&target_node)) {
            continue;
        }

        if let Some(Value::Mapping(then_mapping)) = conditional_mapping.get("$then") {
            apply_then_block_schema(
                then_mapping,
                Rc::clone(&target_node),
                Rc::clone(&root_node),
                schema_origin_name,
                loaded_schemas,
                declarations,
            )?;
        }
    }
    Ok(())
}

/// Applies a "$then" sub-schema on top of an already validated node: missing
/// fields are injected, matching children are re-validated against the
/// conditional definitions, and advanced fields are applied.
fn apply_then_block_schema(
    then_mapping: &Mapping,
    target_node: TreeNodeReference,
    root_node: TreeNodeReference,
    schema_origin_name: &str,
    loaded_schemas: &SchemaCollection,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    inject_missing_schema_fields(then_mapping, Rc::clone(&target_node), schema_origin_name);

    apply_field_schemas_to_children(
        then_mapping,
        Rc::clone(&target_node),
        schema_origin_name,
        Rc::clone(&root_node),
        loaded_schemas,
        declarations,
    )?;

    apply_advanced_field_definitions(then_mapping, target_node, root_node, schema_origin_name)?;

    Ok(())
}

/// Evaluates an "$if" condition. Mappings combine field-equality checks with
/// "$and"/"$or" operators; sequences require every entry to hold.
fn evaluate_condition_block(condition: &Value, target_node: TreeNodeReference) -> bool {
    match condition {
        Value::Sequence(conditions) => conditions
            .iter()
            .all(|entry| evaluate_condition_block(entry, Rc::clone(&target_node))),
        Value::Mapping(condition_mapping) => condition_mapping.iter().all(|(key, expected)| {
            evaluate_condition_entry(key, expected, Rc::clone(&target_node))
        }),
        _ => false,
    }
}

fn evaluate_condition_entry(
    condition_key: &Value,
    expected_value: &Value,
    target_node: TreeNodeReference,
) -> bool {
    let Value::String(condition_name) = condition_key else {
        return true;
    };

    match condition_name.as_str() {
        "$or" => evaluate_or_condition(expected_value, target_node),
        "$and" => evaluate_condition_block(expected_value, target_node),
        field_name => field_has_expected_value(field_name, expected_value, target_node),
    }
}

fn evaluate_or_condition(condition: &Value, target_node: TreeNodeReference) -> bool {
    match condition {
        Value::Sequence(alternatives) => alternatives
            .iter()
            .any(|entry| evaluate_condition_block(entry, Rc::clone(&target_node))),
        // A malformed $or (not a sequence) matches everything, mirroring the
        // permissive behavior of the original engine.
        _ => true,
    }
}

fn field_has_expected_value(
    field_name: &str,
    expected_value: &Value,
    target_node: TreeNodeReference,
) -> bool {
    let children = target_node.borrow().children.clone();

    for child in children {
        let child_borrowed = child.borrow();
        if child_borrowed.key.as_deref() == Some(field_name) {
            return child_borrowed
                .value
                .as_ref()
                .is_some_and(|child_value| child_value == expected_value);
        }
    }
    false
}
