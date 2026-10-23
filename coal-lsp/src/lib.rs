//! The editor side of COAL, kept apart from the protocol plumbing in `main.rs`
//! so that what the editor is told can be tested without an editor.
//!
//! Everything here is synchronous and builds its own trees: a build is slow and
//! its trees are not `Send`, so the server runs these functions on a blocking
//! thread and only ever hands the results back across.

pub mod completion;
pub mod diagnostics;
pub mod editor_tree;
pub mod hover;
pub mod projects;
