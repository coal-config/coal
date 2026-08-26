//! The Jinja engine COAL evaluates every template with.
//!
//! COAL embeds a complete Jinja2 engine (minijinja), so anything Jinja can
//! express — filters, tests, arithmetic, `{% for %}` and `{% if %}` blocks,
//! `{% set %}`, macros — is available in configuration values, in directive
//! arguments, and in schema definitions.
//!
//! # Scope
//!
//! A template is always evaluated *somewhere* in the tree, and the field it
//! sits on decides what plain names mean. Lookup walks outward from that
//! field: its own children first, then its siblings, then the siblings of its
//! parent, and so on up to the configuration root. The first scope that
//! declares the name wins, so a schema can say `{{ ROOT.transports[transport]
//! }}` and have `transport` mean "the value of the sibling field named
//! transport".
//!
//! Two names are always available regardless of scope:
//!
//! | Name | Refers to |
//! | --- | --- |
//! | `ROOT` | the whole configuration tree |
//! | `PARENT`, `PARENT2`, `PARENT3`, ... | the object N levels up (`PARENT` is the one the field sits in) |
//!
//! # Dependencies
//!
//! A template may only read values that are themselves resolved. Every
//! template names the fields it reads, so before rendering anything the engine
//! looks those fields up and reports [`TemplateError::WaitingOn`] if one of
//! them is still a template. The caller then tries again later, once the field
//! it waited for has a value. This is what lets fields reference each other in
//! any order, and what keeps a half-resolved reference from ever being spliced
//! into another field.
//!
//! # Typing
//!
//! A value that consists of exactly one `{{ ... }}` expression adopts the
//! expression's own type: a list stays a list, a mapping stays a mapping, an
//! integer stays an integer. A value that mixes text with expressions renders
//! to a string. A value that uses `{% ... %}` blocks renders to text which is
//! then parsed back as YAML, so a loop that emits list items produces a real
//! list.

use crate::coal_tree::tree_to_template_value;
use crate::type_aliases::TreeNodeReference;
use minijinja::value::{Value as JinjaValue, ValueKind};
use minijinja::{Environment, Error as JinjaError, ErrorKind, State, UndefinedBehavior};
use serde_yaml::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;
use std::sync::OnceLock;

// =====================================================================
// OUTCOMES
// =====================================================================

/// Why a template did not produce a value.
#[derive(Debug, Clone)]
pub enum TemplateError {
    /// The template reads a field that is itself still a template. Rendering
    /// it now would substitute a reference instead of a value, so the caller
    /// should come back once that field has been resolved. Carries the path of
    /// the field being waited for.
    WaitingOn(String),
    /// The template cannot be rendered at all: a name that does not exist, a
    /// syntax error, a filter applied to the wrong kind of value.
    Failed(String),
}

impl TemplateError {
    /// A one-line explanation for diagnostics and warnings.
    pub fn describe(&self) -> String {
        match self {
            TemplateError::WaitingOn(field_path) => {
                format!("it reads '{}', which is itself unresolved", field_path)
            }
            TemplateError::Failed(reason) => reason.clone(),
        }
    }
}

// =====================================================================
// TEMPLATE DETECTION
// =====================================================================

/// Whether the text contains anything the Jinja engine would act on: an
/// expression (`{{ ... }}`) or a statement block (`{% ... %}`).
pub fn contains_template(text: &str) -> bool {
    text.contains("{{") || text.contains("{%")
}

/// Whether the text uses statement blocks, which is what makes a rendered
/// result eligible to be read back as YAML.
fn contains_statement_block(text: &str) -> bool {
    text.contains("{%")
}

/// Returns the inner source when the whole text is a single `{{ ... }}`
/// expression and nothing else. Such a value adopts the expression's own type
/// instead of being turned into a string.
pub fn whole_value_expression(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let inner = trimmed.strip_prefix("{{")?.strip_suffix("}}")?;

    // Anything after the first closing delimiter means the value is a
    // concatenation ("{{ a }}-{{ b }}"), which renders as text instead.
    if inner.contains("}}") {
        return None;
    }
    Some(inner)
}

/// Whether any string anywhere inside the value still holds a template.
pub fn value_contains_template(value: &Value) -> bool {
    match value {
        Value::String(text) => contains_template(text),
        Value::Sequence(sequence) => sequence.iter().any(value_contains_template),
        Value::Mapping(mapping) => mapping.values().any(value_contains_template),
        _ => false,
    }
}

/// Whether anything in the subtree is still an unresolved template.
///
/// Three things do not count. A field the engine has already finished with is
/// resolved even if its value looks like a template, which is what a
/// `{% raw %}` block produces. System directives never reach the compiled
/// output, so a template inside one cannot be what a field is waiting for. And
/// `reading_node` — the field asking the question — is skipped, because a
/// field that reads the object it lives in would otherwise be waiting for
/// itself.
pub fn subtree_contains_template(
    node: &TreeNodeReference,
    reading_node: &TreeNodeReference,
) -> bool {
    if Rc::ptr_eq(node, reading_node) {
        return false;
    }

    let node_borrowed = node.borrow();

    if node_borrowed.children.is_empty() {
        return !node_borrowed.template_resolved
            && node_borrowed
                .value
                .as_ref()
                .is_some_and(value_contains_template);
    }

    node_borrowed.children.iter().any(|child| {
        let is_directive = child
            .borrow()
            .key
            .as_deref()
            .is_some_and(|key| key.starts_with('$'));
        !is_directive && subtree_contains_template(child, reading_node)
    })
}

/// Whether the template asks to emit Jinja delimiters as literal text, which
/// `{% raw %}` is the way to do. Such a result is allowed to look like a
/// template; any other result that does is a reference that did not resolve.
pub fn emits_literal_delimiters(template: &str) -> bool {
    template.contains("{% raw %}") || template.contains("{%- raw %}")
}

/// A subtree as templates see it, converted once into the engine's own
/// representation — which is cheap to clone, so one prepared tree can serve
/// every field in a resolution pass.
pub fn prepare_tree_for_templates(node: TreeNodeReference) -> JinjaValue {
    JinjaValue::from_serialize(tree_to_template_value(node))
}

// =====================================================================
// SCOPE
// =====================================================================

/// The chain of scopes a template is evaluated in: the field the template
/// lives on, then each enclosing node up to the configuration root.
///
/// Variables are materialized lazily — only the names a template actually
/// mentions are converted from tree nodes into values — so evaluating
/// `{{ paths.log_directory }}` never has to snapshot the rest of the tree.
pub struct TemplateScope {
    /// The node the template lives on, followed by every ancestor, ending at
    /// the configuration root.
    node_chain: Vec<TreeNodeReference>,
    /// The whole tree, ready to hand to the engine. Built on first use of
    /// `ROOT`, or supplied up front by a caller that already has one — sharing
    /// it is what keeps a pass over thousands of fields from rebuilding the
    /// tree thousands of times.
    root_value: RefCell<Option<JinjaValue>>,
    /// Whether reading the object the field lives in may go ahead even though
    /// something else inside it is unresolved. See [`TemplateScope::relaxed`].
    relaxed: bool,
}

impl TemplateScope {
    /// Builds the scope for a template written on `node`. `root_node` is used
    /// as the final scope even when `node` is not (yet) attached to it, which
    /// happens while includes are still being expanded.
    pub fn for_node(node: &TreeNodeReference, root_node: &TreeNodeReference) -> Self {
        let mut node_chain = Vec::new();
        let mut current = Some(Rc::clone(node));

        while let Some(node) = current {
            let parent = node
                .borrow()
                .parent
                .as_ref()
                .and_then(|weak_parent| weak_parent.upgrade());
            node_chain.push(node);
            current = parent;
        }

        if !node_chain
            .last()
            .is_some_and(|last| Rc::ptr_eq(last, root_node))
        {
            node_chain.push(Rc::clone(root_node));
        }

        Self {
            node_chain,
            root_value: RefCell::new(None),
            relaxed: false,
        }
    }

    /// Lets a field read the object it lives in even while something else
    /// inside that object is unresolved.
    ///
    /// Two fields that both read their own container — `{{ PARENT | length }}`
    /// beside `{{ PARENT.keys() }}` — each wait for the other and neither ever
    /// goes first. Once ordinary resolution has run out of things it can do,
    /// they are allowed to look at the container as it stands, which is what
    /// they were asking about in the first place. Reading a *particular* other
    /// field still waits, relaxed or not.
    pub fn relaxed(mut self) -> Self {
        self.relaxed = true;
        self
    }

    /// Reuses a tree that has already been prepared, so a resolution pass over
    /// many fields does not rebuild it for every one of them. Build it with
    /// [`prepare_tree_for_templates`].
    pub fn with_prepared_tree(self, root_value: JinjaValue) -> Self {
        *self.root_value.borrow_mut() = Some(root_value);
        self
    }

    fn root_value(&self) -> JinjaValue {
        if let Some(ref prepared_value) = *self.root_value.borrow() {
            return prepared_value.clone();
        }

        let root_node = Rc::clone(self.node_chain.last().expect("scope is never empty"));
        let prepared_value = prepare_tree_for_templates(root_node);
        *self.root_value.borrow_mut() = Some(prepared_value.clone());
        prepared_value
    }

    fn ancestor_node(&self, levels_up: usize) -> Option<TreeNodeReference> {
        self.node_chain.get(levels_up).cloned()
    }

    /// Finds a plain name by walking outward through the scopes and returning
    /// the first matching field.
    fn find_named_node(&self, name: &str) -> Option<TreeNodeReference> {
        for scope_node in &self.node_chain {
            let matching_child = scope_node
                .borrow()
                .children
                .iter()
                .find(|child| child.borrow().key.as_deref() == Some(name))
                .cloned();

            if matching_child.is_some() {
                return matching_child;
            }
        }
        None
    }

    /// Materializes only the variables the template mentions.
    fn build_context(&self, referenced_names: &HashSet<String>) -> BTreeMap<String, JinjaValue> {
        let mut context = BTreeMap::new();

        for name in referenced_names {
            if let Some(value) = self.resolve_variable(name) {
                context.insert(name.clone(), value);
            }
        }
        context
    }

    fn resolve_variable(&self, name: &str) -> Option<JinjaValue> {
        if name == "ROOT" {
            return Some(self.root_value());
        }

        let node = match parse_ancestor_variable(name) {
            Some(levels_up) => self.ancestor_node(levels_up),
            None => self.find_named_node(name),
        }?;
        Some(prepare_tree_for_templates(node))
    }

    /// Reports the first field a template reads that is itself still a
    /// template.
    ///
    /// `referenced_paths` are the dotted paths the engine found in the
    /// template, so `{{ ROOT.paths.log_dir }}/x` asks about exactly that one
    /// field rather than about the tree as a whole.
    fn first_unresolved_dependency(&self, referenced_paths: &HashSet<String>) -> Option<String> {
        let reading_node = self.node_chain.first()?;

        for referenced_path in referenced_paths {
            let Some(node) = self.find_dependency_node(referenced_path) else {
                continue;
            };

            if self.relaxed && self.is_enclosing_scope(&node) {
                continue;
            }

            if subtree_contains_template(&node, reading_node) {
                let field_path = node.borrow().get_path();
                return Some(if field_path.is_empty() {
                    referenced_path.clone()
                } else {
                    field_path
                });
            }
        }
        None
    }

    /// Whether the node is the field the template is written on, or one of the
    /// objects it sits inside.
    fn is_enclosing_scope(&self, node: &TreeNodeReference) -> bool {
        self.node_chain
            .iter()
            .any(|scope_node| Rc::ptr_eq(scope_node, node))
    }

    /// Resolves a dotted path to the node it reads. A path that only partly
    /// exists — because the template reached the rest of the way through a
    /// subscript or a filter — resolves to the deepest node it does reach,
    /// which is the smallest part of the tree the template could be reading.
    fn find_dependency_node(&self, referenced_path: &str) -> Option<TreeNodeReference> {
        let mut segments = referenced_path.split('.');
        let first_segment = segments.next()?;

        let mut current_node = if first_segment == "ROOT" {
            self.node_chain.last().cloned()
        } else {
            match parse_ancestor_variable(first_segment) {
                Some(levels_up) => self.ancestor_node(levels_up),
                None => self.find_named_node(first_segment),
            }
        }?;

        for segment in segments {
            let matching_child = current_node
                .borrow()
                .children
                .iter()
                .find(|child| child.borrow().key.as_deref() == Some(segment))
                .cloned();

            match matching_child {
                Some(child) => current_node = child,
                None => break,
            }
        }
        Some(current_node)
    }
}

/// Recognizes the "PARENT", "PARENT2", "PARENT3" ... family and returns how
/// many levels up the name points.
fn parse_ancestor_variable(name: &str) -> Option<usize> {
    let digits = name.strip_prefix("PARENT")?;
    if digits.is_empty() {
        return Some(1);
    }
    match digits.parse::<usize>() {
        Ok(0) | Err(_) => None,
        Ok(levels_up) => Some(levels_up),
    }
}

// =====================================================================
// RENDERING
// =====================================================================

/// Renders a template and returns the resulting text. Used wherever a
/// directive needs a string: include paths, inheritance targets, counter
/// names.
pub fn render_template_text(
    template: &str,
    scope: &TemplateScope,
) -> Result<String, TemplateError> {
    if !contains_template(template) {
        return Ok(template.to_string());
    }

    let environment = strict_environment();
    let compiled = environment
        .template_from_str(template)
        .map_err(failed_to_render)?;

    if let Some(field_path) =
        scope.first_unresolved_dependency(&compiled.undeclared_variables(true))
    {
        return Err(TemplateError::WaitingOn(field_path));
    }

    let context = scope.build_context(&compiled.undeclared_variables(false));
    compiled.render(context).map_err(failed_to_render)
}

/// Renders a field value and returns it with its natural type.
///
/// A value that is exactly one `{{ ... }}` expression keeps the expression's
/// own type, so a list stays a list and an integer stays an integer. A value
/// built from `{% ... %}` blocks is rendered and then read back as YAML, so a
/// loop that emits list items produces a real list. Everything else renders
/// to a string.
pub fn render_value_template(
    template: &str,
    scope: &TemplateScope,
) -> Result<Value, TemplateError> {
    if let Some(expression_source) = whole_value_expression(template) {
        return evaluate_expression(expression_source, scope);
    }

    let rendered_text = render_template_text(template, scope)?;

    // Text a `{% raw %}` block produced is meant to be text: reading it back as
    // YAML would turn a literal `{{ x }}` into a flow mapping.
    if contains_statement_block(template)
        && !emits_literal_delimiters(template)
        && let Some(structured_value) = parse_rendered_text_as_yaml(&rendered_text)
    {
        return Ok(structured_value);
    }

    Ok(Value::String(rendered_text))
}

/// Reads rendered text back as YAML, but only accepts a list or a mapping.
/// Scalars stay text: a rendered `yes` or `01` is far more likely to be a
/// string the user wrote than a boolean or an octal number they wanted.
fn parse_rendered_text_as_yaml(rendered_text: &str) -> Option<Value> {
    let parsed_value: Value = serde_yaml::from_str(rendered_text).ok()?;
    match parsed_value {
        Value::Sequence(_) | Value::Mapping(_) => Some(parsed_value),
        _ => None,
    }
}

/// Evaluates a single Jinja expression (the inside of a `{{ ... }}`) and
/// returns its value.
pub fn evaluate_expression(
    expression_source: &str,
    scope: &TemplateScope,
) -> Result<Value, TemplateError> {
    let environment = strict_environment();
    let compiled = environment
        .compile_expression(expression_source)
        .map_err(failed_to_render)?;

    if let Some(field_path) =
        scope.first_unresolved_dependency(&compiled.undeclared_variables(true))
    {
        return Err(TemplateError::WaitingOn(field_path));
    }

    let context = scope.build_context(&compiled.undeclared_variables(false));
    let evaluated = compiled.eval(context).map_err(failed_to_render)?;

    // An expression that evaluates to an undefined value has not produced
    // anything; saying so is far more useful than writing a silent null.
    if evaluated.is_undefined() {
        return Err(TemplateError::Failed(format!(
            "'{}' did not resolve to a value",
            expression_source.trim()
        )));
    }

    convert_jinja_value_to_yaml(&evaluated)
}

/// Evaluates a condition and returns whether it holds. Conditions are
/// evaluated leniently: a field that is simply absent makes the condition
/// false instead of raising an error, so `$if` blocks can test fields that
/// may or may not be there.
///
/// The condition may be written as a bare expression (`type == 'database'`)
/// or wrapped in the usual delimiters (`{{ type == 'database' }}`).
pub fn evaluate_condition(
    condition_source: &str,
    scope: &TemplateScope,
) -> Result<bool, TemplateError> {
    let expression_source = whole_value_expression(condition_source).unwrap_or(condition_source);

    let environment = lenient_environment();
    let compiled = environment
        .compile_expression(expression_source)
        .map_err(failed_to_render)?;

    if let Some(field_path) =
        scope.first_unresolved_dependency(&compiled.undeclared_variables(true))
    {
        return Err(TemplateError::WaitingOn(field_path));
    }

    let context = scope.build_context(&compiled.undeclared_variables(false));

    match compiled.eval(context) {
        Ok(value) => Ok(value.is_true()),
        // A condition that cannot be evaluated (an absent field used in a way
        // even the lenient mode rejects) simply does not hold.
        Err(_) => Ok(false),
    }
}

fn convert_jinja_value_to_yaml(value: &JinjaValue) -> Result<Value, TemplateError> {
    serde_yaml::to_value(value)
        .map_err(|error| TemplateError::Failed(format!("could not convert result: {}", error)))
}

fn failed_to_render(error: JinjaError) -> TemplateError {
    TemplateError::Failed(match error.detail() {
        Some(detail) => format!("{}: {}", error.kind(), detail),
        None => error.kind().to_string(),
    })
}

// =====================================================================
// ENVIRONMENTS
// =====================================================================

static STRICT_ENVIRONMENT: OnceLock<Environment<'static>> = OnceLock::new();
static LENIENT_ENVIRONMENT: OnceLock<Environment<'static>> = OnceLock::new();

/// The environment used for values and schema references. Undefined names are
/// an error, so an unresolvable template is reported and left in place rather
/// than silently collapsing to an empty string.
fn strict_environment() -> &'static Environment<'static> {
    STRICT_ENVIRONMENT.get_or_init(|| build_environment(UndefinedBehavior::Strict))
}

/// The environment used for conditions, where an absent field should read as
/// "false" rather than raise.
fn lenient_environment() -> &'static Environment<'static> {
    LENIENT_ENVIRONMENT.get_or_init(|| build_environment(UndefinedBehavior::Chainable))
}

fn build_environment(undefined_behavior: UndefinedBehavior) -> Environment<'static> {
    let mut environment = Environment::new();
    environment.set_undefined_behavior(undefined_behavior);
    environment.set_unknown_method_callback(call_mapping_or_string_method);
    register_coal_filters(&mut environment);
    environment
}

// =====================================================================
// COAL FILTERS
// =====================================================================

fn register_coal_filters(environment: &mut Environment<'static>) {
    environment.add_filter("keys", filter_keys);
    environment.add_filter("values", filter_values);
    environment.add_filter("dict2items", filter_dict_to_items);
    environment.add_filter("items2dict", filter_items_to_dict);
    environment.add_filter("from_yaml", filter_from_yaml);
    environment.add_filter("to_yaml", filter_to_yaml);
}

/// The keys of a mapping, as a list.
fn filter_keys(value: JinjaValue) -> Result<JinjaValue, JinjaError> {
    Ok(JinjaValue::from(mapping_keys(&value)?))
}

/// The values of a mapping, as a list.
fn filter_values(value: JinjaValue) -> Result<JinjaValue, JinjaError> {
    let mut collected_values = Vec::new();
    for key in mapping_keys(&value)? {
        collected_values.push(value.get_item(&key)?);
    }
    Ok(JinjaValue::from(collected_values))
}

/// Turns a mapping into a list of `{key, value}` entries, the shape Ansible's
/// filter of the same name produces.
fn filter_dict_to_items(value: JinjaValue) -> Result<JinjaValue, JinjaError> {
    let mut entries = Vec::new();
    for key in mapping_keys(&value)? {
        let entry_value = value.get_item(&key)?;
        entries.push(JinjaValue::from(BTreeMap::from([
            ("key", key),
            ("value", entry_value),
        ])));
    }
    Ok(JinjaValue::from(entries))
}

/// The inverse of `dict2items`: a list of `{key, value}` entries becomes a
/// mapping.
fn filter_items_to_dict(value: JinjaValue) -> Result<JinjaValue, JinjaError> {
    let mut mapping = BTreeMap::new();

    for entry in value.try_iter()? {
        let key = entry.get_attr("key")?;
        let entry_value = entry.get_attr("value")?;
        mapping.insert(key.to_string(), entry_value);
    }
    Ok(JinjaValue::from(mapping))
}

/// Parses a string as YAML, so text assembled by a template can be turned
/// back into structure explicitly.
fn filter_from_yaml(value: JinjaValue) -> Result<JinjaValue, JinjaError> {
    let text = value.as_str().ok_or_else(|| {
        JinjaError::new(ErrorKind::InvalidOperation, "from_yaml expects a string")
    })?;

    let parsed_value: Value = serde_yaml::from_str(text)
        .map_err(|error| JinjaError::new(ErrorKind::InvalidOperation, error.to_string()))?;
    Ok(JinjaValue::from_serialize(&parsed_value))
}

/// Renders a value as YAML text.
fn filter_to_yaml(value: JinjaValue) -> Result<JinjaValue, JinjaError> {
    let yaml_value: Value = serde_yaml::to_value(&value)
        .map_err(|error| JinjaError::new(ErrorKind::InvalidOperation, error.to_string()))?;
    let rendered = serde_yaml::to_string(&yaml_value)
        .map_err(|error| JinjaError::new(ErrorKind::InvalidOperation, error.to_string()))?;
    Ok(JinjaValue::from(rendered))
}

fn mapping_keys(value: &JinjaValue) -> Result<Vec<JinjaValue>, JinjaError> {
    if value.kind() != ValueKind::Map {
        return Err(JinjaError::new(
            ErrorKind::InvalidOperation,
            format!("expected a mapping, got {}", value.kind()),
        ));
    }
    Ok(value.try_iter()?.collect())
}

// =====================================================================
// PYTHON-STYLE METHODS
// =====================================================================

/// Supplies the mapping and string methods people reach for out of habit —
/// `transports.keys()`, `name.upper()` — on top of the filters Jinja already
/// provides.
fn call_mapping_or_string_method(
    _state: &State,
    value: &JinjaValue,
    method_name: &str,
    arguments: &[JinjaValue],
) -> Result<JinjaValue, JinjaError> {
    match method_name {
        "keys" => filter_keys(value.clone()),
        "values" => filter_values(value.clone()),
        "items" => call_items_method(value),
        "get" => call_get_method(value, arguments),
        "upper" => map_string(value, |text| text.to_uppercase()),
        "lower" => map_string(value, |text| text.to_lowercase()),
        "strip" => map_string(value, |text| text.trim().to_string()),
        "split" => call_split_method(value, arguments),
        "startswith" => call_affix_method(value, arguments, |text, affix| text.starts_with(affix)),
        "endswith" => call_affix_method(value, arguments, |text, affix| text.ends_with(affix)),
        _ => Err(JinjaError::from(ErrorKind::UnknownMethod)),
    }
}

fn call_items_method(value: &JinjaValue) -> Result<JinjaValue, JinjaError> {
    let mut entries = Vec::new();
    for key in mapping_keys(value)? {
        let entry_value = value.get_item(&key)?;
        entries.push(JinjaValue::from(vec![key, entry_value]));
    }
    Ok(JinjaValue::from(entries))
}

fn call_get_method(value: &JinjaValue, arguments: &[JinjaValue]) -> Result<JinjaValue, JinjaError> {
    let key = arguments.first().ok_or_else(|| {
        JinjaError::new(ErrorKind::InvalidOperation, "get expects a key argument")
    })?;
    let fallback = arguments.get(1).cloned().unwrap_or(JinjaValue::from(()));

    match value.get_item(key) {
        Ok(found) if !found.is_undefined() => Ok(found),
        _ => Ok(fallback),
    }
}

fn call_split_method(
    value: &JinjaValue,
    arguments: &[JinjaValue],
) -> Result<JinjaValue, JinjaError> {
    let text = require_string(value, "split")?;
    let parts: Vec<JinjaValue> = match arguments.first().and_then(|argument| argument.as_str()) {
        Some(separator) => text.split(separator).map(JinjaValue::from).collect(),
        None => text.split_whitespace().map(JinjaValue::from).collect(),
    };
    Ok(JinjaValue::from(parts))
}

fn call_affix_method(
    value: &JinjaValue,
    arguments: &[JinjaValue],
    matches_affix: fn(&str, &str) -> bool,
) -> Result<JinjaValue, JinjaError> {
    let text = require_string(value, "startswith/endswith")?;
    let affix = arguments
        .first()
        .and_then(|argument| argument.as_str())
        .ok_or_else(|| {
            JinjaError::new(
                ErrorKind::InvalidOperation,
                "expected a string argument to compare against",
            )
        })?;
    Ok(JinjaValue::from(matches_affix(text, affix)))
}

fn map_string(value: &JinjaValue, transform: fn(&str) -> String) -> Result<JinjaValue, JinjaError> {
    let text = require_string(value, "this method")?;
    Ok(JinjaValue::from(transform(text)))
}

fn require_string<'a>(value: &'a JinjaValue, method_name: &str) -> Result<&'a str, JinjaError> {
    value.as_str().ok_or_else(|| {
        JinjaError::new(
            ErrorKind::InvalidOperation,
            format!("{} expects a string, got {}", method_name, value.kind()),
        )
    })
}
