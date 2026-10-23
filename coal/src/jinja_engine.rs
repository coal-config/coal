//! The Jinja engine COAL evaluates every template with.
//!
//! COAL embeds a complete Jinja2 engine (minijinja), so anything Jinja can
//! express — filters, tests, arithmetic, `{% for %}` and `{% if %}` blocks,
//! `{% set %}`, macros — is available in configuration values, in directive
//! arguments, and in schema definitions.
//!
//! # Where a name is read from
//!
//! A name in a template is looked up at the top level of the file the template
//! was written in, and at the configuration root if that file does not answer
//! for it. A file can therefore talk about its own fields without knowing
//! where in the tree it was mounted, and still reach everything else.
//!
//! "Does not answer for it" is decided by the whole path, not the first name:
//! a file that declares `paths` but no `paths.log_dir` is not the one
//! `{{ paths.log_dir }}` is asking about, so the lookup moves on to the root.
//! Write `root()` to skip the file and read the configuration root outright.
//!
//! # When a name is read
//!
//! A configuration with a `_structure.yaml` is assembled before it is read:
//! the selection is made, the layers are merged, and only then is there a
//! configuration for a reference to be about. So references resolve in two
//! moments. Before the merge, only a reference the file it is written in
//! answers for goes ahead — that file is already whole, and nothing the merge
//! does can change what it says about itself. Everything else — `root()`,
//! `parent()`, and a bare name the file does not answer for — waits until the
//! merge has happened and then reads the configuration that was actually
//! built. A configuration with no `_structure.yaml` is never rearranged, so
//! there is nothing to wait for and everything resolves at once.
//!
//! To reach something relative to where the template sits, say so:
//!
//! | Written | Reads |
//! | --- | --- |
//! | `{{ paths.log_dir }}` | `paths.log_dir` of this file, else of the root |
//! | `{{ root().paths }}` | the top-level `paths`, never the file's own |
//! | `{{ parent().port }}` | the `port` beside this field — `parent()` is the object the field lives in |
//! | `{{ parent(3) }}` | the object three levels up, as a mapping |
//! | `{{ key() }}` | this field's own key |
//! | `{{ parent(2).key() }}` | the key of the object two levels up |
//! | `{{ file() }}` | the file this field was written in, without its extension |
//! | `{{ path() }}` | that file's path |
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

use crate::coal_tree::{CoalTree, tree_to_template_value};
use crate::type_aliases::TreeNodeReference;
use minijinja::value::{Enumerator, Object, ObjectRepr, Value as JinjaValue, ValueKind};
use minijinja::{
    Environment, Error as JinjaError, ErrorKind, Output, State, UndefinedBehavior, escape_formatter,
};
use regex::Regex;
use serde_yaml::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

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
    /// The template reads something the configuration does not have.
    pub fn missing(field_path: &str) -> Self {
        TemplateError::Failed(format!("it reads '{}', which does not exist", field_path))
    }

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

/// The template a field is still waiting to have rendered, if it has one.
///
/// Only a childless field whose own value is text can be waiting: a node with
/// children holds a mapping or a sequence that is rebuilt from those children,
/// and every string inside one is a child of its own.
///
/// A childless node whose value is a mapping or a sequence is therefore not a
/// field with a template but a container that ended up empty — an `$override`
/// whose `$include_optional` named a file that is not there, say. What is left
/// in its value is the directive that was consumed, and the reference written
/// inside that directive is nobody's to wait for and nothing to report.
pub fn unrendered_template_of_node(node: &CoalTree) -> Option<&str> {
    if node.template_resolved || !node.children.is_empty() {
        return None;
    }

    match node.value {
        Some(Value::String(ref text)) if contains_template(text) => Some(text),
        _ => None,
    }
}

/// The first field in the subtree that is still an unresolved template, if
/// there is one. A reference to a whole object is waiting for whichever field
/// inside it has not resolved, and naming that field rather than the object is
/// what lets one warning be traced back to the next.
///
/// Three things do not count. A field the engine has already finished with is
/// resolved even if its value looks like a template, which is what a
/// `{% raw %}` block produces. System directives never reach the compiled
/// output, so a template inside one cannot be what a field is waiting for. And
/// `reading_node` — the field asking the question — is skipped, because a
/// field that reads the object it lives in would otherwise be waiting for
/// itself.
pub fn first_unresolved_field_in(
    node: &TreeNodeReference,
    reading_node: &TreeNodeReference,
) -> Option<TreeNodeReference> {
    if Rc::ptr_eq(node, reading_node) {
        return None;
    }

    let node_borrowed = node.borrow();

    if node_borrowed.children.is_empty() {
        return unrendered_template_of_node(&node_borrowed).map(|_| Rc::clone(node));
    }

    node_borrowed.children.iter().find_map(|child| {
        let is_directive = child
            .borrow()
            .key
            .as_deref()
            .is_some_and(|key| key.starts_with('$'));
        if is_directive {
            None
        } else {
            first_unresolved_field_in(child, reading_node)
        }
    })
}

// =====================================================================
// WHAT A TEMPLATE READS
// =====================================================================

/// What the engine learned by parsing a template once: which fields it reads,
/// which of its own ancestors it asks for, and what it wants from them.
///
/// Parsing is the expensive part of rendering a short template, and a schema
/// template is rendered once per node it validates — thousands of times for
/// the same handful of characters. These facts are therefore worked out once
/// per distinct template source and kept.
#[derive(Clone, Default)]
struct TemplateFacts {
    /// Dotted paths into the configuration, as written (`paths.log_dir`).
    referenced_paths: Vec<String>,
    /// The top-level names to bind, without their trailing path.
    referenced_names: Vec<String>,
    /// What the template asks of its own ancestors. `None` means it works the
    /// level out while rendering, so every ancestor has to be offered.
    ancestor_reads: Option<Vec<AncestorRead>>,
    /// What the template asks of the configuration root through `root()`.
    /// Empty when it never mentions it.
    root_reads: Vec<Option<Vec<String>>>,
    uses_key: bool,
    uses_file: bool,
    uses_path: bool,
    /// Whether the template emits Jinja delimiters as literal text, which
    /// `{% raw %}` is the way to do.
    emits_literal_delimiters: bool,
    /// Whether the template uses statement blocks, which is what makes a
    /// rendered result eligible to be read back as YAML.
    has_statement_blocks: bool,
    /// Whether the template says outright what a read that is not there should
    /// come to — `| default(...)`, `is defined`, `.get(key, fallback)`. Such a
    /// template is rendered where an absent value may be reached through, so
    /// the guard gets its turn.
    guards_absent_values: bool,
}

/// One `parent(...)` in a template, and how much of that ancestor it reads.
///
/// Reading one field of the enclosing object must not mean waiting for the
/// whole object — the field doing the reading is part of it, and so are its
/// siblings, which would leave every `{{ parent().x }}` waiting for every
/// other one.
#[derive(Clone, Debug)]
struct AncestorRead {
    /// How many levels up, as `parent(n)` was written.
    level: usize,
    /// The field read from that ancestor. `None` when the ancestor is used
    /// whole — passed to a filter, iterated, printed — and every part of it
    /// therefore matters. `Some` with nothing in it means only the ancestor's
    /// key or origin was asked for, which no value can change.
    field_path: Option<Vec<String>>,
}

/// When in the build a reference is being resolved.
///
/// A `_structure.yaml` selects one part of the configuration and merges layers
/// onto it, which changes what the root holds and where a field's ancestors
/// are. A reference resolved before that would freeze an answer the merge is
/// about to replace, so most of them wait for it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ResolutionStage {
    /// The structure has not merged yet. Only what a file says about itself
    /// may be read.
    BeforeStructureMerge,
    /// The configuration is the one being built, and everything may be read.
    AfterStructureMerge,
}

/// The names the engine binds itself, which are never configuration fields.
const RESERVED_NAMES: [&str; 5] = ["parent", "root", "key", "file", "path"];

/// The things a node can be asked about itself rather than about its value.
const STRUCTURAL_METHODS: [&str; 3] = ["key", "file", "path"];

static TEMPLATE_FACTS_CACHE: OnceLock<Mutex<HashMap<String, Arc<TemplateFacts>>>> = OnceLock::new();

// =====================================================================
// FROZEN TREE
// =====================================================================

/// What templates have already read from a tree that can no longer change.
///
/// Schema validation reads the configuration a great many times — every
/// allowed-values template, every schema `$template`, once per field it
/// applies to — while never changing a single value in it. Converting a field
/// into something Jinja can read means walking and copying it, and doing that
/// for `messages` or `root()` once per list element dominated validation time.
/// While the tree is frozen, each node is converted once, and a template asked
/// the same question in the same scope is answered from the first rendering.
#[derive(Default)]
struct FrozenTreeCache {
    /// Node → its value as a template reads it. The node is held so its
    /// address cannot be reused by another node while the cache is alive.
    template_values: HashMap<usize, (TreeNodeReference, JinjaValue)>,
    /// Node → its value wrapped as a [`FieldValue`], as `parent()` and
    /// `root()` hand it out.
    field_values: HashMap<usize, (TreeNodeReference, JinjaValue)>,
    /// A rendering, keyed by the template and everything in its scope that
    /// could change the answer.
    renders: HashMap<RenderKey, Result<Value, TemplateError>>,
    /// Whether no field anywhere in the tree is still an unrendered template.
    /// Then no template has anything to wait for, and the field a template is
    /// written on no longer changes its answer.
    fully_resolved: bool,
}

#[derive(PartialEq, Eq, Hash)]
struct RenderKey {
    kind: RenderKind,
    template: String,
    /// Addresses of the nodes the rendering could read: the configuration
    /// root, the file root, the ancestors it reaches and, when it asks for
    /// `key()`/`file()`/`path()`, the field it is written on.
    nodes: Vec<usize>,
    relaxed: bool,
    stage: ResolutionStage,
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum RenderKind {
    Value,
    Yaml,
}

thread_local! {
    static FROZEN_TREE: RefCell<Option<FrozenTreeCache>> = const { RefCell::new(None) };
}

/// Declares the configuration tree read-only until the guard is dropped.
///
/// Only take this while nothing writes to the tree's keys, values or
/// children — schema validation, which only records its findings on the
/// nodes, is the case it was made for.
#[must_use = "the tree only counts as frozen while the guard is alive"]
pub struct FrozenTreeGuard {
    previous: Option<FrozenTreeCache>,
}

pub fn freeze_tree_for_reading(root_node: &TreeNodeReference) -> FrozenTreeGuard {
    let frozen = FrozenTreeCache {
        fully_resolved: !holds_unrendered_template(root_node),
        ..FrozenTreeCache::default()
    };
    let previous = FROZEN_TREE.with(|cache| cache.borrow_mut().replace(frozen));
    FrozenTreeGuard { previous }
}

/// Whether any field below `node` is still an unrendered template, looking
/// exactly where [`first_unresolved_field_in`] would.
fn holds_unrendered_template(node: &TreeNodeReference) -> bool {
    let node_borrowed = node.borrow();
    if node_borrowed.children.is_empty() {
        return unrendered_template_of_node(&node_borrowed).is_some();
    }
    node_borrowed.children.iter().any(|child| {
        let is_directive = child
            .borrow()
            .key
            .as_deref()
            .is_some_and(|key| key.starts_with('$'));
        !is_directive && holds_unrendered_template(child)
    })
}

fn frozen_tree_is_fully_resolved() -> bool {
    FROZEN_TREE.with(|cache| cache.borrow().as_ref().is_some_and(|cache| cache.fully_resolved))
}

impl Drop for FrozenTreeGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        FROZEN_TREE.with(|cache| *cache.borrow_mut() = previous);
    }
}

fn tree_is_frozen() -> bool {
    FROZEN_TREE.with(|cache| cache.borrow().is_some())
}

/// Looks a node up in one of the frozen tree's conversion tables, converting
/// it on first use. Outside a frozen tree it simply converts.
fn frozen_conversion(
    node: &TreeNodeReference,
    table: fn(&mut FrozenTreeCache) -> &mut HashMap<usize, (TreeNodeReference, JinjaValue)>,
    convert: impl FnOnce() -> JinjaValue,
) -> JinjaValue {
    let address = Rc::as_ptr(node) as usize;
    let cached = FROZEN_TREE.with(|cache| {
        cache
            .borrow_mut()
            .as_mut()
            .and_then(|cache| table(cache).get(&address).map(|(_, value)| value.clone()))
    });
    if let Some(value) = cached {
        return value;
    }

    let value = convert();
    FROZEN_TREE.with(|cache| {
        if let Some(cache) = cache.borrow_mut().as_mut() {
            table(cache).insert(address, (Rc::clone(node), value.clone()));
        }
    });
    value
}

/// A node's value as a template reads it.
fn template_value_of_node(node: &TreeNodeReference) -> JinjaValue {
    frozen_conversion(
        node,
        |cache| &mut cache.template_values,
        || JinjaValue::from_serialize(tree_to_template_value(Rc::clone(node))),
    )
}

/// Renders through the frozen tree's memo when the tree is frozen and the
/// scope can be keyed; otherwise renders directly.
fn memoized_render(
    kind: RenderKind,
    template: &str,
    scope: &TemplateScope,
    render: impl FnOnce() -> Result<Value, TemplateError>,
) -> Result<Value, TemplateError> {
    if !tree_is_frozen() {
        return render();
    }
    let Some(key) = scope.render_key(kind, template) else {
        return render();
    };

    let cached = FROZEN_TREE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .and_then(|cache| cache.renders.get(&key).cloned())
    });
    if let Some(result) = cached {
        return result;
    }

    let result = render();
    FROZEN_TREE.with(|cache| {
        if let Some(cache) = cache.borrow_mut().as_mut() {
            cache.renders.insert(key, result.clone());
        }
    });
    result
}
static PARENT_CALL_REGEX: OnceLock<Regex> = OnceLock::new();
static ROOT_CALL_REGEX: OnceLock<Regex> = OnceLock::new();
static GUARD_REGEX: OnceLock<Regex> = OnceLock::new();

/// Matches `parent(...)`, the dotted chain that follows it, and whether that
/// chain ends in a call.
fn parent_call_regex() -> &'static Regex {
    PARENT_CALL_REGEX.get_or_init(|| {
        Regex::new(r"\bparent\s*\(\s*([^)]*?)\s*\)((?:\s*\.\s*[A-Za-z_][A-Za-z0-9_]*)*)\s*(\()?")
            .unwrap()
    })
}

/// Matches `root()`, the dotted chain that follows it, and whether that chain
/// ends in a call. The leading group keeps `something.root()` — a method on a
/// value that happens to be called `root` — from reading as the engine's own.
fn root_call_regex() -> &'static Regex {
    ROOT_CALL_REGEX.get_or_init(|| {
        Regex::new(
            r"(?:^|[^.\w])root\s*\(\s*\)((?:\s*\.\s*[A-Za-z_][A-Za-z0-9_]*)*)\s*(\()?",
        )
        .unwrap()
    })
}

fn template_facts(source: &str) -> Result<Arc<TemplateFacts>, TemplateError> {
    let cache = TEMPLATE_FACTS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    if let Ok(cache) = cache.lock()
        && let Some(facts) = cache.get(source)
    {
        return Ok(Arc::clone(facts));
    }

    let facts = Arc::new(analyze_template(source)?);

    if let Ok(mut cache) = cache.lock() {
        cache.insert(source.to_string(), Arc::clone(&facts));
    }
    Ok(facts)
}

fn analyze_template(source: &str) -> Result<TemplateFacts, TemplateError> {
    let environment = strict_environment();
    let compiled = environment
        .template_from_str(source)
        .map_err(failed_to_render)?;

    let referenced_paths: Vec<String> = strip_called_method_segments(
        source,
        compiled
            .undeclared_variables(true)
            .into_iter()
            .filter(|path| !is_reserved_path(path))
            .collect(),
    );

    let referenced_names: Vec<String> = compiled
        .undeclared_variables(false)
        .into_iter()
        .filter(|name| !RESERVED_NAMES.contains(&name.as_str()))
        .collect();

    Ok(TemplateFacts {
        referenced_paths,
        referenced_names,
        ancestor_reads: collect_ancestor_reads(source),
        root_reads: collect_root_reads(source),
        uses_key: source.contains("key("),
        uses_file: source.contains("file("),
        uses_path: source.contains("path("),
        emits_literal_delimiters: source.contains("{% raw %}") || source.contains("{%- raw %}"),
        has_statement_blocks: source.contains("{%"),
        guards_absent_values: guards_absent_values(source),
    })
}

/// Drops the last segment of a path the template *calls*.
///
/// Jinja hands back the names a template reads as dotted paths, and cannot tell
/// `services.port` from `services.keys()`: both look the same. Only the first
/// is a field. Left alone, the second makes the build wait for — and then
/// complain about — a field named after the method, which the configuration was
/// never meant to have, while the container the method is really being called
/// on is not waited for at all.
fn strip_called_method_segments(source: &str, paths: Vec<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();

    for path in paths {
        let read_path = match path.rsplit_once('.') {
            Some((container_path, _)) if !container_path.is_empty() && is_called(source, &path) => {
                container_path.to_string()
            }
            _ => path,
        };
        if !kept.contains(&read_path) {
            kept.push(read_path);
        }
    }
    kept
}

/// Whether the template writes this path with a call after it.
fn is_called(source: &str, path: &str) -> bool {
    let dotted_pattern = path
        .split('.')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(r"\s*\.\s*");

    Regex::new(&format!(r"(?:^|[^.\w]){}\s*\(", dotted_pattern))
        .map(|pattern| pattern.is_match(source))
        .unwrap_or(false)
}

/// Whether a template says what an absent read should come to, rather than
/// leaving it to be an error. Only the forms Jinja itself offers count:
/// `| default(...)` and its `| d(...)` alias, the `defined` and `undefined`
/// tests, and `.get(key, fallback)`.
fn guards_absent_values(source: &str) -> bool {
    GUARD_REGEX
        .get_or_init(|| {
            Regex::new(r"\|\s*(?:default|d)\s*\(|\bis\s+(?:not\s+)?(?:defined|undefined)\b|\.\s*get\s*\(")
                .unwrap()
        })
        .is_match(source)
}

fn is_reserved_path(path: &str) -> bool {
    let first_segment = path.split('.').next().unwrap_or(path);
    RESERVED_NAMES.contains(&first_segment)
}

/// Reads what the template's `parent(...)` calls ask for, so only those
/// ancestors have to be materialized and only the fields actually read have to
/// be waited for. A call whose level is worked out while rendering gives up
/// and returns `None`.
fn collect_ancestor_reads(source: &str) -> Option<Vec<AncestorRead>> {
    if !source.contains("parent") {
        return Some(Vec::new());
    }

    let mut reads = Vec::new();
    for captures in parent_call_regex().captures_iter(source) {
        let argument = captures[1].trim();
        let level = if argument.is_empty() {
            1
        } else {
            argument.parse::<usize>().ok()?
        };

        let mut segments: Vec<String> = captures[2]
            .split('.')
            .map(|segment| segment.trim().to_string())
            .filter(|segment| !segment.is_empty())
            .collect();
        let chain_ends_in_call = captures.get(3).is_some();

        let field_path = if segments.is_empty() {
            // parent(n) on its own: filtered, iterated or printed whole.
            None
        } else if chain_ends_in_call {
            // The last name is a method. Asking a node for its key or its file
            // reads no value at all; anything else reads whatever it is called
            // on.
            let method_name = segments.pop().unwrap_or_default();
            if segments.is_empty() && STRUCTURAL_METHODS.contains(&method_name.as_str()) {
                Some(Vec::new())
            } else if segments.is_empty() {
                None
            } else {
                Some(segments)
            }
        } else {
            Some(segments)
        };

        reads.push(AncestorRead { level, field_path });
    }
    Some(reads)
}

/// Reads what the template's `root()` calls ask for, so only the fields
/// actually read have to be waited for rather than the whole configuration.
fn collect_root_reads(source: &str) -> Vec<Option<Vec<String>>> {
    if !source.contains("root") {
        return Vec::new();
    }

    let mut reads = Vec::new();
    for captures in root_call_regex().captures_iter(source) {
        let mut segments: Vec<String> = captures[1]
            .split('.')
            .map(|segment| segment.trim().to_string())
            .filter(|segment| !segment.is_empty())
            .collect();
        let chain_ends_in_call = captures.get(2).is_some();

        let field_path = if segments.is_empty() {
            // root() on its own: filtered, iterated or printed whole.
            None
        } else if chain_ends_in_call {
            let method_name = segments.pop().unwrap_or_default();
            if segments.is_empty() && STRUCTURAL_METHODS.contains(&method_name.as_str()) {
                Some(Vec::new())
            } else if segments.is_empty() {
                None
            } else {
                Some(segments)
            }
        } else {
            Some(segments)
        };

        reads.push(field_path);
    }
    reads
}

/// Whether the template asks to emit Jinja delimiters as literal text.
pub fn emits_literal_delimiters(template: &str) -> bool {
    template_facts(template).is_ok_and(|facts| facts.emits_literal_delimiters)
}

// =====================================================================
// A FIELD, AS A TEMPLATE SEES IT
// =====================================================================

/// A node handed to a template: its value, plus the three things about the
/// node itself a template may ask for.
///
/// It behaves exactly like the value it wraps — indexing, iteration, `length`,
/// printing — and additionally answers `key()`, `file()` and `path()`.
#[derive(Debug)]
struct FieldValue {
    value: JinjaValue,
    key: Option<String>,
    file_stem: Option<String>,
    origin_path: Option<String>,
}

impl FieldValue {
    /// Wraps a node so a template can ask about the node as well as read it.
    ///
    /// Only objects and lists are wrapped. A field holding a plain scalar is
    /// handed over as that scalar, because a wrapper would no longer compare
    /// equal to it — `{{ parent().mode == 'fast' }}` has to mean what it says.
    fn of_node(node: &TreeNodeReference) -> JinjaValue {
        frozen_conversion(node, |cache| &mut cache.field_values, || Self::wrap(node))
    }

    fn wrap(node: &TreeNodeReference) -> JinjaValue {
        let value = template_value_of_node(node);
        if !matches!(value.kind(), ValueKind::Map | ValueKind::Seq) {
            return value;
        }

        let (key, origin_path) = {
            let node_borrowed = node.borrow();
            (node_borrowed.key.clone(), node_borrowed.origin.clone())
        };

        JinjaValue::from_object(FieldValue {
            value,
            key,
            file_stem: origin_path.as_deref().and_then(file_stem_of),
            origin_path,
        })
    }
}

/// A `_structure.yaml` selector's match, as a layer reads it.
///
/// It answers like the node it matched — `{{ node.node_type }}` — and prints
/// as the key that named it, so `{{ node }}` is `cop01`. A layer can therefore
/// be written either way round without knowing which it is going to get.
#[derive(Debug)]
struct SelectionMatch {
    fields: JinjaValue,
    key: String,
}

impl Object for SelectionMatch {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &JinjaValue) -> Option<JinjaValue> {
        self.fields
            .get_item(key)
            .ok()
            .filter(|found| !found.is_undefined())
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        match self.fields.try_iter() {
            Ok(entries) => Enumerator::Values(entries.collect()),
            Err(_) => Enumerator::NonEnumerable,
        }
    }

    fn render(self: &Arc<Self>, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.key)
    }
}

impl SelectionMatch {
    fn of_node(node: &TreeNodeReference) -> JinjaValue {
        let key = node.borrow().key.clone().unwrap_or_default();
        JinjaValue::from_object(SelectionMatch {
            fields: JinjaValue::from_serialize(tree_to_template_value(Rc::clone(node))),
            key,
        })
    }
}

fn file_stem_of(origin_path: &str) -> Option<String> {
    Path::new(origin_path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
}

impl Object for FieldValue {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        match self.value.kind() {
            ValueKind::Seq => ObjectRepr::Seq,
            ValueKind::Map => ObjectRepr::Map,
            _ => ObjectRepr::Plain,
        }
    }

    fn get_value(self: &Arc<Self>, key: &JinjaValue) -> Option<JinjaValue> {
        self.value
            .get_item(key)
            .ok()
            .filter(|found| !found.is_undefined())
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        match self.value.try_iter() {
            Ok(entries) => Enumerator::Values(entries.collect()),
            Err(_) => Enumerator::NonEnumerable,
        }
    }

    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        self.value.len()
    }

    fn is_true(self: &Arc<Self>) -> bool {
        self.value.is_true()
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &State,
        method_name: &str,
        _arguments: &[JinjaValue],
    ) -> Result<JinjaValue, JinjaError> {
        match method_name {
            "key" => Ok(optional_text(&self.key)),
            "file" => Ok(optional_text(&self.file_stem)),
            "path" => Ok(optional_text(&self.origin_path)),
            _ => Err(JinjaError::from(ErrorKind::UnknownMethod)),
        }
    }

    fn render(self: &Arc<Self>, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.value)
    }
}

fn optional_text(text: &Option<String>) -> JinjaValue {
    match text {
        Some(text) => JinjaValue::from(text.clone()),
        None => JinjaValue::UNDEFINED,
    }
}

/// Unwraps a node handed back out of a template, so what reaches the tree is
/// the plain value rather than the wrapper.
fn unwrap_field_value(value: &JinjaValue) -> JinjaValue {
    match value.downcast_object_ref::<FieldValue>() {
        Some(field) => field.value.clone(),
        None => value.clone(),
    }
}

// =====================================================================
// SCOPE
// =====================================================================

/// Where a template is written, which is all the engine needs to resolve it:
/// the configuration root that names are read from, and the chain of nodes
/// `parent(...)`, `key()` and `file()` speak about.
pub struct TemplateScope {
    root_node: TreeNodeReference,
    /// The nearest enclosing node a file was loaded onto, which is where a
    /// bare name is looked up before the configuration root.
    file_root: TreeNodeReference,
    /// The field the template is written on, followed by every enclosing
    /// object. `parent(n)` is `ancestors[n]`.
    ancestors: Vec<TreeNodeReference>,
    /// Whether reading a whole enclosing object may go ahead even though
    /// something else inside it is unresolved. See [`TemplateScope::relaxed`].
    relaxed: bool,
    /// Names supplied by whatever is rendering the template rather than read
    /// from the configuration — the captures of a `_structure.yaml` pattern.
    /// They shadow configuration fields of the same name and are always
    /// resolved, so nothing ever waits for one.
    bindings: BTreeMap<String, Value>,
    /// Nodes supplied by whatever is rendering the template — the matches a
    /// `_structure.yaml` selector made. They answer like the node and print
    /// like its key, and they shadow configuration fields of the same name.
    node_bindings: BTreeMap<String, TreeNodeReference>,
    /// Where in the build this rendering happens. Before the structure has
    /// merged, a template that reaches outside its own file is held back.
    stage: ResolutionStage,
}

impl TemplateScope {
    /// The scope of a template written on `node`.
    pub fn for_node(node: &TreeNodeReference, root_node: &TreeNodeReference) -> Self {
        Self {
            root_node: Rc::clone(root_node),
            file_root: nearest_file_root(node, root_node),
            ancestors: ancestor_chain(node, root_node),
            relaxed: false,
            bindings: BTreeMap::new(),
            node_bindings: BTreeMap::new(),
            stage: ResolutionStage::AfterStructureMerge,
        }
    }

    /// Says when this rendering is happening. At
    /// [`ResolutionStage::BeforeStructureMerge`] a template that reads
    /// anything other than its own file is refused rather than answered from a
    /// tree the structure has yet to rearrange.
    pub fn at_stage(mut self, stage: ResolutionStage) -> Self {
        self.stage = stage;
        self
    }

    /// Lets a field read a whole enclosing object even while something else
    /// inside that object is unresolved.
    ///
    /// Two fields that each read their own container — `{{ parent() | length }}`
    /// beside `{{ parent().keys() }}` — each wait for the other and neither
    /// goes first. Once ordinary resolution has run out of things it can do,
    /// they are allowed to look at the container as it stands, which is what
    /// they were asking about. Reading a *named* field still waits, relaxed or
    /// not, so a reference is never resolved against a half-finished value.
    pub fn relaxed(mut self) -> Self {
        self.relaxed = true;
        self
    }

    /// Supplies names the configuration does not hold. Used for the captures
    /// of a `_structure.yaml` pattern, so a layer can be written in terms of
    /// the environment or the node type the selection settled on.
    pub fn with_bindings(mut self, bindings: BTreeMap<String, Value>) -> Self {
        self.bindings = bindings;
        self
    }

    /// Supplies whole nodes under names of their own — what a `_structure.yaml`
    /// selector matched. Unlike a plain binding these are still part of the
    /// tree, so a template reading one waits for its fields exactly as it would
    /// for any other field.
    pub fn with_node_bindings(mut self, node_bindings: BTreeMap<String, TreeNodeReference>) -> Self {
        self.node_bindings = node_bindings;
        self
    }

    /// The scope of a template that describes an object rather than one of its
    /// fields — a schema's `$template`. It reads the object exactly as the
    /// field definitions beside it do, so `parent()` is the object itself.
    pub fn for_object_block(node: &TreeNodeReference, root_node: &TreeNodeReference) -> Self {
        let mut ancestors = ancestor_chain(node, root_node);
        ancestors.insert(0, Rc::clone(node));

        Self {
            root_node: Rc::clone(root_node),
            file_root: nearest_file_root(node, root_node),
            ancestors,
            relaxed: false,
            bindings: BTreeMap::new(),
            node_bindings: BTreeMap::new(),
            stage: ResolutionStage::AfterStructureMerge,
        }
    }

    fn reading_node(&self) -> &TreeNodeReference {
        self.ancestors.first().unwrap_or(&self.root_node)
    }

    fn ancestor(&self, levels_up: usize) -> Option<&TreeNodeReference> {
        self.ancestors.get(levels_up)
    }

    /// The field a bare name binds to.
    ///
    /// The root of the file the template was written in is asked first, and
    /// the configuration root only if that file does not answer for the name.
    /// Answering means every path the template reads through the name
    /// resolves there: a file that declares `paths` but no `paths.log_dir` is
    /// not what `{{ paths.log_dir }}` is about, so the lookup moves on.
    fn field_for_name(&self, name: &str, facts: &TemplateFacts) -> Option<TreeNodeReference> {
        if let Some(bound_node) = self.node_bindings.get(name) {
            return Some(Rc::clone(bound_node));
        }
        match self.file_field_for_name(name, facts) {
            Some(file_field) => Some(file_field),
            None => find_child(&self.root_node, name),
        }
    }

    /// The field a bare name binds to *within its own file*, or `None` when
    /// that file does not answer for the name.
    ///
    /// The configuration root is never a file for this purpose: a template
    /// written at the top level reads the very object a structure merge
    /// rewrites, so it has nothing of its own to be resolved against.
    fn file_field_for_name(
        &self,
        name: &str,
        facts: &TemplateFacts,
    ) -> Option<TreeNodeReference> {
        if Rc::ptr_eq(&self.file_root, &self.root_node) {
            return None;
        }
        let file_field = find_child(&self.file_root, name)?;

        self.paths_read_through(name, facts)
            .iter()
            .all(|segments| walk_path_strictly(&file_field, segments).is_some())
            .then_some(file_field)
    }

    /// Why this template may not be rendered yet, when what it reads is not
    /// settled until the structure has merged.
    ///
    /// `root()` says outright which root it means, and `parent()` walks a
    /// chain the merge rearranges, so neither may be answered early. A bare
    /// name may, but only when the file the template is written in answers for
    /// it: that file is already whole, and no layer merged onto the
    /// configuration changes what it says about itself.
    fn deferred_until_structure_merge(&self, facts: &TemplateFacts) -> Option<TemplateError> {
        if self.stage == ResolutionStage::AfterStructureMerge {
            return None;
        }

        let reads_beyond_its_file = !facts.root_reads.is_empty()
            || !matches!(&facts.ancestor_reads, Some(reads) if reads.is_empty())
            || !facts.referenced_names.iter().all(|name| {
                self.name_is_supplied(name) || self.file_field_for_name(name, facts).is_some()
            });

        reads_beyond_its_file.then(|| {
            TemplateError::WaitingOn(
                "the configuration _structure.yaml has yet to assemble".to_string(),
            )
        })
    }

    /// The paths this template reads through `name`, each relative to the
    /// field that name binds to.
    fn paths_read_through(&self, name: &str, facts: &TemplateFacts) -> Vec<Vec<String>> {
        facts
            .referenced_paths
            .iter()
            .filter_map(|referenced_path| {
                let mut segments = referenced_path.split('.');
                if segments.next()? != name {
                    return None;
                }
                Some(segments.map(|segment| segment.to_string()).collect())
            })
            .collect()
    }

    /// Everything about this scope that could change how `template` renders,
    /// as a key for the frozen tree's memo. `None` when the scope carries
    /// values from outside the tree, which the key cannot describe.
    fn render_key(&self, kind: RenderKind, template: &str) -> Option<RenderKey> {
        if !self.bindings.is_empty() || !self.node_bindings.is_empty() {
            return None;
        }
        let facts = match whole_value_expression(template) {
            Some(expression_source) if kind == RenderKind::Value => {
                template_facts(&format!("{{{{ {} }}}}", expression_source)).ok()?
            }
            _ => template_facts(template).ok()?,
        };

        let address = |node: &TreeNodeReference| Rc::as_ptr(node) as usize;
        let mut nodes = vec![address(&self.root_node), address(&self.file_root)];
        let levels = self.ancestor_levels(&facts);
        nodes.push(levels.len());
        for level in levels {
            nodes.push(level);
            nodes.push(self.ancestor(level).map_or(0, address));
        }
        let reads_its_own_field = facts.uses_key || facts.uses_file || facts.uses_path;
        if reads_its_own_field || !frozen_tree_is_fully_resolved() {
            nodes.push(address(self.reading_node()));
        }

        Some(RenderKey {
            kind,
            template: template.to_string(),
            nodes,
            relaxed: self.relaxed,
            stage: self.stage,
        })
    }

    /// Materializes only what the template asks for: the top-level fields it
    /// names, and the ancestors its `parent(...)` calls reach.
    fn build_context(&self, facts: &TemplateFacts) -> BTreeMap<String, JinjaValue> {
        let mut context = BTreeMap::new();

        for name in &facts.referenced_names {
            if let Some(bound_value) = self.bindings.get(name) {
                context.insert(name.clone(), JinjaValue::from_serialize(bound_value));
                continue;
            }
            if let Some(bound_node) = self.node_bindings.get(name) {
                context.insert(name.clone(), SelectionMatch::of_node(bound_node));
                continue;
            }
            if let Some(node) = self.field_for_name(name, facts) {
                context.insert(name.clone(), template_value_of_node(&node));
            }
        }

        let levels = self.ancestor_levels(facts);
        if !levels.is_empty() {
            context.insert("parent".to_string(), self.ancestor_function(&levels));
        }

        if !facts.root_reads.is_empty() {
            let configuration_root = FieldValue::of_node(&self.root_node);
            context.insert(
                "root".to_string(),
                JinjaValue::from_function(move || configuration_root.clone()),
            );
        }

        if facts.uses_key || facts.uses_file || facts.uses_path {
            let reading_node = self.reading_node().borrow();
            let key = reading_node.key.clone();
            let origin_path = reading_node.origin.clone();
            let file_stem = origin_path.as_deref().and_then(file_stem_of);

            context.insert(
                "key".to_string(),
                JinjaValue::from_function(move || optional_text(&key)),
            );
            context.insert(
                "file".to_string(),
                JinjaValue::from_function(move || optional_text(&file_stem)),
            );
            context.insert(
                "path".to_string(),
                JinjaValue::from_function(move || optional_text(&origin_path)),
            );
        }

        context
    }

    /// Which ancestors have to be materialized for this template.
    fn ancestor_levels(&self, facts: &TemplateFacts) -> Vec<usize> {
        match &facts.ancestor_reads {
            Some(reads) => reads.iter().map(|read| read.level).collect(),
            None => (0..self.ancestors.len()).collect(),
        }
    }

    /// Builds `parent(...)` as a function over the ancestors it may be asked
    /// for. The ancestors are materialized up front — there are only ever a
    /// handful, and the function itself has to stand on its own once the tree
    /// is out of reach.
    fn ancestor_function(&self, levels: &[usize]) -> JinjaValue {
        let mut materialized: BTreeMap<usize, JinjaValue> = BTreeMap::new();
        for &level in levels {
            if let Some(node) = self.ancestor(level) {
                materialized
                    .entry(level)
                    .or_insert_with(|| FieldValue::of_node(node));
            }
        }

        JinjaValue::from_function(move |levels_up: Option<usize>| {
            let level = levels_up.unwrap_or(1);
            match materialized.get(&level) {
                Some(ancestor) => Ok(ancestor.clone()),
                None => Err(JinjaError::new(
                    ErrorKind::InvalidOperation,
                    format!("there is no object {} levels above this field", level),
                )),
            }
        })
    }

    /// Reports the first field a template reads that is itself still a
    /// template.
    fn first_unresolved_dependency(&self, facts: &TemplateFacts) -> Option<String> {
        let reading_node = self.reading_node();

        for referenced_path in &facts.referenced_paths {
            if self.is_bound(referenced_path) {
                continue;
            }
            let Some(node) = self.find_dependency_node(referenced_path, facts) else {
                continue;
            };
            // Landing back on an object the template is written inside means
            // the read is about the container rather than about one field of
            // it. A path that named a field and did not reach one is not
            // something to wait for at all. A bare name *is* the container, and
            // is the same shape as `{{ parent() | length }}`: it waits for the
            // rest of its container as usual, and only the relaxed round lets
            // it look at the container as it stands.
            if encloses(&node, reading_node) && (referenced_path.contains('.') || self.relaxed) {
                continue;
            }
            if let Some(unresolved) = first_unresolved_field_in(&node, reading_node) {
                return Some(describe_field(&unresolved));
            }
        }

        for read in &facts.root_reads {
            let read_node = match read {
                // Only the root's key or origin was asked for.
                Some(segments) if segments.is_empty() => continue,
                Some(segments) => match self.field_read_through(&self.root_node, segments, reading_node) {
                    Some(node) => node,
                    None => continue,
                },
                None if self.relaxed => continue,
                None => Rc::clone(&self.root_node),
            };

            if let Some(unresolved) = first_unresolved_field_in(&read_node, reading_node) {
                return Some(describe_field(&unresolved));
            }
        }

        for read in self.ancestor_reads(facts) {
            let Some(ancestor) = self.ancestor(read.level) else {
                continue;
            };

            let read_node = match &read.field_path {
                // Only the ancestor's key or origin was asked for, and no
                // value can change those.
                Some(segments) if segments.is_empty() => continue,
                Some(segments) => match self.field_read_through(ancestor, segments, reading_node) {
                    Some(node) => node,
                    None => continue,
                },
                None if self.relaxed => continue,
                None => Rc::clone(ancestor),
            };

            if let Some(unresolved) = first_unresolved_field_in(&read_node, reading_node) {
                return Some(describe_field(&unresolved));
            }
        }
        None
    }

    /// What a `parent(n).a.b` or `root().a.b` read has to wait for.
    ///
    /// The path is walked as far as the tree goes, because a template may
    /// reach the last part of it through a subscript or a filter, and the
    /// deepest node reached is the smallest thing the field could be reading.
    /// A path that reaches nothing beyond an object the field is *written
    /// inside* is the exception: waiting for that object means waiting for the
    /// field itself and for every sibling that reads the same way, so two
    /// fields each naming a field their parent does not have would wait for
    /// each other for ever and be reported as a loop. Nothing exists to wait
    /// for there — the name is either supplied by a fallback such as
    /// `| default(...)`, or it is genuinely missing and reported as such once
    /// the render refuses.
    fn field_read_through(
        &self,
        start_node: &TreeNodeReference,
        segments: &[String],
        reading_node: &TreeNodeReference,
    ) -> Option<TreeNodeReference> {
        let reached = walk_path(start_node, segments)?;
        (!encloses(&reached, reading_node)).then_some(reached)
    }

    fn ancestor_reads(&self, facts: &TemplateFacts) -> Vec<AncestorRead> {
        match &facts.ancestor_reads {
            Some(reads) => reads.clone(),
            None => (0..self.ancestors.len())
                .map(|level| AncestorRead {
                    level,
                    field_path: None,
                })
                .collect(),
        }
    }

    /// Names the first thing a template reads that the configuration does not
    /// have, so a failed render can say what is missing instead of only that
    /// something was.
    ///
    /// Consulted after Jinja has already refused to render, which is what
    /// makes it safe to walk the tree this literally: a name a template uses
    /// successfully never reaches here.
    fn first_missing_reference(&self, facts: &TemplateFacts) -> Option<String> {
        for name in &facts.referenced_names {
            if !self.name_is_supplied(name) && self.field_for_name(name, facts).is_none() {
                return Some(name.clone());
            }
        }

        for referenced_path in &facts.referenced_paths {
            if !self.is_bound(referenced_path)
                && !self.reference_path_exists(referenced_path, facts)
            {
                return Some(referenced_path.clone());
            }
        }

        for read in &facts.root_reads {
            let Some(segments) = read else { continue };
            if segments.is_empty() {
                continue;
            }
            if walk_path_strictly(&self.root_node, segments).is_none() {
                return Some(format!("root().{}", segments.join(".")));
            }
        }

        for read in self.ancestor_reads(facts) {
            let Some(ancestor) = self.ancestor(read.level) else {
                continue;
            };
            let Some(segments) = &read.field_path else {
                continue;
            };
            if segments.is_empty() {
                continue;
            }
            if walk_path_strictly(ancestor, segments).is_none() {
                return Some(describe_ancestor_read(ancestor, segments));
            }
        }
        None
    }

    /// Whether the first name of a path was supplied rather than read from
    /// the configuration.
    fn is_bound(&self, referenced_path: &str) -> bool {
        let first_name = referenced_path.split('.').next().unwrap_or(referenced_path);
        self.bindings.contains_key(first_name)
    }

    /// Whether a name was supplied rather than read from the configuration,
    /// either as a plain value or as a whole node.
    fn name_is_supplied(&self, name: &str) -> bool {
        self.bindings.contains_key(name) || self.node_bindings.contains_key(name)
    }

    /// Whether a dotted path reaches a field from wherever its name binds.
    fn reference_path_exists(&self, referenced_path: &str, facts: &TemplateFacts) -> bool {
        let mut segments = referenced_path.split('.');
        let Some(first_segment) = segments.next() else {
            return true;
        };
        let Some(node) = self.field_for_name(first_segment, facts) else {
            return false;
        };
        let remaining: Vec<String> = segments.map(|segment| segment.to_string()).collect();
        walk_path_strictly(&node, &remaining).is_some()
    }

    /// Resolves a dotted path to the node it reads. A path that only partly
    /// exists — because the template reached the rest of the way through a
    /// subscript or a filter — resolves to the deepest node it does reach,
    /// which is the smallest part of the tree the template could be reading.
    fn find_dependency_node(
        &self,
        referenced_path: &str,
        facts: &TemplateFacts,
    ) -> Option<TreeNodeReference> {
        let mut segments = referenced_path.split('.');
        let mut current_node = self.field_for_name(segments.next()?, facts)?;

        for segment in segments {
            match find_child(&current_node, segment) {
                Some(child) => current_node = child,
                None => break,
            }
        }
        Some(current_node)
    }
}

/// The nearest enclosing node a file was loaded onto, which is what a bare
/// name is resolved against first.
///
/// The walk starts at the field itself, so a field that *is* a whole file
/// reads its own top level. The configuration root is always a file root, so
/// the walk never runs off the end.
fn nearest_file_root(node: &TreeNodeReference, root_node: &TreeNodeReference) -> TreeNodeReference {
    let mut current = Some(Rc::clone(node));

    while let Some(candidate) = current {
        if candidate.borrow().file_root {
            return candidate;
        }
        current = {
            let candidate_borrowed = candidate.borrow();
            candidate_borrowed
                .parent
                .as_ref()
                .and_then(|weak_parent| weak_parent.upgrade())
        };
    }
    Rc::clone(root_node)
}

/// The node, then every enclosing object up to and including the root.
fn ancestor_chain(
    node: &TreeNodeReference,
    root_node: &TreeNodeReference,
) -> Vec<TreeNodeReference> {
    let mut chain = Vec::new();
    let mut current = Some(Rc::clone(node));

    while let Some(node) = current {
        let parent = node
            .borrow()
            .parent
            .as_ref()
            .and_then(|weak_parent| weak_parent.upgrade());
        chain.push(node);
        current = parent;
    }

    if !chain.last().is_some_and(|last| Rc::ptr_eq(last, root_node)) {
        chain.push(Rc::clone(root_node));
    }
    chain
}

/// Walks a dotted path from a node, stopping at the deepest node it reaches.
fn walk_path(node: &TreeNodeReference, segments: &[String]) -> Option<TreeNodeReference> {
    let mut current_node = Rc::clone(node);
    for segment in segments {
        match find_child(&current_node, segment) {
            Some(child) => current_node = child,
            None => break,
        }
    }
    Some(current_node)
}

/// Whether a node is the field doing the reading, or an object that field is
/// written inside. Waiting for one of those is waiting for the field itself.
fn encloses(candidate: &TreeNodeReference, reading_node: &TreeNodeReference) -> bool {
    let mut current_node = Rc::clone(reading_node);
    loop {
        if Rc::ptr_eq(&current_node, candidate) {
            return true;
        }
        let parent = current_node
            .borrow()
            .parent
            .as_ref()
            .and_then(|parent| parent.upgrade());
        match parent {
            Some(parent) => current_node = parent,
            None => return false,
        }
    }
}

/// Walks a dotted path from a node, insisting that every segment exists.
fn walk_path_strictly(node: &TreeNodeReference, segments: &[String]) -> Option<TreeNodeReference> {
    let mut current_node = Rc::clone(node);
    for segment in segments {
        current_node = find_child(&current_node, segment)?;
    }
    Some(current_node)
}

/// Writes a `parent(n).x` read as the path it points at, so a warning names a
/// field the reader can go and look for.
fn describe_ancestor_read(ancestor: &TreeNodeReference, segments: &[String]) -> String {
    let ancestor_path = ancestor.borrow().get_path();
    let read_path = segments.join(".");

    if ancestor_path.is_empty() || ancestor_path == "ROOT" {
        read_path
    } else {
        format!("{}.{}", ancestor_path, read_path)
    }
}

/// How a field is named in a warning. The root has no path of its own.
fn describe_field(node: &TreeNodeReference) -> String {
    let field_path = node.borrow().get_path();
    if field_path.is_empty() || field_path == "ROOT" {
        "the configuration as a whole".to_string()
    } else {
        field_path
    }
}

fn find_child(node: &TreeNodeReference, key: &str) -> Option<TreeNodeReference> {
    node.borrow()
        .children
        .iter()
        .find(|child| child.borrow().key.as_deref() == Some(key))
        .cloned()
}

// =====================================================================
// RENDERING
// =====================================================================

/// Why a template cannot be rendered yet: either the build has not reached the
/// moment it may read what it reads, or a field it reads is itself still a
/// template.
fn not_ready_to_render(scope: &TemplateScope, facts: &TemplateFacts) -> Option<TemplateError> {
    scope.deferred_until_structure_merge(facts).or_else(|| {
        scope
            .first_unresolved_dependency(facts)
            .map(TemplateError::WaitingOn)
    })
}

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

    let facts = template_facts(template)?;
    if let Some(reason) = not_ready_to_render(scope, &facts) {
        return Err(reason);
    }

    environment_for(&facts)
        .template_from_str(template)
        .map_err(failed_to_render)?
        .render(scope.build_context(&facts))
        .map_err(|error| failed_to_render_in_scope(error, &facts, scope))
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
    memoized_render(RenderKind::Value, template, scope, || {
        render_value_template_uncached(template, scope)
    })
}

fn render_value_template_uncached(
    template: &str,
    scope: &TemplateScope,
) -> Result<Value, TemplateError> {
    if let Some(expression_source) = whole_value_expression(template) {
        return evaluate_expression(expression_source, scope);
    }

    let rendered_text = render_template_text(template, scope)?;
    let facts = template_facts(template)?;

    // Text a `{% raw %}` block produced is meant to be text: reading it back as
    // YAML would turn a literal `{{ x }}` into a flow mapping.
    if facts.has_statement_blocks
        && !facts.emits_literal_delimiters
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

/// Renders a template and reads the result as YAML, whatever shape it takes.
/// Used where a template stands in for a piece of the configuration language
/// itself, as a schema's `$template` does.
pub fn render_template_as_yaml(
    template: &str,
    scope: &TemplateScope,
) -> Result<Value, TemplateError> {
    memoized_render(RenderKind::Yaml, template, scope, || {
        render_template_as_yaml_uncached(template, scope)
    })
}

fn render_template_as_yaml_uncached(
    template: &str,
    scope: &TemplateScope,
) -> Result<Value, TemplateError> {
    let rendered_text = render_template_text(template, scope)?;

    if rendered_text.trim().is_empty() {
        return Ok(Value::Null);
    }

    serde_yaml::from_str(&rendered_text).map_err(|error| {
        TemplateError::Failed(format!(
            "the result is not valid YAML ({}):\n{}",
            error, rendered_text
        ))
    })
}

/// Evaluates a single Jinja expression (the inside of a `{{ ... }}`) and
/// returns its value.
pub fn evaluate_expression(
    expression_source: &str,
    scope: &TemplateScope,
) -> Result<Value, TemplateError> {
    let wrapped_source = format!("{{{{ {} }}}}", expression_source);
    let facts = template_facts(&wrapped_source)?;

    if let Some(reason) = not_ready_to_render(scope, &facts) {
        return Err(reason);
    }

    let environment = environment_for(&facts);
    let compiled = environment
        .compile_expression(expression_source)
        .map_err(failed_to_render)?;
    let evaluated = compiled
        .eval(scope.build_context(&facts))
        .map_err(|error| failed_to_render_in_scope(error, &facts, scope))?;

    // An expression that evaluates to an undefined value has not produced
    // anything; saying so is far more useful than writing a silent null.
    if evaluated.is_undefined() {
        return Err(match scope.first_missing_reference(&facts) {
            Some(missing) => TemplateError::missing(&missing),
            None => TemplateError::Failed(format!(
                "'{}' did not resolve to a value",
                expression_source.trim()
            )),
        });
    }

    convert_jinja_value_to_yaml(&unwrap_field_value(&evaluated))
}

/// Evaluates a condition and returns whether it holds. Conditions are
/// evaluated leniently: a field that is simply absent makes the condition
/// false instead of raising an error.
pub fn evaluate_condition(
    condition_source: &str,
    scope: &TemplateScope,
) -> Result<bool, TemplateError> {
    let expression_source = whole_value_expression(condition_source).unwrap_or(condition_source);
    let wrapped_source = format!("{{{{ {} }}}}", expression_source);
    let facts = template_facts(&wrapped_source)?;

    if let Some(reason) = not_ready_to_render(scope, &facts) {
        return Err(reason);
    }

    let compiled = lenient_environment()
        .compile_expression(expression_source)
        .map_err(failed_to_render)?;

    match compiled.eval(scope.build_context(&facts)) {
        Ok(value) => Ok(value.is_true()),
        Err(_) => Ok(false),
    }
}

fn convert_jinja_value_to_yaml(value: &JinjaValue) -> Result<Value, TemplateError> {
    serde_yaml::to_value(value)
        .map_err(|error| TemplateError::Failed(format!("could not convert result: {}", error)))
}

/// Turns Jinja's own complaint into a COAL one. Jinja reports an undefined
/// value without saying which, because as far as it is concerned the name was
/// never offered — so the scope is asked which of the fields the template
/// reads the configuration does not have.
fn failed_to_render_in_scope(
    error: JinjaError,
    facts: &TemplateFacts,
    scope: &TemplateScope,
) -> TemplateError {
    if error.kind() == ErrorKind::UndefinedError
        && let Some(missing) = scope.first_missing_reference(facts)
    {
        return TemplateError::missing(&missing);
    }
    failed_to_render(error)
}

fn failed_to_render(error: JinjaError) -> TemplateError {
    let described = match error.detail() {
        Some(detail) => format!("{}: {}", error.kind(), detail),
        None => error.kind().to_string(),
    };

    TemplateError::Failed(described)
}

// =====================================================================
// ENVIRONMENTS
// =====================================================================

static STRICT_ENVIRONMENT: OnceLock<Environment<'static>> = OnceLock::new();
static GUARDED_ENVIRONMENT: OnceLock<Environment<'static>> = OnceLock::new();
static LENIENT_ENVIRONMENT: OnceLock<Environment<'static>> = OnceLock::new();

/// Which environment a template is rendered in: the one that stops at the
/// first absent name, or the one that lets a guard have its turn.
///
/// A template that never mentions a fallback is rendered strictly, exactly as
/// it always was — an absent name is an error wherever it appears, in a
/// comparison, a concatenation or a filter alike.
fn environment_for(facts: &TemplateFacts) -> &'static Environment<'static> {
    if facts.guards_absent_values {
        guarded_environment()
    } else {
        strict_environment()
    }
}

/// The environment used for values and schema references. Undefined names are
/// an error, so an unresolvable template is reported and left in place rather
/// than silently collapsing to an empty string.
fn strict_environment() -> &'static Environment<'static> {
    STRICT_ENVIRONMENT.get_or_init(|| build_environment(UndefinedBehavior::Strict, false))
}

/// The environment used for a template that guards a read it expects may not
/// be there.
///
/// Strictness raises the moment a path passes *through* an absent field, which
/// is before any guard can run: `{{ node.telemetry.port | default(9000) }}`
/// reports `telemetry` as missing however carefully it was written, so an
/// optional object could only ever be guarded one level deep. Here an absent
/// value may be reached through, and the complaint moves to the point where a
/// value is written into the output — which a guard, by consuming the absent
/// value, never reaches. A read the guard did not cover is reported exactly as
/// it is under strictness.
fn guarded_environment() -> &'static Environment<'static> {
    GUARDED_ENVIRONMENT.get_or_init(|| build_environment(UndefinedBehavior::Chainable, true))
}

/// The environment used for conditions, where an absent field should read as
/// "false" rather than raise. A condition writes nothing out, so it has no use
/// for the output guard.
fn lenient_environment() -> &'static Environment<'static> {
    LENIENT_ENVIRONMENT.get_or_init(|| build_environment(UndefinedBehavior::Chainable, false))
}

fn build_environment(
    undefined_behavior: UndefinedBehavior,
    refuse_to_write_absent_values: bool,
) -> Environment<'static> {
    let mut environment = Environment::new();
    environment.set_undefined_behavior(undefined_behavior);
    environment.set_unknown_method_callback(call_mapping_or_string_method);
    if refuse_to_write_absent_values {
        environment.set_formatter(refuse_absent_value);
    }
    register_coal_filters(&mut environment);
    environment
}

/// Refuses to write an absent value into the output.
///
/// This is what keeps a missing field an error in a guarded template while a
/// path is still allowed to reach through one: anything that survives as far as
/// the output was not guarded after all.
fn refuse_absent_value(
    output: &mut Output,
    state: &State,
    value: &JinjaValue,
) -> Result<(), JinjaError> {
    if value.is_undefined() {
        return Err(JinjaError::from(ErrorKind::UndefinedError));
    }
    escape_formatter(output, state, value)
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
    let yaml_value: Value = serde_yaml::to_value(unwrap_field_value(&value))
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
