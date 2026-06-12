use crate::coal_tree::CoalTree;
use crate::virtual_file_system::VirtualFileSystem;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// A thread-safe, shared reference to the Virtual File System.
/// Uses Arc for thread-safe reference counting and Mutex for synchronized mutable access.
pub type VirtualFileSystemReference = Arc<Mutex<VirtualFileSystem>>;

/// A shared, mutable reference to a CoalTree node.
/// Uses Rc for reference counting (single-threaded) and RefCell for interior mutability.
pub type TreeNodeReference = Rc<RefCell<CoalTree>>;

/// Wraps a VirtualFileSystem in a new shared reference.
#[inline]
pub fn new_virtual_file_system_reference(
    virtual_file_system: VirtualFileSystem,
) -> VirtualFileSystemReference {
    Arc::new(Mutex::new(virtual_file_system))
}

/// Wraps a CoalTree node in a new shared reference.
#[inline]
pub fn new_tree_node_reference(tree_node: CoalTree) -> TreeNodeReference {
    Rc::new(RefCell::new(tree_node))
}
