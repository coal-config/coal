//! What the editor is offered and told, checked against small projects written
//! to a temporary directory.

use coal::type_aliases::{VirtualFileSystemReference, new_virtual_file_system_reference};
use coal::virtual_file_system::{VirtualFileSystem, normalize_path};
use coal_lsp::completion::complete;
use coal_lsp::diagnostics::collect_workspace_diagnostics;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tower_lsp_server::ls_types::{CompletionItem, CompletionTextEdit};

struct Project {
    root: PathBuf,
    vfs: VirtualFileSystemReference,
}

impl Project {
    fn new(name: &str, files: &[(&str, &str)]) -> Project {
        let root = std::env::temp_dir().join(format!("coal-lsp-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (path, content) in files {
            let full = root.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, content).unwrap();
        }
        Project {
            root,
            vfs: new_virtual_file_system_reference(VirtualFileSystem::new()),
        }
    }

    fn root(&self) -> String {
        normalize_path(&self.root.to_string_lossy())
    }

    fn path(&self, file: &str) -> String {
        normalize_path(&self.root.join(file).to_string_lossy())
    }

    /// What an editor with unsaved changes would send.
    fn edit(&self, file: &str, content: &str) {
        self.vfs.lock().unwrap().set_file_content(&self.path(file), content.to_string());
    }

    /// Completions with the cursor at the `|` in the file's content, which is
    /// written to the editor buffer without the marker.
    fn complete_at(&self, file: &str, content_with_cursor: &str) -> Vec<CompletionItem> {
        let (line, character) = cursor_position(content_with_cursor);
        self.edit(file, &content_with_cursor.replace('|', ""));
        complete(&self.root(), &self.vfs, &self.path(file), line, character).unwrap_or_default()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn cursor_position(content: &str) -> (u32, u32) {
    for (line_index, line) in content.lines().enumerate() {
        if let Some(column) = line.find('|') {
            return (line_index as u32, line[..column].encode_utf16().count() as u32);
        }
    }
    panic!("no cursor marker in test content");
}

fn labels(items: &[CompletionItem]) -> Vec<String> {
    let mut labels: Vec<String> = items.iter().map(|item| item.label.clone()).collect();
    labels.sort();
    labels
}

const LIST_SCHEMA: &str = "\
mode: [fast, slow]
colors: [\"{{ ['red', 'green', 'blue'] }}\"]
$optional:
  sizes: \"[{{ ['s', 'm'] }}]\"
  app: any
";

fn list_project(name: &str) -> Project {
    Project::new(
        name,
        &[
            ("_schema/_self.yaml", LIST_SCHEMA),
            ("_self.yaml", "mode: fast\ncolors:\n  - red\n"),
            ("sizes.yaml", "- s\n"),
        ],
    )
}

// =====================================================================
// allowed values of list items
// =====================================================================

#[test]
fn an_empty_list_item_offers_the_allowed_values() {
    let project = list_project("empty-item");
    let items = project.complete_at("_self.yaml", "mode: fast\ncolors:\n  - red\n  - |\n");
    assert_eq!(labels(&items), ["blue", "green", "red"]);
}

#[test]
fn an_empty_list_item_followed_by_another_item_offers_the_allowed_values() {
    let project = list_project("empty-item-between");
    let items = project.complete_at("_self.yaml", "mode: fast\ncolors:\n  - |\n  - red\n");
    assert_eq!(labels(&items), ["blue", "green", "red"]);
}

#[test]
fn an_empty_list_item_at_the_end_of_the_file_offers_the_allowed_values() {
    let project = list_project("empty-item-eof");
    let items = project.complete_at("_self.yaml", "mode: fast\ncolors:\n  - red\n  - |");
    assert_eq!(labels(&items), ["blue", "green", "red"]);
}

#[test]
fn a_partly_typed_list_item_offers_the_allowed_values() {
    let project = list_project("partial-item");
    let items = project.complete_at("_self.yaml", "mode: fast\ncolors:\n  - gr|\n");
    assert_eq!(labels(&items), ["blue", "green", "red"]);
}

#[test]
fn an_item_of_an_unindented_list_offers_the_allowed_values() {
    let project = list_project("unindented-item");
    let items = project.complete_at("_self.yaml", "mode: fast\ncolors:\n- red\n- |\n");
    assert_eq!(labels(&items), ["blue", "green", "red"]);
}

#[test]
fn an_item_of_a_file_holding_a_list_offers_the_allowed_values() {
    let project = list_project("list-file");
    let items = project.complete_at("sizes.yaml", "- s\n- |\n");
    assert_eq!(labels(&items), ["m", "s"]);
}

#[test]
fn a_field_with_allowed_values_still_offers_them() {
    let project = list_project("scalar-field");
    let items = project.complete_at("_self.yaml", "mode: |\ncolors:\n  - red\n");
    assert_eq!(labels(&items), ["fast", "slow"]);
}

// =====================================================================
// $include
// =====================================================================

fn include_project(name: &str) -> Project {
    Project::new(
        name,
        &[
            ("_definitions/templates/base.yaml", "retries: 3\n"),
            ("_definitions/templates/heavy.yaml", "workers: 12\nnested:\n  depth: 1\n"),
            ("nodes/_definitions/sizes.yaml", "small: 1\nlarge: 9\n"),
            ("nodes/_definitions/templates/base.yaml", "retries: 5\ndebug: true\n"),
            ("nodes/cop01.yaml", "id: 1\n"),
            ("_schema/_definitions/port_number.yaml", "integer\n"),
            ("_schema/_optional/app.yaml", "any\n"),
            ("_self.yaml", "app:\n  retries: 1\n"),
        ],
    )
}

#[test]
fn an_include_offers_the_top_level_definitions() {
    let project = include_project("include-top");
    let items = project.complete_at("_self.yaml", "app:\n  $include: |\n");
    // nodes/_definitions is only visible inside nodes/.
    assert_eq!(labels(&items), ["templates"]);
}

#[test]
fn an_include_in_a_subdirectory_also_offers_its_own_definitions() {
    let project = include_project("include-subdirectory");
    let items = project.complete_at("nodes/cop01.yaml", "id: 1\n$include: |\n");
    assert_eq!(labels(&items), ["sizes", "templates"]);
}

#[test]
fn an_include_in_a_subdirectory_sees_the_merged_definition() {
    let project = include_project("include-merged");
    let items = project.complete_at("nodes/cop01.yaml", "id: 1\n$include: templates.base.|\n");
    assert_eq!(labels(&items), ["debug", "retries"]);
}

#[test]
fn an_include_offers_what_the_typed_path_holds() {
    let project = include_project("include-level");
    let items = project.complete_at("_self.yaml", "app:\n  $include: templates.|\n");
    assert_eq!(labels(&items), ["base", "heavy"]);
}

#[test]
fn an_include_path_is_completed_one_segment_at_a_time() {
    let project = include_project("include-segment");
    let items = project.complete_at("_self.yaml", "app:\n  $include: templates.heavy.ne|\n");
    assert_eq!(labels(&items), ["nested", "workers"]);

    // Only "ne" is replaced, so the editor filters on the segment.
    let nested = items.iter().find(|item| item.label == "nested").unwrap();
    let Some(CompletionTextEdit::Edit(edit)) = &nested.text_edit else {
        panic!("expected a text edit");
    };
    assert_eq!(edit.range.start.character, 28);
    assert_eq!(edit.range.end.character, 30);
}

#[test]
fn an_include_written_as_a_list_offers_definitions() {
    let project = include_project("include-list");
    let items = project.complete_at(
        "_self.yaml",
        "app:\n  $include:\n    - templates.base\n    - templates.|\n",
    );
    assert_eq!(labels(&items), ["base", "heavy"]);
}

#[test]
fn an_optional_include_offers_definitions() {
    let project = include_project("include-optional");
    let items = project.complete_at("_self.yaml", "app:\n  $include_optional: \"templ|\n");
    assert_eq!(labels(&items), ["templates"]);
}

#[test]
fn an_include_in_the_schema_offers_the_schema_definitions() {
    let project = include_project("include-schema");
    let items = project.complete_at("_schema/_self.yaml", "port:\n  $include: |\n");
    assert_eq!(labels(&items), ["port_number"]);
}

#[test]
fn an_include_offers_nothing_below_a_name_that_is_not_there() {
    let project = include_project("include-missing");
    let items = project.complete_at("_self.yaml", "app:\n  $include: nothing.|\n");
    assert!(items.is_empty());
}

// =====================================================================
// diagnostics
// =====================================================================

#[test]
fn a_fixed_mistake_leaves_no_diagnostic_behind() {
    let project = list_project("fixed-mistake");
    let self_file = project.path("_self.yaml");

    project.edit("_self.yaml", "mode: quick\ncolors:\n  - red\n");
    let broken = collect_workspace_diagnostics(&project.root(), &project.vfs, None);
    assert_eq!(broken.values().map(Vec::len).sum::<usize>(), 1, "{:?}", broken);

    project.edit("_self.yaml", "mode: fast\ncolors:\n  - red\n");
    let fixed = collect_workspace_diagnostics(&project.root(), &project.vfs, None);
    assert!(fixed.is_empty(), "{:?}", fixed);
    let _ = self_file;
}

#[test]
fn a_finding_about_an_empty_list_item_is_on_the_item_line() {
    let project = list_project("empty-item-line");
    project.edit("_self.yaml", "mode: fast\ncolors:\n  - \n  - red\n");
    let diagnostics = collect_workspace_diagnostics(&project.root(), &project.vfs, None);
    let lines: Vec<u32> = diagnostics
        .values()
        .flatten()
        .map(|diagnostic| diagnostic.range.start.line)
        .collect();
    assert_eq!(lines, [2]);
}

#[test]
fn a_build_that_cannot_finish_is_reported_in_the_edited_file() {
    let project = include_project("fatal");
    project.edit("_self.yaml", "app:\n  $include: nothing.here\n");
    let fallback = project.path("_self.yaml");
    let diagnostics =
        collect_workspace_diagnostics(&project.root(), &project.vfs, Some(fallback.as_str()));

    assert_eq!(diagnostics.len(), 1, "{:?}", diagnostics);
    let (url, file_diagnostics) = diagnostics.iter().next().unwrap();
    assert_eq!(normalize_path(&url.to_file_path().unwrap().to_string_lossy()), fallback);
    assert!(file_diagnostics[0].message.starts_with("Fatal Error"));
    let _ = Arc::clone(&project.vfs);
}
