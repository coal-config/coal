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
//! The `$` directives that shape an object (`$inherit`, `$declare`/`$use`,
//! `$optional`, `$template`, `$each`) all operate on the *same* node and are
//! freely nestable: a `$template` may live inside an `$optional`, what it
//! writes may `$use` a declaration, and that declaration may carry a
//! `$template` of its own.
//!
//! `$each` is what a schema folder called `_each` becomes: the definition
//! every key of an object must match.

use crate::coal_tree::{Span, tree_to_yaml};
use crate::definition_scopes::{DefinitionScopes, directory_of_origin};
use crate::diagnostics::DiagnosticError;
use crate::directory_loader::load_configuration_directory;
use crate::jinja_engine::{
    TemplateScope, contains_template, render_template_as_yaml, render_value_template,
};
use crate::type_aliases::{TreeNodeReference, VirtualFileSystemReference};
use crate::yaml_values::{deep_merge_yaml_values, extract_string_list_from_value};
use serde_yaml::{Mapping, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::path::Path;
use std::rc::Rc;

// =====================================================================
// SCHEMA LOADING
// =====================================================================

/// Reads the schema directory into one schema block.
///
/// `_schema/` is laid out exactly like the configuration it describes: a file
/// named after a field describes that field, a folder is a level of nesting,
/// and `_each` describes every key of the object holding it. The root of
/// `_schema/` is therefore the schema of the root of the configuration, and
/// there is no entry file to name.
///
/// The directory is read by the ordinary loader, so everything that holds for
/// a configuration folder holds here too — `_self`, `_each`, `_definitions`,
/// and files or folders written either way round.
pub fn load_schema_block_from_directory(
    schema_directory_path: &str,
    virtual_file_system: VirtualFileSystemReference,
) -> Result<Option<SchemaBlock>, Box<dyn Error>> {
    if !Path::new(schema_directory_path).is_dir() {
        return Ok(None);
    }

    let loaded = load_configuration_directory(schema_directory_path, virtual_file_system)?;

    // Read off the tree before it becomes a plain value, which is the last
    // moment the schema still knows which file each block was written in.
    let origins = SchemaOrigins::of_tree(&loaded.root_node);

    let schema_root = tree_to_yaml(loaded.root_node);
    let definitions = SchemaDefinitions::read(&loaded.definition_scopes, &origins);

    let mut visited_paths = HashSet::new();
    let resolved_root = resolve_schema_reuse(
        &schema_root,
        "",
        &schema_root,
        &definitions,
        &mut visited_paths,
    )?;

    Ok(Some(SchemaBlock {
        root: resolved_root,
        unresolved_root: schema_root,
        definitions,
        origins,
    }))
}

/// A schema directory, read and resolved.
pub struct SchemaBlock {
    /// The schema itself, with every `$inherit` and `$include` merged in.
    pub root: Value,
    /// The schema tree before resolution, which is what a `$inherit` written
    /// by a `$template` is resolved against.
    pub unresolved_root: Value,
    /// The definitions, for the same reason.
    pub definitions: SchemaDefinitions,
    /// Where each block of the schema was written.
    pub origins: SchemaOrigins,
}

/// The `_definitions` of the schema directory, as each part of the schema sees
/// them.
///
/// A `$include` in the schema reads the definitions of the directory its file
/// sits in and of every directory above it inside `_schema/`, the deeper ones
/// overriding — the same rule a configuration follows. The schema is resolved
/// as plain values, so the view of every directory that holds schema is read
/// once, up front.
#[derive(Clone, Default)]
pub struct SchemaDefinitions {
    root: Value,
    by_directory: BTreeMap<String, Value>,
    origins: SchemaOrigins,
}

impl SchemaDefinitions {
    fn read(scopes: &DefinitionScopes, origins: &SchemaOrigins) -> Self {
        let mut by_directory = BTreeMap::new();
        for (file, _) in origins.entries.values() {
            let directory = directory_of_origin(file);
            if !by_directory.contains_key(&directory) {
                let visible = tree_to_yaml(scopes.visible_from_directory(&directory));
                by_directory.insert(directory, visible);
            }
        }

        Self {
            root: tree_to_yaml(scopes.visible_from_root()),
            by_directory,
            origins: origins.clone(),
        }
    }

    /// What schema at `schema_path` can include.
    fn visible_at(&self, schema_path: &str) -> &Value {
        self.origins
            .nearest(schema_path)
            .and_then(|(file, _)| self.by_directory.get(&directory_of_origin(&file)))
            .unwrap_or(&self.root)
    }
}

/// The file and line each part of the schema was written in, keyed by the path
/// through the schema that reaches it.
///
/// A missing field is unlike every other finding: there is no line to point
/// at, because the mistake is an absence. The one place that always exists and
/// always has a line is the schema that asked for the field, which is what
/// this is for.
#[derive(Clone, Default)]
pub struct SchemaOrigins {
    entries: BTreeMap<String, (String, Span)>,
}

impl SchemaOrigins {
    /// Reads the origins off the loaded schema tree.
    fn of_tree(root_node: &TreeNodeReference) -> Self {
        let mut entries = BTreeMap::new();
        collect_schema_origins(root_node, "", &mut entries);
        Self { entries }
    }

    /// Where the schema at this path was written, falling back to the nearest
    /// block above it.
    ///
    /// The fallback is what covers a block that arrived through `$inherit` or
    /// `$include`: the fields it contributes have no path of their own in the
    /// schema as written, and the block that reused them is the next best
    /// thing to point at.
    fn nearest(&self, schema_path: &str) -> Option<(String, Span)> {
        let mut candidate = schema_path;

        loop {
            if let Some(entry) = self.entries.get(candidate) {
                return Some(entry.clone());
            }
            match candidate.rfind('.') {
                Some(index) => candidate = &candidate[..index],
                None if candidate.is_empty() => return None,
                None => candidate = "",
            }
        }
    }
}

/// Walks the schema tree recording where each block was written. Only a block
/// that came from a file is recorded — a folder has no line to report.
fn collect_schema_origins(
    node: &TreeNodeReference,
    schema_path: &str,
    entries: &mut BTreeMap<String, (String, Span)>,
) {
    let (origin, span, children) = {
        let node_borrowed = node.borrow();
        (
            node_borrowed.origin.clone(),
            node_borrowed.span.clone(),
            node_borrowed.children.clone(),
        )
    };

    if let (Some(origin), Some(span)) = (origin, span)
        && origin_names_a_file(&origin)
    {
        entries.insert(schema_path.to_string(), (origin, span));
    }

    for child in children {
        let child_key = child.borrow().key.clone();
        let Some(child_key) = child_key else {
            continue;
        };
        collect_schema_origins(&child, &join_schema_path(schema_path, &child_key), entries);
    }
}

/// Whether an origin names a file rather than the folder a field came from.
/// Only a file has a line to report a finding on.
fn origin_names_a_file(origin: &str) -> bool {
    let lowered = origin.to_ascii_lowercase();
    lowered.ends_with(".yaml") || lowered.ends_with(".yml")
}

/// Extends a schema path with one more step. The root of the schema has no
/// path at all, which is what makes the first step the whole path.
fn join_schema_path(schema_path: &str, step: &str) -> String {
    if schema_path.is_empty() {
        step.to_string()
    } else {
        format!("{}.{}", schema_path, step)
    }
}

/// How a schema path is named in a report when there is no file to name.
fn label_of_schema_path(schema_path: &str) -> String {
    if schema_path.is_empty() {
        "_schema".to_string()
    } else {
        format!("_schema.{}", schema_path)
    }
}

/// Deep-merges every `$inherit` and `$include` in the schema into the block
/// that declares it.
///
/// The two differ only in where they look. `$inherit` names another part of
/// the schema tree, so a field can be described as "the same as that one";
/// `$include` names a definition in `_schema/_definitions/`, which is where a
/// shape lives that is *not* a field of the configuration and exists purely to
/// be reused. Both merge underneath the block, so what a block says for itself
/// wins, and both accept a list merged left to right.
///
/// The visited set breaks a chain that leads back to where it started.
fn resolve_schema_reuse(
    schema_value: &Value,
    schema_path: &str,
    schema_root: &Value,
    definitions: &SchemaDefinitions,
    visited_paths: &mut HashSet<String>,
) -> Result<Value, Box<dyn Error>> {
    let Value::Mapping(mapping) = schema_value else {
        if let Value::Sequence(sequence) = schema_value {
            let mut resolved = Vec::new();
            for item in sequence {
                resolved.push(resolve_schema_reuse(
                    item,
                    schema_path,
                    schema_root,
                    definitions,
                    visited_paths,
                )?);
            }
            return Ok(Value::Sequence(resolved));
        }
        return Ok(schema_value.clone());
    };

    let mut resolved_mapping = Mapping::new();
    for (key, value) in mapping {
        let child_path = match key.as_str() {
            Some(step) => join_schema_path(schema_path, step),
            None => schema_path.to_string(),
        };
        resolved_mapping.insert(
            key.clone(),
            resolve_schema_reuse(value, &child_path, schema_root, definitions, visited_paths)?,
        );
    }

    let mut merged = Value::Mapping(Mapping::new());
    let mut reused_anything = false;

    // What is reused is resolved as if it were written here, so an include
    // inside it reads the definitions this place sees.
    for (directive, source, source_name) in [
        ("$inherit", schema_root, "the schema tree"),
        ("$include", definitions.visible_at(schema_path), "the _definitions visible there"),
    ] {
        let Some(reused) = resolved_mapping.remove(directive) else {
            continue;
        };
        reused_anything = true;

        for reused_path in extract_string_list_from_value(&reused) {
            let visit_key = format!("{}:{}", directive, reused_path);
            if !visited_paths.insert(visit_key.clone()) {
                continue;
            }

            let Some(target) = schema_block_at_path(source, &reused_path) else {
                return Err(format!(
                    "Schema {} names '{}', which {} does not have.",
                    directive, reused_path, source_name
                )
                .into());
            };

            let resolved_target = resolve_schema_reuse(
                &target,
                schema_path,
                schema_root,
                definitions,
                visited_paths,
            )?;
            merged = deep_merge_yaml_values(&merged, &resolved_target);
            visited_paths.remove(&visit_key);
        }
    }

    if !reused_anything {
        return Ok(Value::Mapping(resolved_mapping));
    }

    // A definition that is not an object — a type name, or a list of allowed
    // values — simply *is* the field's schema.
    if !merged.is_mapping() && resolved_mapping.is_empty() {
        return Ok(merged);
    }

    Ok(deep_merge_yaml_values(&merged, &Value::Mapping(resolved_mapping)))
}

/// Walks a dotted path into the schema tree.
fn schema_block_at_path(schema_root: &Value, path: &str) -> Option<Value> {
    // Walk by reference and copy only the block that was asked for: copying
    // the whole tree at every step made each `$include` cost as much as the
    // entire `_definitions` namespace.
    let mut current = schema_root;

    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        let Value::Mapping(mapping) = current else {
            return None;
        };
        current = mapping.get(segment)?;
    }
    Some(current.clone())
}

// =====================================================================
// SCHEMA ENVIRONMENT
// =====================================================================

/// What one validation run shares: the tree being validated.
///
/// A schema template names the fields it reads, so evaluating one costs no
/// more than fetching those fields. Nothing has to be prepared in advance and
/// nothing has to be thrown away when the schema engine writes a default.
pub struct SchemaEnvironment {
    root_node: TreeNodeReference,
    /// The schema tree and the definitions namespace, kept so that schema a
    /// `$template` *writes* can use `$inherit` and `$include` too. Schema
    /// written by hand has both resolved when the directory is read; schema
    /// written by a template does not exist until validation runs.
    schema_root: Value,
    definitions: SchemaDefinitions,
    /// Where each block of the schema was written, so a field the
    /// configuration never provided can be reported on the line that asked
    /// for it.
    schema_origins: SchemaOrigins,
    /// The fields a schema asked for and the configuration does not have,
    /// keyed by path so the same field declared by two blocks is reported
    /// once and the report comes out in a stable order.
    missing_fields: RefCell<BTreeMap<String, DiagnosticError>>,
    /// Schema a `$template` wrote, with its `$inherit`/`$include` resolved,
    /// keyed by where in the schema it was written and what it said. The same
    /// template writes the same schema for every object it validates, and
    /// resolving it copies every definition it reuses, so it is done once.
    resolved_writes: RefCell<HashMap<(String, Value), Rc<Value>>>,
}

impl SchemaEnvironment {
    pub fn new(root_node: TreeNodeReference) -> Self {
        Self {
            root_node,
            schema_root: Value::Null,
            definitions: SchemaDefinitions::default(),
            schema_origins: SchemaOrigins::default(),
            missing_fields: RefCell::new(BTreeMap::new()),
            resolved_writes: RefCell::new(HashMap::new()),
        }
    }

    /// Supplies the schema tree, the definitions a `$template` may reuse, and
    /// where each block of the schema was written.
    pub fn with_schema(
        mut self,
        schema_root: Value,
        definitions: SchemaDefinitions,
        schema_origins: SchemaOrigins,
    ) -> Self {
        self.schema_root = schema_root;
        self.definitions = definitions;
        self.schema_origins = schema_origins;
        self
    }

    /// Resolves `$inherit` and `$include` in schema a template just wrote.
    fn resolve_reuse_in(
        &self,
        written_schema: Value,
        schema_path: &str,
    ) -> Result<Rc<Value>, Box<dyn Error>> {
        if matches!(self.schema_root, Value::Null) {
            return Ok(Rc::new(written_schema));
        }

        let cache_key = (schema_path.to_string(), written_schema);
        if let Some(resolved) = self.resolved_writes.borrow().get(&cache_key) {
            return Ok(Rc::clone(resolved));
        }

        let mut visited_paths = HashSet::new();
        let resolved = Rc::new(resolve_schema_reuse(
            &cache_key.1,
            schema_path,
            &self.schema_root,
            &self.definitions,
            &mut visited_paths,
        )?);
        self.resolved_writes
            .borrow_mut()
            .insert(cache_key, Rc::clone(&resolved));
        Ok(resolved)
    }

    /// The fields the schema declared and the configuration never provided.
    ///
    /// They are collected while the schema is walked rather than written into
    /// the tree, so validating a configuration cannot change it: `coal` and
    /// `coal --validate` compile the same thing.
    pub fn into_missing_field_diagnostics(self) -> Vec<DiagnosticError> {
        self.missing_fields.into_inner().into_values().collect()
    }

    /// Records a field the schema asked for and the configuration does not
    /// have.
    ///
    /// Where such a finding belongs is not obvious, because the mistake is an
    /// absence and an absence has no line. The object that should have held
    /// the field is the first choice — a `.yaml` file, or the `_self.yaml` a
    /// folder speaks for itself with. A folder without one has no line at all,
    /// and there the finding goes to the schema that asked for the field,
    /// which always exists and always has one. The message names the other
    /// side either way, and says where the field would go.
    fn record_missing_field(
        &self,
        parent_node: &TreeNodeReference,
        field_name: &str,
        expected_type: &str,
        schema_path: &str,
    ) {
        let (parent_path, parent_origin, parent_span) = {
            let parent_borrowed = parent_node.borrow();
            (
                parent_borrowed.get_path(),
                parent_borrowed.origin.clone(),
                parent_borrowed.span.clone(),
            )
        };

        // The root has no path of its own, so a field of it is named on its own.
        let parent_is_the_root = parent_path.is_empty() || parent_path == "ROOT";
        let field_path = if parent_is_the_root {
            field_name.to_string()
        } else {
            format!("{}.{}", parent_path, field_name)
        };

        let parent_file = parent_origin.clone().filter(|origin| origin_names_a_file(origin));
        let schema_anchor = self
            .schema_origins
            .nearest(&join_schema_path(schema_path, field_name));

        let (origin, span) = match (&parent_file, &schema_anchor) {
            (Some(parent_file), _) => (parent_file.clone(), parent_span),
            (None, Some((schema_file, schema_span))) => {
                (schema_file.clone(), Some(schema_span.clone()))
            }
            (None, None) => (
                parent_origin.unwrap_or_else(|| "Unknown Origin".to_string()),
                parent_span,
            ),
        };

        // Naming the schema file twice says nothing the first mention did
        // not, so where the finding already sits on it the path through the
        // schema is named instead.
        let declared_by = match &schema_anchor {
            Some((schema_file, _)) if schema_file != &origin => schema_file.clone(),
            _ => label_of_schema_path(&join_schema_path(schema_path, field_name)),
        };

        // Only a folder gets told where to write the field: it is the one case
        // with no line to add it to, and the answer is to create a file.
        let where_to_write = match parent_file {
            Some(_) => String::new(),
            None => {
                let folder = match parent_is_the_root {
                    true => String::new(),
                    false => format!("{}/", parent_path.replace('.', "/")),
                };
                format!(
                    " -> write it in '{}_self.yaml' or '{}{}.yaml'",
                    folder, folder, field_name
                )
            }
        };

        self.missing_fields
            .borrow_mut()
            .entry(field_path.clone())
            .or_insert_with(|| {
                // Where to write the field names the object that is missing
                // it, which is a different object for every selection, so it
                // is said in the message and left out of what makes two
                // findings the same one.
                let detail = format!("[{}] value: None -> expected {}", declared_by, expected_type);
                let message = format!(
                    "[{}] {} {}{}",
                    origin, field_path, detail, where_to_write
                );
                DiagnosticError {
                    error_type: "MissingMandatoryField".to_string(),
                    field_path,
                    file_origin: origin,
                    message,
                    detail,
                    span,
                }
            });
    }

    /// The scope a template that describes the field `node` is evaluated in.
    fn scope_for(&self, node: &TreeNodeReference) -> TemplateScope {
        TemplateScope::for_node(node, &self.root_node).relaxed()
    }

    /// The scope a template that describes an object rather than one of its
    /// fields is evaluated in, so that `parent()` means the same thing there
    /// as it does in the field definitions beside it.
    fn scope_for_object(&self, node: &TreeNodeReference) -> TemplateScope {
        TemplateScope::for_object_block(node, &self.root_node).relaxed()
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
    schema_path: &str,
    environment: &SchemaEnvironment,
) -> Result<(), Box<dyn Error>> {
    {
        let mut target_borrowed = target_node.borrow_mut();
        target_borrowed.schema_origin = Some(schema_path.to_string());
        target_borrowed.allowed = false;
    }

    match schema_definition {
        Value::String(type_text) => apply_scalar_type_schema(
            target_node,
            type_text,
            schema_path,
            environment,
        ),
        Value::Sequence(schema_sequence) => apply_sequence_schema(
            target_node,
            schema_sequence,
            schema_path,
            environment,
        ),
        Value::Mapping(schema_mapping) => apply_mapping_schema(
            target_node,
            schema_mapping,
            schema_path,
            environment,
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
    schema_path: &str,
    environment: &SchemaEnvironment,
) -> Result<(), Box<dyn Error>> {
    if type_text.starts_with('[') && type_text.ends_with(']') && !contains_template(type_text) {
        return apply_bracketed_list_schema(
            target_node,
            type_text,
            schema_path,
            environment,
        );
    }

    if is_bracketed_template(type_text) {
        return apply_bracketed_list_schema(
            target_node,
            type_text,
            schema_path,
            environment,
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
    schema_path: &str,
    environment: &SchemaEnvironment,
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
            schema_path,
            environment,
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
    schema_path: &str,
    environment: &SchemaEnvironment,
) -> Result<(), Box<dyn Error>> {
    if let Some(element_schema) = sequence_element_schema(schema_sequence) {
        return apply_list_element_schema(
            target_node,
            element_schema,
            schema_path,
            environment,
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
    schema_path: &str,
    environment: &SchemaEnvironment,
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
            schema_path,
            environment,
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
/// `$template` are blocks applied on top of it, and they inherit these
/// settings so that, for instance, a `$template` nested inside an `$optional`
/// keeps the fields it writes optional.
#[derive(Clone, Copy)]
struct BlockOptions {
    /// Whether fields the block declares but the configuration omits are
    /// reported as missing.
    report_missing_fields: bool,
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
            report_missing_fields: true,
            fields_are_optional: false,
            layered: false,
        }
    }

    /// The settings of an `$optional` block: its fields are permitted, never
    /// required, so nothing is injected for the ones that are absent.
    fn optional(self) -> Self {
        Self {
            report_missing_fields: false,
            fields_are_optional: true,
            layered: true,
        }
    }

    /// The settings of a block a `$template` wrote: it says what else the
    /// object needs, under whatever rules the block that carries it works.
    fn written_by_template(self) -> Self {
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
            report_missing_fields: true,
            fields_are_optional: false,
            layered: true,
        }
    }
}

fn apply_mapping_schema(
    target_node: TreeNodeReference,
    schema_mapping: &Mapping,
    schema_path: &str,
    environment: &SchemaEnvironment,
) -> Result<(), Box<dyn Error>> {
    // Reuse was resolved when the schema directory was read, so what a
    // block says here is all it says.
    let effective_schema = Value::Mapping(schema_mapping.clone());

    // "$use" may name a definition that is not an object at all — a type name
    // or an allowed-value list. The field then simply *is* that definition.
    let Value::Mapping(effective_mapping) = effective_schema else {
        return apply_schema_definition_to_tree_node(
            target_node,
            &effective_schema,
            schema_path,
            environment,
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
        schema_path,
        environment,
        BlockOptions::declaring(),
    )
}

/// Applies a block that sits on top of an object that has already been
/// validated: an `$optional` block, or the `$then` of a matching
/// `$template`. Such a block only ever grants more — it never takes back
/// what the enclosing schema already permitted.
fn apply_nested_schema_block(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_path: &str,
    environment: &SchemaEnvironment,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    // Reuse was resolved when the schema directory was read, so what a
    // block says here is all it says.
    let effective_schema = Value::Mapping(block_mapping.clone());

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
        schema_path,
        environment,
        options,
    )
}

/// The directives a schema block may carry. Everything else starting with a
/// "$" is a mistake worth reporting rather than ignoring in silence.
const KNOWN_BLOCK_DIRECTIVES: [&str; 4] = ["$inherit", "$optional", "$template", "$each"];

/// Reports any "$" key the schema engine does not know, so a typo or an
/// outdated directive name surfaces instead of being quietly skipped.
fn check_block_directives_are_known(block_mapping: &Mapping, target_node: &TreeNodeReference) {
    for key in block_mapping.keys() {
        let Value::String(directive_name) = key else {
            continue;
        };
        if !directive_name.starts_with('$')
            || KNOWN_BLOCK_DIRECTIVES.contains(&directive_name.as_str())
        {
            continue;
        }

        let hint = match directive_name.as_str() {
            "$conditional" | "$conditional_fields" => {
                " -> conditional schema is now written with $template".to_string()
            }
            "$keyed_by" => {
                " -> the keys an object may carry are now described with a $template, which can say anything a fixed key list could and rather more"
                    .to_string()
            }
            "$each" => {
                " -> the definition every key of an object must match is now an `_each` folder beside the field, exactly as it is in a configuration"
                    .to_string()
            }
            "$schema" => {
                " -> a schema reuses another with $inherit, naming it by its path within _schema"
                    .to_string()
            }
            "$ignore" => {
                " -> $ignore leaves fields out of a configuration; a schema says what is permitted with $optional, or an _optional folder"
                    .to_string()
            }
            "$declare" | "$use" => {
                " -> a reusable shape now lives in _schema/_definitions/ and is pulled in with $include, exactly as it is in a configuration"
                    .to_string()
            }
            "$select" | "$keys" | "$override" => {
                " -> these are gone. Layering across the configuration is declared once in _structure.yaml, and a schema describes the merged result"
                    .to_string()
            }
            "$advanced_fields" => {
                " -> a schema describes the configuration, it does not write to it. Declare the type as the field's definition, its allowed values as a list or a template, and give the field a value in the configuration instead of a default_value"
                    .to_string()
            }
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
/// nestable: `$optional` and `$template` route straight back into this
/// same function.
fn apply_prepared_schema_block(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_path: &str,
    environment: &SchemaEnvironment,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    check_block_directives_are_known(block_mapping, &target_node);

    if options.report_missing_fields {
        record_missing_schema_fields(block_mapping, &target_node, schema_path, environment);
    }

    apply_field_schema_to_children(
        block_mapping,
        Rc::clone(&target_node),
        schema_path,
        environment,
        options,
    )?;

    apply_optional_block(
        block_mapping,
        Rc::clone(&target_node),
        schema_path,
        environment,
        options,
    )?;

    apply_template_block(
        block_mapping,
        target_node,
        schema_path,
        environment,
        options,
    )?;

    Ok(())
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

/// Matches each child of the target node against the block and applies the
/// corresponding definition. Precedence for each child:
///   1. an explicitly declared field,
///   2. otherwise the `$each` definition, when the block gives one,
///   3. otherwise a '$'-prefixed key is passed through for later phases,
///   4. otherwise the key is left as it was (reported as FieldNotAllowed
///      unless another block permits it).
fn apply_field_schema_to_children(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_path: &str,
    environment: &SchemaEnvironment,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    let every_key_schema = block_mapping.get("$each");
    let children = target_node.borrow().children.clone();

    for child in children {
        let child_key = match child.borrow().key.clone() {
            Some(key) => key,
            None => continue,
        };

        // A system directive is not configuration. It is still in the tree
        // because a later phase consumes it, and validating it as though it
        // were a field would report an `_each` template for every mandatory
        // field the thing it is a template *for* has to have.
        if child_key.starts_with('$') {
            child.borrow_mut().allowed = true;
            continue;
        }

        if let Some(field_schema) = block_mapping.get(Value::String(child_key.clone())) {
            apply_field_schema_to_child(
                &child,
                field_schema,
                &join_schema_path(schema_path, &child_key),
                environment,
                options,
            )?;
            if options.fields_are_optional {
                child.borrow_mut().optional = true;
            }
            continue;
        }

        if every_key_schema.is_some() {
            if let Some(any_field_schema) = every_key_schema {
                apply_schema_definition_to_tree_node(
                    Rc::clone(&child),
                    any_field_schema,
                    &join_schema_path(schema_path, "$each"),
                    environment,
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
    schema_path: &str,
    environment: &SchemaEnvironment,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    let child_already_checked = child.borrow().object_fields_checked;

    if let (true, true, Value::Mapping(field_mapping)) =
        (options.layered, child_already_checked, field_schema)
    {
        return apply_layered_mapping_schema(
            Rc::clone(child),
            field_mapping,
            schema_path,
            environment,
        );
    }

    apply_schema_definition_to_tree_node(
        Rc::clone(child),
        field_schema,
        schema_path,
        environment,
    )
}

/// Applies an object definition on top of one that has already been applied to
/// the same node: the fields it declares are added, and the ones the earlier
/// definition permitted are left alone.
fn apply_layered_mapping_schema(
    target_node: TreeNodeReference,
    schema_mapping: &Mapping,
    schema_path: &str,
    environment: &SchemaEnvironment,
) -> Result<(), Box<dyn Error>> {
    // Reuse was resolved when the schema directory was read, so what a
    // block says here is all it says.
    let effective_schema = Value::Mapping(schema_mapping.clone());

    let Value::Mapping(effective_mapping) = effective_schema else {
        // The definition turned out not to be an object after all, so there is
        // nothing to layer: apply it the ordinary way.
        return apply_schema_definition_to_tree_node(
            target_node,
            &effective_schema,
            schema_path,
            environment,
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
        schema_path,
        environment,
        BlockOptions::inside_layered_field(),
    )
}

/// Reports every field the schema declares that the configuration does not
/// provide. Nothing is written to the tree: a schema says what it expected,
/// it does not supply it.
fn record_missing_schema_fields(
    block_mapping: &Mapping,
    target_node: &TreeNodeReference,
    schema_path: &str,
    environment: &SchemaEnvironment,
) {
    let existing_field_names: Vec<String> = target_node
        .borrow()
        .children
        .iter()
        .filter_map(|child| child.borrow().key.clone())
        .collect();

    for expected_field_name in collect_expected_field_names(block_mapping) {
        if existing_field_names.contains(&expected_field_name) {
            continue;
        }

        let expected_type = block_mapping
            .get(Value::String(expected_field_name.clone()))
            .map(describe_expected_type)
            .unwrap_or_else(|| "any".to_string());

        environment.record_missing_field(
            target_node,
            &expected_field_name,
            &expected_type,
            schema_path,
        );
    }
}

/// How a schema definition is named in a "this field is missing" report.
fn describe_expected_type(definition: &Value) -> String {
    match definition {
        Value::String(type_text) if contains_template(type_text) => "string".to_string(),
        Value::String(type_text) if type_text.starts_with('[') => "list".to_string(),
        Value::String(type_text) => type_text.clone(),
        Value::Sequence(_) => "list".to_string(),
        Value::Mapping(_) => "object".to_string(),
        _ => "any".to_string(),
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

    expected_field_names
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
/// `$use`, `$each`, `$keyed_by` and `$template`.
fn apply_optional_block(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_path: &str,
    environment: &SchemaEnvironment,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    let Some(optional_definition) = block_mapping.get("$optional") else {
        return Ok(());
    };

    let Value::Mapping(optional_mapping) = optional_definition else {
        mark_schema_error(
            &target_node,
            "invalid $optional -> expected a mapping of field definitions. To make a field itself optional, describe it inside the enclosing object's $optional (or its _optional folder) rather than marking it."
                .to_string(),
        );
        return Ok(());
    };

    apply_nested_schema_block(
        optional_mapping,
        target_node,
        &join_schema_path(schema_path, "$optional"),
        environment,
        options.optional(),
    )
}

// =====================================================================
// $template
// =====================================================================

/// Renders a schema block's "$template" and applies whatever schema it wrote
/// on top of the block that carries it.
///
/// This is how a schema reacts to the data it is validating. The template is
/// ordinary Jinja, evaluated in the scope of the object being described — so
/// `parent()` is that object, exactly as it is in the field definitions beside
/// it — and its output is read back as schema:
///
/// ```yaml
/// type: string
/// $template: |
///   {%- if parent().type in ['database', 'cache'] %}
///   credentials: string
///   {%- endif %}
/// ```
///
/// Whatever it produces is a full schema block, so it may itself declare
/// fields, `$optional`, `$each`, `$use` — or another `$template`.
fn apply_template_block(
    block_mapping: &Mapping,
    target_node: TreeNodeReference,
    schema_path: &str,
    environment: &SchemaEnvironment,
    options: BlockOptions,
) -> Result<(), Box<dyn Error>> {
    let Some(template_definition) = block_mapping.get("$template") else {
        return Ok(());
    };

    let Value::String(template_source) = template_definition else {
        mark_schema_error(
            &target_node,
            "invalid $template -> expected a Jinja template that writes schema".to_string(),
        );
        return Ok(());
    };

    let scope = environment.scope_for_object(&target_node);
    let rendered_schema = match render_template_as_yaml(template_source, &scope) {
        Ok(rendered_schema) => rendered_schema,
        Err(reason) => {
            mark_schema_error(
                &target_node,
                format!("$template could not be rendered -> {}", reason.describe()),
            );
            return Ok(());
        }
    };

    let rendered_schema = match environment.resolve_reuse_in(rendered_schema, schema_path) {
        Ok(rendered_schema) => rendered_schema,
        Err(reason) => {
            mark_schema_error(
                &target_node,
                format!("$template wrote schema that could not be resolved -> {}", reason),
            );
            return Ok(());
        }
    };

    match rendered_schema.as_ref() {
        // A template that decides nothing applies is doing its job.
        Value::Null => Ok(()),
        Value::Mapping(rendered_mapping) => apply_nested_schema_block(
            rendered_mapping,
            target_node,
            schema_path,
            environment,
            options.written_by_template(),
        ),
        _ => {
            mark_schema_error(
                &target_node,
                "invalid $template -> it produced something other than a mapping of fields"
                    .to_string(),
            );
            Ok(())
        }
    }
}
