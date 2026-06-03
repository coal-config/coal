use crate::coal_tree::CoalTree;
use crate::vfs::VirtualFileSystem;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Type alias for a thread-safe, shared reference to the Virtual File System.
/// Uses Arc for thread-safe reference counting and Mutex for synchronized mutable access.
pub type VfsRef = Arc<Mutex<VirtualFileSystem>>;

/// Type alias for a shared, mutable reference to a CoalTree node.
/// Uses Rc for reference counting (single-threaded) and RefCell for interior mutability.
pub type TreeRef = Rc<RefCell<CoalTree>>;

/// Helper function to create a new VfsRef
#[inline]
pub fn new_vfs_ref(vfs: VirtualFileSystem) -> VfsRef {
    Arc::new(Mutex::new(vfs))
}

/// Helper function to create a new TreeRef
#[inline]
pub fn new_tree_ref(tree: CoalTree) -> TreeRef {
    Rc::new(RefCell::new(tree))
}
