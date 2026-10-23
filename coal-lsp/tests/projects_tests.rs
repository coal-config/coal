//! How the server finds the COAL projects in a workspace, and how the findings
//! of several projects are combined.

use coal::virtual_file_system::normalize_path;
use coal_lsp::projects::{
    PublishedDiagnostics, can_change_project_roots, discover_project_roots, enclosing_project_root,
    is_within, nearest_project, projects_containing,
};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tower_lsp_server::ls_types::{Diagnostic, Position, Range, Uri};

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str, files: &[&str]) -> Workspace {
        let root = std::env::temp_dir().join(format!("coal-projects-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for file in files {
            let full = root.join(file);
            match file.ends_with('/') {
                true => fs::create_dir_all(&full).unwrap(),
                false => {
                    fs::create_dir_all(full.parent().unwrap()).unwrap();
                    fs::write(full, "").unwrap();
                }
            }
        }
        Workspace { root }
    }

    fn path(&self, relative: &str) -> String {
        normalize_path(&self.root.join(relative).to_string_lossy())
    }

    fn found(&self) -> Vec<String> {
        let prefix = format!("{}/", self.path(""));
        discover_project_roots(&self.root)
            .into_iter()
            .map(|root| root.strip_prefix(&prefix).map(str::to_string).unwrap_or_else(|| ".".to_string()))
            .collect()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_workspace_without_markers_holds_no_project() {
    let workspace = Workspace::new("none", &["docs/notes.yaml", "ci/pipeline.yml", "_self.yaml"]);
    assert!(workspace.found().is_empty());
}

#[test]
fn each_kind_of_marker_makes_a_root() {
    let workspace = Workspace::new(
        "markers",
        &[
            "structured/_structure.yaml",
            "structured_yml/_structure.yml",
            "schema_only/_schema/",
            "marked/.coal",
            "plain/users.yaml",
        ],
    );
    assert_eq!(workspace.found(), vec!["marked", "schema_only", "structured", "structured_yml"]);
}

#[test]
fn the_workspace_folder_itself_can_be_the_root() {
    let workspace = Workspace::new("self", &["_structure.yaml", "nodes/cop01/_self.yaml"]);
    assert_eq!(workspace.found(), vec!["."]);
}

#[test]
fn reserved_hidden_and_build_folders_are_not_searched() {
    let workspace = Workspace::new(
        "skipped",
        &[
            "project/_schema/",
            "project/_definitions/deep/_schema/",
            "_parked/old/_structure.yaml",
            ".git/modules/x/_structure.yaml",
            "node_modules/pkg/_schema/",
            "target/debug/.coal",
        ],
    );
    assert_eq!(workspace.found(), vec!["project"]);
}

#[test]
fn a_project_marked_inside_another_is_found_too() {
    let workspace = Workspace::new("nested", &["outer/_structure.yaml", "outer/sites/lab/.coal"]);
    assert_eq!(workspace.found(), vec!["outer", "outer/sites/lab"]);
}

#[test]
fn a_file_belongs_to_the_nearest_root_above_it() {
    let workspace = Workspace::new(
        "nearest",
        &["outer/_structure.yaml", "outer/sites/lab/.coal", "outer/sites/lab/users.yaml"],
    );
    let roots = discover_project_roots(&workspace.root);
    let file = workspace.path("outer/sites/lab/users.yaml");

    assert_eq!(nearest_project(&file, &roots), Some(workspace.path("outer/sites/lab")));
    assert_eq!(
        projects_containing(&file, &roots),
        vec![workspace.path("outer"), workspace.path("outer/sites/lab")]
    );
    assert_eq!(nearest_project(&workspace.path("elsewhere.yaml"), &roots), None);
}

#[test]
fn walking_up_finds_the_root_from_a_file_deep_inside() {
    let workspace = Workspace::new(
        "walk-up",
        &["config/_schema/nodes/_each.yaml", "config/nodes/cop01/_self.yaml", "loose/file.yaml"],
    );
    let root = Some(workspace.path("config"));

    assert_eq!(enclosing_project_root(&workspace.root.join("config/nodes/cop01/_self.yaml")), root);
    // A schema file is part of the project whose `_schema` holds it.
    assert_eq!(enclosing_project_root(&workspace.root.join("config/_schema/nodes/_each.yaml")), root);
    assert_eq!(enclosing_project_root(&workspace.root.join("loose/file.yaml")), None);
}

#[test]
fn a_root_contains_only_what_is_below_it() {
    assert!(is_within("/w/config", "/w/config"));
    assert!(is_within("/w/config/a.yaml", "/w/config"));
    assert!(is_within("/w/config/a.yaml", "/w/config/"));
    assert!(!is_within("/w/config-old/a.yaml", "/w/config"));
    assert!(!is_within("/w", "/w/config"));
}

#[test]
fn only_marker_names_can_change_the_roots() {
    assert!(can_change_project_roots("/w/p/.coal"));
    assert!(can_change_project_roots("/w/p/_structure.yaml"));
    assert!(can_change_project_roots("/w/p/_schema"));
    assert!(can_change_project_roots("/w/p/_schema/nodes.yaml"));
    assert!(can_change_project_roots("C:\\w\\p\\_schema"));
    assert!(!can_change_project_roots("/w/p/nodes/_self.yaml"));
    assert!(!can_change_project_roots("/w/p/my_schema.yaml"));
}

fn diagnostic(message: &str) -> Diagnostic {
    Diagnostic {
        range: Range::new(Position::new(0, 0), Position::new(0, 50)),
        message: message.to_string(),
        ..Default::default()
    }
}

fn url(path: &str) -> Uri {
    format!("file://{}", path).parse::<Uri>().unwrap()
}

#[test]
fn a_file_two_projects_read_shows_what_both_report() {
    let mut published = PublishedDiagnostics::default();
    let shared = url("/w/outer/lab/users.yaml");

    published.replace("/w/outer", HashMap::from([(shared.clone(), vec![diagnostic("from outer")])]));
    let sent = published.replace(
        "/w/outer/lab",
        HashMap::from([(shared.clone(), vec![diagnostic("from lab"), diagnostic("from outer")])]),
    );

    let messages: Vec<&str> = sent[0].1.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(messages, vec!["from outer", "from lab"]);

    // The inner project fixing its finding leaves the outer one's in place.
    let sent = published.replace("/w/outer/lab", HashMap::new());
    assert_eq!(sent.len(), 1);
    let messages: Vec<&str> = sent[0].1.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(messages, vec!["from outer"]);
}

#[test]
fn a_project_that_goes_away_has_its_files_cleared() {
    let mut published = PublishedDiagnostics::default();
    let file = url("/w/p/a.yaml");
    published.replace("/w/p", HashMap::from([(file.clone(), vec![diagnostic("wrong")])]));

    let sent = published.remove("/w/p");
    assert_eq!(sent, vec![(file, vec![])]);
    assert!(published.projects().is_empty());
}
