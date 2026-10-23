use crate::type_aliases::TreeNodeReference;
use serde_yaml::{Mapping, Value};
use std::cell::RefCell;
use std::rc::{Rc, Weak};

/// The physical location (1-based line and column) a node originated from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Span {
    pub line: usize,
    pub col: usize,
}

/// A single node in the configuration tree. Each node tracks both its data
/// (key, value, children) and the validation state attached to it by the
/// schema engine (field_type, optional, allowed, defaults).
#[derive(Debug)]
pub struct CoalTree {
    pub key: Option<String>,
    pub value: Option<Value>,
    pub parent: Option<Weak<RefCell<CoalTree>>>,
    pub field_type: Option<String>,
    pub origin: Option<String>,
    pub schema_origin: Option<String>,
    pub optional: bool,
    pub default_value: Option<Value>,
    pub allowed_values: Option<Vec<Value>>,
    pub allowed: bool,
    /// Whether an object schema has already been matched against this node's
    /// children, which is what decides that a key nobody declared is not
    /// allowed. A later schema block layered on top of that one only adds to
    /// it; a node that has never been checked is validated from scratch.
    pub object_fields_checked: bool,
    /// Whether the Jinja engine has finished with this field. A value that is
    /// final but happens to *look* like a template — the output of a
    /// `{% raw %}` block — must not be rendered again, and must not make the
    /// fields that read it wait forever.
    pub template_resolved: bool,
    /// A problem with the schema definition itself (unknown type name,
    /// unresolvable reference, malformed definition) that applied to this
    /// node. Reported as a "SchemaError" diagnostic.
    pub schema_error: Option<String>,
    /// Whether a file's contents were loaded directly onto this node, which
    /// makes it the root a template written in that file resolves names
    /// against before falling back to the configuration root.
    ///
    /// The flag travels with the node when it is cloned, so content copied
    /// out of an `_each` folder or pulled in by a structure layer keeps
    /// resolving against the file it was written in — as that file sits in
    /// the place the copy landed, not where it was declared.
    pub file_root: bool,
    /// Whether an `_each` template has already copied its *directives* onto
    /// this field.
    ///
    /// Ordinary fields can be copied again harmlessly, because a value already
    /// there wins. A directive cannot: it is consumed by the phase that
    /// carries it out, so finding it gone is not evidence the field never had
    /// it, and copying it in again would set the build going round forever.
    ///
    /// The flag is cleared on a copy, so a field a structure layer moves
    /// somewhere new is offered its new neighbours' template.
    pub each_directives_copied: bool,
    /// For an `$include` that arrived inside an included definition: the
    /// directory whose `_definitions` it reads. Such an include reads what the
    /// place that asked for the definition sees, not what the definition's own
    /// file sees, so a lower level's override reaches through a shared
    /// definition. Anywhere else this is unset and the node's own file decides.
    pub include_scope: Option<String>,
    pub children: Vec<TreeNodeReference>,
    pub span: Option<Span>,
}

impl Default for CoalTree {
    fn default() -> Self {
        Self {
            key: None,
            value: None,
            parent: None,
            field_type: None,
            origin: None,
            schema_origin: None,
            optional: false,
            default_value: None,
            allowed_values: None,
            allowed: true,
            object_fields_checked: false,
            template_resolved: false,
            schema_error: None,
            file_root: false,
            each_directives_copied: false,
            include_scope: None,
            children: Vec::new(),
            span: None,
        }
    }
}

impl CoalTree {
    /// Builds the dotted path from the root to this node, excluding internal
    /// marker keys such as "ROOT".
    pub fn get_path(&self) -> String {
        let mut path_elements = self.collect_path_elements();
        path_elements.reverse();
        path_elements.join(".")
    }

    fn collect_path_elements(&self) -> Vec<String> {
        let mut path_elements = Vec::new();

        if let Some(ref current_key) = self.key {
            path_elements.push(current_key.clone());
        }

        let mut current_parent = self.get_parent_node();

        while let Some(parent_node) = current_parent {
            let parent_borrowed = parent_node.borrow();
            Self::push_visible_parent_key(&mut path_elements, &parent_borrowed);
            current_parent = parent_borrowed.get_parent_node();
        }

        path_elements
    }

    fn get_parent_node(&self) -> Option<TreeNodeReference> {
        self.parent
            .as_ref()
            .and_then(|weak_parent| weak_parent.upgrade())
    }

    fn push_visible_parent_key(path_elements: &mut Vec<String>, parent: &CoalTree) {
        if let Some(ref parent_key) = parent.key
            && Self::is_visible_path_key(parent_key)
        {
            path_elements.push(parent_key.clone());
        }
    }

    fn is_visible_path_key(key: &str) -> bool {
        key != "ROOT" && key != "TEMP"
    }

    /// A node is a list when it has children and every child key is numeric.
    pub fn is_list_type(&self) -> bool {
        if self.children.is_empty() {
            return false;
        }
        self.children
            .iter()
            .all(|child| Self::is_numeric_key(&child.borrow().key))
    }

    fn is_numeric_key(key: &Option<String>) -> bool {
        key.as_deref().unwrap_or("").parse::<usize>().is_ok()
    }

    /// A node is an object when it holds a mapping value or has non-list
    /// children.
    pub fn is_object_type(&self) -> bool {
        self.has_mapping_value() || self.has_object_children()
    }

    fn has_mapping_value(&self) -> bool {
        self.value.as_ref().is_some_and(|value| value.is_mapping())
    }

    fn has_object_children(&self) -> bool {
        !self.children.is_empty() && !self.is_list_type()
    }
}

/// Converts the tree into a plain serde_yaml value: leaf nodes contribute
/// their values, list nodes become sequences, and the rest become mappings.
pub fn tree_to_yaml(current_node: TreeNodeReference) -> Value {
    let node_borrowed = current_node.borrow();

    if node_borrowed.children.is_empty() {
        return node_borrowed.value.clone().unwrap_or(Value::Null);
    }

    if node_borrowed.is_list_type() {
        convert_children_to_sequence(&node_borrowed.children)
    } else {
        convert_children_to_mapping(&node_borrowed.children)
    }
}

fn convert_children_to_sequence(children: &[TreeNodeReference]) -> Value {
    let sequence = children
        .iter()
        .map(|child| tree_to_yaml(Rc::clone(child)))
        .collect();
    Value::Sequence(sequence)
}

fn convert_children_to_mapping(children: &[TreeNodeReference]) -> Value {
    let mut mapping = Mapping::new();
    for child in children {
        if let Some(ref key) = child.borrow().key {
            mapping.insert(Value::String(key.clone()), tree_to_yaml(Rc::clone(child)));
        }
    }
    Value::Mapping(mapping)
}

/// Converts the tree into a value for template evaluation. It differs from
/// [`tree_to_yaml`] in two ways, and both are about showing a template only
/// what the configuration actually says: system directives ('$'-prefixed keys)
/// are invisible, exactly as they are in the compiled output, and so are the
/// empty placeholders the schema engine creates for missing fields. A template
/// that asks for the keys of a mapping therefore never sees a `$select` that
/// has not been consumed yet, nor a field that only a schema asked for.
pub fn tree_to_template_value(current_node: TreeNodeReference) -> Value {
    let node_borrowed = current_node.borrow();

    let value_children: Vec<TreeNodeReference> = node_borrowed
        .children
        .iter()
        .filter(|child| {
            !is_system_directive_key(&child.borrow().key) && !is_schema_placeholder(child)
        })
        .cloned()
        .collect();

    if value_children.is_empty() {
        return node_borrowed
            .value
            .clone()
            .map(without_system_directive_keys)
            .unwrap_or(Value::Null);
    }

    if value_children
        .iter()
        .all(|child| is_numeric_child_key(&child.borrow().key))
    {
        return Value::Sequence(
            value_children
                .iter()
                .map(|child| tree_to_template_value(Rc::clone(child)))
                .collect(),
        );
    }

    let mut mapping = Mapping::new();
    for child in &value_children {
        if let Some(ref key) = child.borrow().key {
            mapping.insert(
                Value::String(key.clone()),
                tree_to_template_value(Rc::clone(child)),
            );
        }
    }
    Value::Mapping(mapping)
}

/// A node the schema engine created for a field the configuration never
/// declared. It exists so a missing mandatory field can be reported, and it is
/// deliberately invisible to templates: a schema must not be able to conjure
/// up a key that the configuration does not actually have.
///
/// A placeholder holds no value of its own, and a placeholder for a field
/// declared as an object holds nothing but further placeholders. The moment
/// any of them gains a value — a default, say — there is something real there
/// and the node becomes visible again.
fn is_schema_placeholder(node: &TreeNodeReference) -> bool {
    let node_borrowed = node.borrow();
    node_borrowed.value.is_none() && node_borrowed.children.iter().all(is_schema_placeholder)
}

fn is_system_directive_key(key: &Option<String>) -> bool {
    key.as_deref().is_some_and(|key| key.starts_with('$'))
}

fn is_numeric_child_key(key: &Option<String>) -> bool {
    key.as_deref().unwrap_or("").parse::<usize>().is_ok()
}

fn without_system_directive_keys(mut value: Value) -> Value {
    crate::yaml_values::strip_system_directive_keys_from_value(&mut value);
    value
}
