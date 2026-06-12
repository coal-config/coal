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
