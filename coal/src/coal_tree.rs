use crate::type_aliases::TreeRef;
use serde_yaml::{Mapping, Value};
use std::cell::RefCell;
use std::rc::{Rc, Weak};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Span {
    pub line: usize,
    pub col: usize,
}

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
    pub children: Vec<TreeRef>,
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

        let mut current_parent = self.get_parent_rc();

        while let Some(parent_reference) = current_parent {
            let parent_borrowed = parent_reference.borrow();
            Self::add_valid_parent_key(&mut path_elements, &parent_borrowed);
            current_parent = parent_borrowed.get_parent_rc();
        }

        path_elements
    }

    fn get_parent_rc(&self) -> Option<TreeRef> {
        self.parent.as_ref().and_then(|weak| weak.upgrade())
    }

    fn add_valid_parent_key(path_elements: &mut Vec<String>, parent: &CoalTree) {
        if let Some(ref parent_key) = parent.key {
            if Self::is_valid_path_key(parent_key) {
                path_elements.push(parent_key.clone());
            }
        }
    }

    fn is_valid_path_key(key: &str) -> bool {
        key != "ROOT" && key != "TEMP"
    }

    pub fn is_list_type(&self) -> bool {
        if self.children.is_empty() {
            return false;
        }
        self.children.iter().all(|child| {
            Self::is_numeric_key(&child.borrow().key)
        })
    }

    fn is_numeric_key(key: &Option<String>) -> bool {
        key.as_deref().unwrap_or("").parse::<usize>().is_ok()
    }

    pub fn is_object_type(&self) -> bool {
        self.has_mapping_value() || self.has_object_children()
    }

    fn has_mapping_value(&self) -> bool {
        self.value.as_ref().map_or(false, |val| val.is_mapping())
    }

    fn has_object_children(&self) -> bool {
        !self.children.is_empty() && !self.is_list_type()
    }
}

pub fn tree_to_yaml(current_tree_node: TreeRef) -> Value {
    let node_borrowed = current_tree_node.borrow();

    if node_borrowed.children.is_empty() {
        return get_node_value(&node_borrowed);
    }

    if node_borrowed.is_list_type() {
        convert_children_to_sequence(&node_borrowed.children)
    } else {
        convert_children_to_mapping(&node_borrowed.children)
    }
}

fn get_node_value(node: &CoalTree) -> Value {
    node.value.clone().unwrap_or(Value::Null)
}

fn convert_children_to_sequence(children: &[TreeRef]) -> Value {
    let sequence = children.iter()
        .map(|child| tree_to_yaml(Rc::clone(child)))
        .collect();
    Value::Sequence(sequence)
}

fn convert_children_to_mapping(children: &[TreeRef]) -> Value {
    let mut mapping = Mapping::new();
    for child in children {
        if let Some(ref key) = child.borrow().key {
            mapping.insert(Value::String(key.clone()), tree_to_yaml(Rc::clone(child)));
        }
    }
    Value::Mapping(mapping)
}