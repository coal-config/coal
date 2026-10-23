//! Which `_definitions` an `$include` can see.
//!
//! Every `_definitions` folder belongs to the directory that holds it, and is
//! visible to everything written inside that directory. An include therefore
//! sees the definitions of the directory its file sits in and of every
//! directory above it, up to the configuration root, merged from the top down:
//! where two levels define the same name, the deeper one wins. Objects are
//! merged field by field, so a lower level only has to write what it changes;
//! a plain value or a list is replaced as a whole.
//!
//! A folder's definitions are invisible outside it, so two sibling folders may
//! define the same name without getting in each other's way.

use crate::coal_tree::{CoalTree, tree_to_yaml};
use crate::tree_operations::clone_tree_node_recursively;
use crate::type_aliases::{TreeNodeReference, new_tree_node_reference};
use crate::virtual_file_system::normalize_path;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::rc::Rc;

/// The key the root of a definitions namespace carries.
pub const DEFINITIONS_ROOT_KEY: &str = "DEFINITIONS";

/// The `_definitions` of a configuration, by the directory that declares them.
///
/// Cheap to clone: clones share the definitions and the merged views.
#[derive(Clone)]
pub struct DefinitionScopes {
    inner: Rc<ScopesInner>,
}

struct ScopesInner {
    /// The configuration root, normalized. Nothing above it is consulted.
    root_directory: String,
    /// What each directory's own `_definitions` hold, not yet merged with the
    /// levels above.
    by_directory: RefCell<BTreeMap<String, TreeNodeReference>>,
    /// The merged namespace per directory, built the first time it is asked
    /// for. Cleared whenever a directory contributes something new.
    merged: RefCell<HashMap<String, TreeNodeReference>>,
}

impl DefinitionScopes {
    pub fn new(root_directory: &str) -> Self {
        Self {
            inner: Rc::new(ScopesInner {
                root_directory: normalize_path(root_directory),
                by_directory: RefCell::new(BTreeMap::new()),
                merged: RefCell::new(HashMap::new()),
            }),
        }
    }

    /// The namespace node `directory`'s own `_definitions` are merged into,
    /// created empty the first time.
    pub fn own_namespace_of(&self, directory: &str) -> TreeNodeReference {
        let directory = normalize_path(directory);
        self.inner.merged.borrow_mut().clear();
        Rc::clone(
            self.inner
                .by_directory
                .borrow_mut()
                .entry(directory.clone())
                .or_insert_with(|| empty_namespace(&directory)),
        )
    }

    /// Every directory that declares definitions, top-down.
    pub fn declaring_directories(&self) -> Vec<String> {
        self.inner.by_directory.borrow().keys().cloned().collect()
    }

    /// What an include written in `directory` can see.
    pub fn visible_from_directory(&self, directory: &str) -> TreeNodeReference {
        let directory = normalize_path(directory);
        if let Some(merged) = self.inner.merged.borrow().get(&directory) {
            return Rc::clone(merged);
        }

        let merged = empty_namespace(&directory);
        for level in self.levels_down_to(&directory) {
            if let Some(own) = self.inner.by_directory.borrow().get(&level) {
                overlay(own, &merged);
            }
        }

        self.inner
            .merged
            .borrow_mut()
            .insert(directory, Rc::clone(&merged));
        merged
    }

    /// What an include written at `origin` — a file, or the folder a field came
    /// from — can see.
    pub fn visible_from_origin(&self, origin: Option<&str>) -> TreeNodeReference {
        match origin {
            Some(origin) => self.visible_from_directory(&directory_of_origin(origin)),
            None => self.visible_from_root(),
        }
    }

    /// What an include written at the configuration root can see.
    pub fn visible_from_root(&self) -> TreeNodeReference {
        let root = self.inner.root_directory.clone();
        self.visible_from_directory(&root)
    }

    /// The root first, then each directory on the way down to `directory`.
    /// A directory outside the configuration sees the root's definitions only.
    fn levels_down_to(&self, directory: &str) -> Vec<String> {
        let root = &self.inner.root_directory;
        let mut levels = vec![root.clone()];

        let Some(below_root) = directory.strip_prefix(root.as_str()) else {
            return levels;
        };
        if !below_root.is_empty() && !below_root.starts_with('/') && !root.ends_with('/') {
            // `/a/bc` is not below `/a/b`.
            return levels;
        }

        let mut current = root.trim_end_matches('/').to_string();
        for segment in below_root.split('/').filter(|segment| !segment.is_empty()) {
            current = format!("{}/{}", current, segment);
            levels.push(current.clone());
        }
        levels
    }
}

/// The directory a node written at `origin` belongs to.
pub fn directory_of_origin(origin: &str) -> String {
    let normalized = normalize_path(origin);
    let lowered = normalized.to_ascii_lowercase();
    let names_a_file = lowered.ends_with(".yaml") || lowered.ends_with(".yml");

    match names_a_file || Path::new(&normalized).is_file() {
        true => Path::new(&normalized)
            .parent()
            .map(|parent| normalize_path(&parent.to_string_lossy()))
            .unwrap_or(normalized),
        false => normalized,
    }
}

fn empty_namespace(directory: &str) -> TreeNodeReference {
    new_tree_node_reference(CoalTree {
        key: Some(DEFINITIONS_ROOT_KEY.to_string()),
        origin: Some(directory.to_string()),
        file_root: true,
        ..Default::default()
    })
}

/// Lays `source` over `target`: objects merge field by field, and anything
/// else in `source` replaces what `target` had.
fn overlay(source: &TreeNodeReference, target: &TreeNodeReference) {
    let source_children = source.borrow().children.clone();

    for source_child in source_children {
        let key = source_child.borrow().key.clone();
        let existing = target
            .borrow()
            .children
            .iter()
            .position(|child| child.borrow().key == key);

        let Some(position) = existing else {
            let copy = clone_tree_node_recursively(source_child, Rc::clone(target));
            target.borrow_mut().children.push(copy);
            continue;
        };

        let existing_child = Rc::clone(&target.borrow().children[position]);
        if is_object(&existing_child) && is_object(&source_child) {
            overlay(&source_child, &existing_child);
            // Keep the recorded value in step with the merged fields, the way
            // a loaded object records the mapping it holds.
            let merged_value = tree_to_yaml(Rc::clone(&existing_child));
            existing_child.borrow_mut().value = Some(merged_value);
            continue;
        }

        let copy = clone_tree_node_recursively(source_child, Rc::clone(target));
        target.borrow_mut().children[position] = copy;
    }
}

/// An object, as opposed to a plain value or a list.
fn is_object(node: &TreeNodeReference) -> bool {
    let node = node.borrow();
    !node.children.is_empty() && !node.is_list_type()
}
