//! The schema engine: it walks a schema definition alongside the compiled
//! tree and records, on every node, what the configuration was allowed to put
//! there.
//!
//! A schema definition is one of three shapes:
//!
//! * a **string** — a type name (`integer`), a list-of type (`[string]`), or a
//!   Jinja template whose result is the set of values the field may hold,
//! * a **sequence** — either a single element definition (a typed list) or a
//!   literal list of allowed values,
//! * a **mapping** — an object, whose keys are the fields it declares plus the
//!   `$` directives that shape it.
//!
//! The `$` directives that shape an object (`$declare`/`$use`, `$optional`,
//! `$conditional`, `$any`, `$keyed_by`, `$advanced_fields`) all operate on the
//! *same* node and are freely nestable: a `$conditional` may live inside an
//! `$optional`, its `$then` may `$use` a declaration, and that declaration may
//! carry conditionals of its own.

use crate::coal_tree::CoalTree;
use crate::jinja_engine::{
    TemplateScope, contains_template, evaluate_condition, prepare_tree_for_templates,
    render_value_template,
};
use crate::type_aliases::{TreeNodeReference, VirtualFileSystemReference, new_tree_node_reference};
use crate::yaml_values::{deep_merge_yaml_values, extract_string_list_from_value};
use minijinja::Value as JinjaValue;
use serde_yaml::{Mapping, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::Path;
use std::rc::Rc;

/// Schema definitions keyed by schema name (the file stem).
pub type SchemaCollection = HashMap<String, Value>;

/// Reusable sub-schemas declared with "$declare", keyed by declaration name.
type SchemaDeclarations = HashMap<String, Value>;

/// How many times a "$use" chain may pull in another definition that itself
/// uses one. Deep enough for any real schema, shallow enough that a cycle
/// stops instead of looping.
const MAX_USE_EXPANSION_DEPTH: usize = 32;

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
// SCHEMA ENVIRONMENT
// =====================================================================

/// What one validation run shares: the tree being validated, and that same
/// tree prepared for the Jinja engine.
///
/// The prepared tree is what makes schema templates affordable. Evaluating
/// `"{{ ROOT.transports.keys() }}"` on a thousand fields would otherwise walk
/// and convert the whole configuration a thousand times; here it is built once
/// and rebuilt only when the schema engine itself changes the data — when it
/// creates a node for a missing field, or fills in a default.
pub struct SchemaEnvironment {
    root_node: TreeNodeReference,
    prepared_tree: RefCell<Option<JinjaValue>>,
}

impl SchemaEnvironment {
    pub fn new(root_node: TreeNodeReference) -> Self {
        Self {
            root_node,
            prepared_tree: RefCell::new(None),
        }
    }

    /// The scope a template written on `node` is evaluated in. By the time a
    /// schema runs, every template that could be resolved has been, so a
    /// schema template reading the object it describes is allowed to go ahead.
    fn scope_for(&self, node: &TreeNodeReference) -> TemplateScope {
        TemplateScope::for_node(node, &self.root_node)
            .with_prepared_tree(self.prepared_tree())
            .relaxed()
    }

    fn prepared_tree(&self) -> JinjaValue {
        if let Some(ref prepared_tree) = *self.prepared_tree.borrow() {
            return prepared_tree.clone();
        }

        let prepared_tree = prepare_tree_for_templates(Rc::clone(&self.root_node));
        *self.prepared_tree.borrow_mut() = Some(prepared_tree.clone());
        prepared_tree
    }

    /// Called whenever the schema engine writes to the tree, so the next
    /// template sees the change.
    fn tree_changed(&self) {
        *self.prepared_tree.borrow_mut() = None;
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
    environment: &SchemaEnvironment,
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
            environment,
            declarations,
        ),
        Value::Sequence(schema_sequence) => apply_sequence_schema(
            target_node,
            schema_sequence,
            schema_origin_name,
            environment,
            declarations,
        ),
        Value::Mapping(schema_mapping) => apply_mapping_schema(
            target_node,
            schema_mapping,
            schema_origin_name,
            environment,
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
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    if type_text.starts_with('[') && type_text.ends_with(']') && !contains_template(type_text) {
        return apply_bracketed_list_schema(
            target_node,
            type_text,
            schema_origin_name,
            environment,
            declarations,
        );
    }

    if is_bracketed_template(type_text) {
        return apply_bracketed_list_schema(
            target_node,
            type_text,
            schema_origin_name,
            environment,
            declarations,
        );
    }

    if contains_template(type_text) {
        apply_template_allowed_values_schema(target_node, type_text, environment);
        return Ok(());
    }

    if is_known_type_name(type_text) {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some(type_text.to_string());
        target_borrowed.allowed = true;
        return Ok(());
    }

    mark_schema_error(
        &target_node,
        format!(
            "unknown type '{}' -> expected one of {:?}, a \"{{{{ ... }}}}\" template, or a mapping",
            type_text, KNOWN_TYPE_NAMES
        ),
    );
    Ok(())
}

/// Recognizes the "[{{ ... }}]" form: a list whose every element is
/// constrained by the template inside the brackets. The brackets are COAL's
/// list-of marker, not part of the template.
fn is_bracketed_template(type_text: &str) -> bool {
    let trimmed = type_text.trim();
    trimmed.starts_with('[')
        && trimmed.ends_with(']')
        && contains_template(&trimmed[1..trimmed.len() - 1])
}

/// Handles the "[element_type]" form: the node itself must be a list, and
/// the inner type is applied to every element.
fn apply_bracketed_list_schema(
    target_node: TreeNodeReference,
    type_text: &str,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    let children = {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some("list".to_string());
        target_borrowed.allowed = true;
        target_borrowed.children.clone()
    };

    let trimmed = type_text.trim();
    let element_type_text = &trimmed[1..trimmed.len() - 1];
    let element_schema = Value::String(element_type_text.to_string());

    for child in children {
        apply_schema_definition_to_tree_node(
            child,
            &element_schema,
            schema_origin_name,
            environment,
            declarations,
        )?;
    }
    Ok(())
}

/// Handles a template definition: the field's allowed values are whatever the
/// template evaluates to. The template is ordinary Jinja evaluated in the
/// field's own scope, so it can read siblings by name and reach anywhere in
/// the tree through `ROOT`.
fn apply_template_allowed_values_schema(
    target_node: TreeNodeReference,
    template_text: &str,
    environment: &SchemaEnvironment,
) {
    {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some("string".to_string());
        target_borrowed.allowed = true;
    }

    match resolve_allowed_values_from_template(template_text, &target_node, environment) {
        Ok(allowed_values) => {
            target_node.borrow_mut().allowed_values = Some(allowed_values);
        }
        Err(reason) => {
            target_node.borrow_mut().schema_error = Some(format!(
                "could not resolve allowed values from '{}' -> {}",
                template_text, reason
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
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    if let Some(element_schema) = sequence_element_schema(schema_sequence) {
        return apply_list_element_schema(
            target_node,
            element_schema,
            schema_origin_name,
            environment,
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
/// a single mapping element, a single string element naming a known type (so
/// the unquoted YAML form `ports: [integer]` works like `"[integer]"`), or a
/// single template (so `[{{ ROOT.clusters }}]` constrains every element).
fn sequence_element_schema(schema_sequence: &[Value]) -> Option<&Value> {
    if schema_sequence.len() != 1 {
        return None;
    }

    match &schema_sequence[0] {
        Value::Mapping(_) => Some(&schema_sequence[0]),
        Value::String(type_text) if is_known_type_name(type_text) => Some(&schema_sequence[0]),
        Value::String(type_text) if contains_template(type_text) => Some(&schema_sequence[0]),
        _ => None,
    }
}

fn apply_list_element_schema(
    target_node: TreeNodeReference,
    element_schema: &Value,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
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
            environment,
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

/// How a schema block is applied on top of the node it describes.
///
/// A block is a mapping of field definitions and `$` directives. The outermost
/// block of an object is the schema itself; `$optional` and the `$then` of a
/// `$conditional` are blocks applied on top of it, and they inherit these
/// settings so that, for instance, a conditional nested inside an `$optional`
/// keeps its fields optional.
#[derive(Clone, Copy)]
struct BlockOptions {
    /// Whether fields the block declares but the configuration omits are
    /// created as empty nodes, so a missing mandatory field is reported.
    inject_missing_fields: bool,
    /// Whether every field the block matches is permitted to be absent.
    fields_are_optional: bool,
    /// Whether the block is layered on top of a schema that has already been
    /// applied to the same node. A layered block only ever grants: it never
    /// takes back what the block underneath it already permitted.
    layered: bool,
}

impl BlockOptions {
    /// The settings of an object's own schema.
    fn declaring() -> Self {
        Self {
            inject_missing_fields: true,
            fields_are_optional: false,
            layered: false,
        }
    }

    /// The settings of an `$optional` block: its fields are permitted, never
    /// required, so nothing is injected for the ones that are absent.
    fn optional(self) -> Self {
        Self {
            inject_missing_fields: false,
            fields_are_optional: true,
            layered: true,
        }
    }

    /// The settings of a `$then` block: it says what else is required once its
    /// condition holds, under whatever rules the block that carries it works.
    fn conditional_then(self) -> Self {
        Self {
            layered: true,
            ..self
        }
    }

    /// The settings inside a field of a layered block. The field's own
    /// definition is a fresh schema — its fields are required unless it says
    /// otherwise — but the node it describes has already been validated, so
    /// its existing grants stay.
    fn inside_layered_field() -> Self {
        Self {
            inject_missing_fields: true,
            fields_are_optional: false,
            layered: true,
        }
    }
}

fn apply_mapping_schema(
    target_node: TreeNodeReference,
    schema_mapping: &Mapping,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    let (effective_schema, declarations) =
        prepare_schema_block(schema_mapping, declarations, &target_node);

    // "$use" may name a definition that is not an object at all — a type name
    // or an allowed-value list. The field then simply *is* that definition.
    let Value::Mapping(effective_mapping) = effective_schema else {
        return apply_schema_definition_to_tree_node(
            target_node,
            &effective_schema,
            schema_origin_name,
            environment,
            &declarations,
        );
    };

    {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some("object".to_string());
        target_borrowed.allowed = true;
    }

    mark_all_children_not_allowed(&target_node);

    apply_prepared_schema_block(
        &effective_mapping,
        target_node,
        schema_origin_name,
        environment,
        &declarations,
        BlockOptions::declaring(),
    )
}

/// Applies a block that sits on top of an object that has already been
/// validated: an `$optional` block, or the `$then` of a matching
/// `$conditional`. Such a block only ever grants more — it never takes back
/// what the enclosing schema already permitted.
fn apply_nested_schema_block(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    let (effective_schema, declarations) =
        prepare_schema_block(block_mapping, declarations, &target_node);

    let Value::Mapping(effective_mapping) = effective_schema else {
        mark_schema_error(
            &target_node,
            "invalid schema block -> expected a mapping of field definitions".to_string(),
        );
        return Ok(());
    };

    apply_prepared_schema_block(
        &effective_mapping,
        target_node,
        schema_origin_name,
        environment,
        &declarations,
        options,
    )
}

/// The directives a schema block may carry. Everything else starting with a
/// "$" is a mistake worth reporting rather than ignoring in silence.
const KNOWN_BLOCK_DIRECTIVES: [&str; 7] = [
    "$schema",
    "$declare",
    "$use",
    "$optional",
    "$conditional",
    "$any",
    "$keyed_by",
];

/// Reports any "$" key the schema engine does not know, so a typo or an
/// outdated directive name surfaces instead of being quietly skipped.
fn check_block_directives_are_known(block_mapping: &Mapping, target_node: &TreeNodeReference) {
    for key in block_mapping.keys() {
        let Value::String(directive_name) = key else {
            continue;
        };
        if !directive_name.starts_with('$')
            || KNOWN_BLOCK_DIRECTIVES.contains(&directive_name.as_str())
            || directive_name == "$advanced_fields"
        {
            continue;
        }

        let hint = match directive_name.as_str() {
            "$conditional_fields" => " -> it is now called $conditional".to_string(),
            _ => format!(" -> expected one of {:?}", KNOWN_BLOCK_DIRECTIVES),
        };
        mark_schema_error(
            target_node,
            format!("unknown schema directive '{}'{}", directive_name, hint),
        );
    }
}

/// Applies one block of field definitions and directives to a node. Every
/// directive that shapes an object is handled here, which is what makes them
/// nestable: `$optional` and `$conditional` route straight back into this
/// same function.
fn apply_prepared_schema_block(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    check_block_directives_are_known(block_mapping, &target_node);

    if options.inject_missing_fields {
        inject_missing_schema_fields(
            block_mapping,
            Rc::clone(&target_node),
            schema_origin_name,
            environment,
        );
    }

    apply_field_schemas_to_children(
        block_mapping,
        Rc::clone(&target_node),
        schema_origin_name,
        environment,
        declarations,
        options,
    )?;

    apply_advanced_field_definitions(
        block_mapping,
        Rc::clone(&target_node),
        environment,
        schema_origin_name,
        options,
    )?;

    apply_optional_block(
        block_mapping,
        Rc::clone(&target_node),
        schema_origin_name,
        environment,
        declarations,
        options,
    )?;

    apply_conditional_blocks(
        block_mapping,
        target_node,
        schema_origin_name,
        environment,
        declarations,
        options,
    )?;

    Ok(())
}

// =====================================================================
// $declare and $use
// =====================================================================

/// Resolves a block's "$use" directives and collects its "$declare" entries,
/// returning the schema that is actually applied together with the
/// declarations visible inside it.
///
/// A used definition is merged *underneath* the block that uses it, so fields
/// written next to `$use` stay valid and win where both define the same field
/// — the same rule `$inherit` follows in configuration files. `$use` also
/// accepts a list, merged left to right.
fn prepare_schema_block(
    block_mapping: &Mapping,
    inherited_declarations: &SchemaDeclarations,
    target_node: &TreeNodeReference,
) -> (Value, SchemaDeclarations) {
    let mut declarations = collect_local_declarations(block_mapping, inherited_declarations);
    let mut effective_schema = Value::Mapping(block_mapping.clone());
    let mut already_used = HashSet::new();

    for _ in 0..MAX_USE_EXPANSION_DEPTH {
        let Value::Mapping(current_mapping) = &effective_schema else {
            break;
        };
        let Some(use_directive) = current_mapping.get("$use") else {
            break;
        };

        let used_names = extract_string_list_from_value(use_directive);
        let mut local_mapping = current_mapping.clone();
        local_mapping.remove("$use");

        let used_schema = merge_used_declarations(
            &used_names,
            &declarations,
            &mut already_used,
            target_node,
            &local_mapping,
        );

        effective_schema = match used_schema {
            // A declaration that is not an object — a type name, or a list of
            // allowed values — simply *is* the field's schema.
            Some(used_schema) if !used_schema.is_mapping() => {
                return (used_schema, declarations);
            }
            Some(used_schema) => {
                deep_merge_yaml_values(&used_schema, &Value::Mapping(local_mapping))
            }
            None => Value::Mapping(local_mapping),
        };

        if let Value::Mapping(expanded_mapping) = &effective_schema {
            declarations = collect_local_declarations(expanded_mapping, &declarations);
        }
    }

    if let Value::Mapping(unfinished_mapping) = &effective_schema
        && unfinished_mapping.contains_key("$use")
    {
        mark_schema_error(
            target_node,
            format!(
                "$use chain is deeper than {} -> a declaration that uses a declaration that uses a declaration, without end",
                MAX_USE_EXPANSION_DEPTH
            ),
        );
    }

    (effective_schema, declarations)
}

/// Merges the named declarations into one definition. Returns None when
/// nothing usable was found, and reports a schema error naming what was
/// missing or unmergeable.
fn merge_used_declarations(
    used_names: &[String],
    declarations: &SchemaDeclarations,
    already_used: &mut HashSet<String>,
    target_node: &TreeNodeReference,
    local_mapping: &Mapping,
) -> Option<Value> {
    let mut merged_schema: Option<Value> = None;

    for used_name in used_names {
        if !already_used.insert(used_name.clone()) {
            continue;
        }

        let Some(declared_schema) = declarations.get(used_name) else {
            mark_schema_error(
                target_node,
                format!(
                    "unknown $use target '{}' -> no $declare with that name is in scope",
                    used_name
                ),
            );
            continue;
        };

        if !declared_schema.is_mapping() {
            if block_declares_fields(local_mapping) {
                mark_schema_error(
                    target_node,
                    format!(
                        "$use '{}' names a plain definition -> it cannot be combined with the fields declared beside it",
                        used_name
                    ),
                );
            }
            return Some(declared_schema.clone());
        }

        merged_schema = Some(match merged_schema {
            Some(previous) => deep_merge_yaml_values(&previous, declared_schema),
            None => declared_schema.clone(),
        });
    }

    merged_schema
}

/// Whether the block declares anything of its own besides the directives that
/// only carry declarations.
fn block_declares_fields(block_mapping: &Mapping) -> bool {
    block_mapping
        .keys()
        .any(|key| !matches!(key, Value::String(name) if name == "$declare"))
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

// =====================================================================
// FIELD MATCHING
// =====================================================================

/// Starts an object's children off as not allowed, so that only the fields the
/// schema goes on to match end up permitted. Recording that this happened is
/// what lets a later block layer more fields on top without undoing it.
fn mark_all_children_not_allowed(target_node: &TreeNodeReference) {
    let mut target_borrowed = target_node.borrow_mut();
    target_borrowed.object_fields_checked = true;

    for child in &target_borrowed.children {
        child.borrow_mut().allowed = false;
    }
}

/// The keys an object may carry beyond its explicitly declared fields,
/// derived from a `$keyed_by` directive.
enum AllowedKeys {
    /// No `$keyed_by` directive: the `$any` catch-all (if present) governs
    /// which keys are permitted.
    Unrestricted,
    /// Keys must be drawn from this set.
    Restricted(HashSet<String>),
    /// A `$keyed_by` directive was present but could not be resolved. A
    /// SchemaError has been recorded; key checking is skipped so the build
    /// does not also flag every key as disallowed.
    Unresolved,
}

/// Matches each child of the target node against the block and applies the
/// corresponding definition. Precedence for each child:
///   1. an explicitly declared field,
///   2. otherwise, if the key is permitted, the `$any` value schema (or `any`
///      when `$keyed_by` is given without `$any`),
///   3. otherwise a '$'-prefixed key is passed through for later phases,
///   4. otherwise the key is left as it was (reported as FieldNotAllowed
///      unless another block permits it).
fn apply_field_schemas_to_children(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    let any_field_schema = block_mapping.get("$any");
    let allowed_keys = resolve_allowed_key_set(block_mapping, &target_node, environment);
    let children = target_node.borrow().children.clone();

    for child in children {
        let child_key = match child.borrow().key.clone() {
            Some(key) => key,
            None => continue,
        };

        if let Some(field_schema) = block_mapping.get(Value::String(child_key.clone())) {
            apply_field_schema_to_child(
                &child,
                field_schema,
                schema_origin_name,
                environment,
                declarations,
                options,
            )?;
            if options.fields_are_optional {
                child.borrow_mut().optional = true;
            }
            continue;
        }

        if key_is_permitted(
            &child_key,
            &allowed_keys,
            any_field_schema.is_some(),
            options,
        ) {
            if let Some(any_field_schema) = any_field_schema {
                apply_schema_definition_to_tree_node(
                    Rc::clone(&child),
                    any_field_schema,
                    schema_origin_name,
                    environment,
                    declarations,
                )?;
            } else {
                child.borrow_mut().allowed = true;
            }
            if options.fields_are_optional {
                child.borrow_mut().optional = true;
            }
        } else if child_key.starts_with('$') {
            child.borrow_mut().allowed = true;
        }
    }
    Ok(())
}

/// Applies one field's definition to the child that carries it.
///
/// A layered block re-declaring an object the schema underneath it already
/// described — "a database also has a password" — must add to that object
/// rather than start it over, or the fields the first block granted would be
/// revoked. Everything else is applied from scratch, including a field that
/// only a catch-all had permitted so far: its keys have never been checked,
/// and this definition is the first schema that can check them.
fn apply_field_schema_to_child(
    child: &TreeNodeReference,
    field_schema: &Value,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    let child_already_checked = child.borrow().object_fields_checked;

    if let (true, true, Value::Mapping(field_mapping)) =
        (options.layered, child_already_checked, field_schema)
    {
        return apply_layered_mapping_schema(
            Rc::clone(child),
            field_mapping,
            schema_origin_name,
            environment,
            declarations,
        );
    }

    apply_schema_definition_to_tree_node(
        Rc::clone(child),
        field_schema,
        schema_origin_name,
        environment,
        declarations,
    )
}

/// Applies an object definition on top of one that has already been applied to
/// the same node: the fields it declares are added, and the ones the earlier
/// definition permitted are left alone.
fn apply_layered_mapping_schema(
    target_node: TreeNodeReference,
    schema_mapping: &Mapping,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
) -> Result<(), Box<dyn Error>> {
    let (effective_schema, declarations) =
        prepare_schema_block(schema_mapping, declarations, &target_node);

    let Value::Mapping(effective_mapping) = effective_schema else {
        // The definition turned out not to be an object after all, so there is
        // nothing to layer: apply it the ordinary way.
        return apply_schema_definition_to_tree_node(
            target_node,
            &effective_schema,
            schema_origin_name,
            environment,
            &declarations,
        );
    };

    {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.field_type = Some("object".to_string());
        target_borrowed.allowed = true;
    }

    apply_prepared_schema_block(
        &effective_mapping,
        target_node,
        schema_origin_name,
        environment,
        &declarations,
        BlockOptions::inside_layered_field(),
    )
}

fn key_is_permitted(
    child_key: &str,
    allowed_keys: &AllowedKeys,
    has_any_schema: bool,
    options: BlockOptions,
) -> bool {
    match allowed_keys {
        AllowedKeys::Unrestricted => has_any_schema,
        AllowedKeys::Restricted(permitted_keys) => permitted_keys.contains(child_key),
        // The directive is broken and has already been reported. The block
        // that declares the object skips key checking so the same mistake is
        // not reported twice on every field; a layered block grants nothing,
        // because the block underneath it has already had its say.
        AllowedKeys::Unresolved => !options.layered,
    }
}

/// Computes the permitted key set from a `$keyed_by` directive. The directive
/// accepts a list of literal keys or a Jinja template that evaluates to the
/// list of names — `"{{ ROOT.transports.keys() }}"` for the keys of a mapping,
/// `"{{ ROOT.regions }}"` for the entries of a list.
fn resolve_allowed_key_set(
    block_mapping: &Mapping,
    target_node: &TreeNodeReference,
    environment: &SchemaEnvironment,
) -> AllowedKeys {
    let keys_definition = match block_mapping.get("$keyed_by") {
        Some(definition) => definition,
        None => return AllowedKeys::Unrestricted,
    };

    match keys_definition {
        Value::Sequence(key_sequence) => {
            AllowedKeys::Restricted(key_sequence.iter().filter_map(value_as_key).collect())
        }
        Value::String(template_text) if contains_template(template_text) => {
            match resolve_allowed_values_from_template(template_text, target_node, environment) {
                Ok(values) => {
                    AllowedKeys::Restricted(values.iter().filter_map(value_as_key).collect())
                }
                Err(reason) => {
                    target_node.borrow_mut().schema_error = Some(format!(
                        "could not resolve $keyed_by from '{}' -> {}",
                        template_text, reason
                    ));
                    AllowedKeys::Unresolved
                }
            }
        }
        Value::String(single_key) => AllowedKeys::Restricted(HashSet::from([single_key.clone()])),
        _ => {
            target_node.borrow_mut().schema_error = Some(
                "invalid $keyed_by -> expected a list of keys or a \"{{ ... }}\" template"
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

/// Creates placeholder children for every field the schema declares but the
/// configuration does not provide, so missing mandatory fields show up in
/// diagnostics (and "$advanced_fields" defaults have a node to fill).
fn inject_missing_schema_fields(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
) {
    let expected_field_names = collect_expected_field_names(block_mapping);

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
        environment.tree_changed();
    }
}

fn collect_expected_field_names(block_mapping: &Mapping) -> Vec<String> {
    let mut expected_field_names = Vec::new();

    for (key, _) in block_mapping {
        if let Value::String(field_name) = key
            && !field_name.starts_with('$')
        {
            expected_field_names.push(field_name.clone());
        }
    }

    if let Some(Value::Mapping(advanced_fields)) = block_mapping.get("$advanced_fields") {
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
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    environment: &SchemaEnvironment,
    schema_origin_name: &str,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    let advanced_fields = match block_mapping.get("$advanced_fields") {
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
                    environment,
                    schema_origin_name,
                    options,
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
    environment: &SchemaEnvironment,
    schema_origin_name: &str,
    options: BlockOptions,
) {
    {
        let mut node_borrowed = node.borrow_mut();
        node_borrowed.schema_origin = Some(schema_origin_name.to_string());
        node_borrowed.allowed = true;
        if options.fields_are_optional {
            node_borrowed.optional = true;
        }
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
            drop(node_borrowed);
            environment.tree_changed();
        }
    }

    match definition_mapping.get("allowed_values") {
        Some(Value::Sequence(allowed_values)) => {
            node.borrow_mut().allowed_values = Some(allowed_values.clone());
        }
        Some(Value::String(template_text)) => {
            match resolve_allowed_values_from_template(template_text, context_node, environment) {
                Ok(allowed_values) => {
                    node.borrow_mut().allowed_values = Some(allowed_values);
                }
                Err(reason) => {
                    node.borrow_mut().schema_error = Some(format!(
                        "could not resolve allowed values from '{}' -> {}",
                        template_text, reason
                    ));
                }
            }
        }
        _ => {}
    }
}

// =====================================================================
// TEMPLATE-DRIVEN ALLOWED VALUES
// =====================================================================

/// Evaluates a schema template and reads its result as the list of values the
/// field may hold.
///
/// The template is plain Jinja: whatever list it produces *is* the allowed
/// set. A mapping is deliberately not accepted — writing
/// `{{ ROOT.transports }}` leaves it ambiguous whether the keys or the values
/// were meant, so the schema has to say which with `.keys()` or `.values()`.
fn resolve_allowed_values_from_template(
    template_text: &str,
    context_node: &TreeNodeReference,
    environment: &SchemaEnvironment,
) -> Result<Vec<Value>, String> {
    let scope = environment.scope_for(context_node);
    let template_source = normalize_schema_template(template_text);
    let evaluated_value =
        render_value_template(&template_source, &scope).map_err(|reason| reason.describe())?;

    match evaluated_value {
        Value::Sequence(values) if !values.is_empty() => Ok(values),
        Value::Sequence(_) => Err("the template produced an empty list".to_string()),
        Value::Mapping(_) => Err(
            "the template produced a mapping -> add .keys() or .values() to say which of the two constrains the field"
                .to_string(),
        ),
        Value::Null => Err("the template produced nothing".to_string()),
        single_value => Ok(vec![single_value]),
    }
}

/// A schema may write its template with or without the surrounding braces:
/// `"{{ ROOT.regions }}"` and `"ROOT.regions"` mean the same thing where a
/// template is expected.
fn normalize_schema_template(template_text: &str) -> String {
    if contains_template(template_text) {
        return template_text.to_string();
    }
    format!("{{{{ {} }}}}", template_text.trim())
}

// =====================================================================
// $optional
// =====================================================================

/// Applies the "$optional" block: everything it declares is permitted but
/// never required. Because it is a full schema block, it may itself carry
/// `$use`, `$any`, `$keyed_by`, `$advanced_fields` and `$conditional`.
fn apply_optional_block(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    let Some(optional_definition) = block_mapping.get("$optional") else {
        return Ok(());
    };

    let Value::Mapping(optional_mapping) = optional_definition else {
        mark_schema_error(
            &target_node,
            "invalid $optional -> expected a mapping of field definitions".to_string(),
        );
        return Ok(());
    };

    apply_nested_schema_block(
        optional_mapping,
        target_node,
        schema_origin_name,
        environment,
        declarations,
        options.optional(),
    )
}

// =====================================================================
// $conditional
// =====================================================================

/// Evaluates every "$conditional" entry: when its "$if" expression holds for
/// the node's own data, the "$then" block is applied on top.
///
/// The condition is a Jinja expression evaluated in the node's scope, so it
/// reads the node's fields by name — `$if: "{{ type == 'database' }}"` — and
/// can reach anywhere else through `ROOT`. A field that is absent simply makes
/// the condition false.
fn apply_conditional_blocks(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_origin_name: &str,
    environment: &SchemaEnvironment,
    declarations: &SchemaDeclarations,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    let Some(conditional_definition) = block_mapping.get("$conditional") else {
        return Ok(());
    };

    let Value::Sequence(conditional_blocks) = conditional_definition else {
        mark_schema_error(
            &target_node,
            "invalid $conditional -> expected a list of $if/$then entries".to_string(),
        );
        return Ok(());
    };

    for conditional_block in conditional_blocks {
        let Value::Mapping(conditional_mapping) = conditional_block else {
            mark_schema_error(
                &target_node,
                "invalid $conditional entry -> expected a mapping with $if and $then".to_string(),
            );
            continue;
        };

        if !conditional_block_applies(conditional_mapping, &target_node, environment) {
            continue;
        }

        let Some(Value::Mapping(then_mapping)) = conditional_mapping.get("$then") else {
            continue;
        };

        apply_nested_schema_block(
            then_mapping,
            Rc::clone(&target_node),
            schema_origin_name,
            environment,
            declarations,
            options.conditional_then(),
        )?;
    }
    Ok(())
}

fn conditional_block_applies(
    conditional_mapping: &Mapping,
    target_node: &TreeNodeReference,
    environment: &SchemaEnvironment,
) -> bool {
    let Some(condition) = conditional_mapping.get("$if") else {
        mark_schema_error(
            target_node,
            "invalid $conditional entry -> expected an $if condition".to_string(),
        );
        return false;
    };

    let Value::String(condition_source) = condition else {
        mark_schema_error(
            target_node,
            "invalid $if -> expected a Jinja expression, for example \"{{ type == 'database' }}\""
                .to_string(),
        );
        return false;
    };

    let scope = environment.scope_for(target_node);
    match evaluate_condition(condition_source, &scope) {
        Ok(condition_holds) => condition_holds,
        Err(reason) => {
            mark_schema_error(
                target_node,
                format!(
                    "$if '{}' could not be evaluated -> {}",
                    condition_source,
                    reason.describe()
                ),
            );
            false
        }
    }
}
