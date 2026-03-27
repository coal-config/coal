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
    pub children: Vec<Rc<RefCell<CoalTree>>>,
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
        let mut path_elements = Vec::new();
        if let Some(ref current_key) = self.key {
            path_elements.push(current_key.clone());
        }

        let mut current_parent = self.parent.as_ref().and_then(|weak| weak.upgrade());

        while let Some(parent_reference) = current_parent {
            let parent_borrowed = parent_reference.borrow();
            if let Some(ref parent_key) = parent_borrowed.key {
                if parent_key != "ROOT" && parent_key != "TEMP" {
                    path_elements.push(parent_key.clone());
                }
            }
            current_parent = parent_borrowed.parent.as_ref().and_then(|weak| weak.upgrade());
        }

        path_elements.reverse();
        path_elements.join(".")
    }

    pub fn is_list_type(&self) -> bool {
        if self.children.is_empty() {
            return false;
        }
        self.children.iter().all(|child| {
            child.borrow().key.as_deref().unwrap_or("").parse::<usize>().is_ok()
        })
    }

    pub fn is_object_type(&self) -> bool {
        if let Some(ref value) = self.value {
            if value.is_mapping() { return true; }
        }
        if !self.children.is_empty() && !self.is_list_type() {
            return true;
        }
        false
    }
}

pub fn tree_to_yaml(current_tree_node: Rc<RefCell<CoalTree>>) -> Value {
    let node_borrowed = current_tree_node.borrow();

    if node_borrowed.children.is_empty() {
        return node_borrowed.value.clone().unwrap_or(Value::Null);
    }

    if node_borrowed.is_list_type() {
        let sequence = node_borrowed.children.iter()
            .map(|child| tree_to_yaml(Rc::clone(child)))
            .collect();
        Value::Sequence(sequence)
    } else {
        let mut mapping = Mapping::new();
        for child in &node_borrowed.children {
            if let Some(ref key) = child.borrow().key {
                mapping.insert(Value::String(key.clone()), tree_to_yaml(Rc::clone(child)));
            }
        }
        Value::Mapping(mapping)
    }
}