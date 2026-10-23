//! A list item is written on the line of its dash, including one left empty —
//! which is what every list looks like while an item is being typed — and
//! including the items of a file that holds nothing but a list.
use coal::tree_builder::build_entire_coal_configuration_tree;
use coal::type_aliases::{TreeNodeReference, new_virtual_file_system_reference};
use coal::virtual_file_system::VirtualFileSystem;
use std::fs;
use std::path::PathBuf;

fn build(name: &str, files: &[(&str, &str)]) -> (PathBuf, TreeNodeReference) {
    let root = std::env::temp_dir().join(format!("coal-positions-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (path, content) in files {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
    let vfs = new_virtual_file_system_reference(VirtualFileSystem::new());
    let (tree, _) = build_entire_coal_configuration_tree(root.to_str().unwrap(), vfs, false, &[], &[])
        .expect("project should build");
    (root, tree)
}

fn item_lines(tree: &TreeNodeReference, list: &str) -> Vec<usize> {
    let list_node = tree
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key.as_deref() == Some(list))
        .cloned()
        .expect("list is in the tree");
    let mut lines: Vec<(usize, usize)> = list_node
        .borrow()
        .children
        .iter()
        .map(|item| {
            let item = item.borrow();
            (
                item.key.as_deref().unwrap().parse().unwrap(),
                item.span.as_ref().unwrap().line,
            )
        })
        .collect();
    lines.sort();
    lines.into_iter().map(|(_, line)| line).collect()
}

#[test]
fn empty_items_are_on_the_line_of_their_dash() {
    let (root, tree) = build(
        "empty",
        &[(
            "_self.yaml",
            "colors:\n  - red\n  - \n  -\n  - blue\n  -   # nothing yet\n\n  - \"\"\nnext: 1\n",
        )],
    );
    assert_eq!(item_lines(&tree, "colors"), [2, 3, 4, 5, 6, 8]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_empty_last_item_is_on_the_line_of_its_dash() {
    let (root, tree) = build("last", &[("_self.yaml", "colors:\n- red\n- ")]);
    assert_eq!(item_lines(&tree, "colors"), [2, 3]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn the_items_of_a_list_file_are_on_their_own_lines() {
    let (root, tree) = build("file", &[("sizes.yaml", "# sizes\n- s\n- \n- m\n")]);
    assert_eq!(item_lines(&tree, "sizes"), [2, 3, 4]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn quoted_empty_items_stay_where_they_are_written() {
    let (root, tree) = build("quoted", &[("_self.yaml", "colors:\n  - \"\"\n  - ''\n  -\n  - ''\n")]);
    assert_eq!(item_lines(&tree, "colors"), [2, 3, 4, 5]);
    let _ = fs::remove_dir_all(root);
}
