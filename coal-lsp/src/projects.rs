//! Which directories are COAL projects, and which project a file belongs to.
//!
//! An editor workspace is not necessarily a COAL project: the project can sit
//! in a subfolder, there can be several side by side, and a workspace can hold
//! YAML that has nothing to do with COAL at all. So the server does not take
//! the workspace folder as the root. It looks for the directories that *are*
//! roots, and validates each of those on its own.
//!
//! A directory is a project root when it holds
//!
//! * a `.coal` file — the explicit marker, for a project that has neither of
//!   the two below; or
//! * `_structure.yaml` (or `.yml`) or a `_schema/` folder. Both are only
//!   allowed in the root directory of a configuration (the loader rejects them
//!   anywhere else), so either one says where the root is without anything
//!   added to the project.
//!
//! A file belongs to the nearest directory above it that is a root.

use coal::virtual_file_system::normalize_path;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use tower_lsp_server::ls_types::{Diagnostic, Uri};

/// The file that marks a directory as a project root explicitly.
pub const PROJECT_MARKER: &str = ".coal";

const STRUCTURE_FILES: [&str; 2] = ["_structure.yaml", "_structure.yml"];
const SCHEMA_DIRECTORY: &str = "_schema";

/// Folders never worth looking inside for a project: build output and
/// package caches can be enormous, and hold copies of projects rather than
/// projects.
const SKIPPED_DIRECTORIES: [&str; 2] = ["node_modules", "target"];

/// How deep below a workspace folder a project is looked for.
const MAX_SEARCH_DEPTH: usize = 16;

/// Whether `directory` is the root of a COAL project.
pub fn is_project_root(directory: &Path) -> bool {
    directory.join(PROJECT_MARKER).exists()
        || STRUCTURE_FILES.iter().any(|name| directory.join(name).is_file())
        || directory.join(SCHEMA_DIRECTORY).is_dir()
}

/// Every project root at or below `folder`, as normalized paths.
///
/// Folders whose name starts with `_` are not searched: COAL never loads them
/// as data, and the reserved ones (`_schema`, `_definitions`, `_each`, ...)
/// are parts of the project above them. Hidden folders (`.git`, `.vscode`)
/// and build output are skipped too. The search continues below a root,
/// because a folder inside one project can be marked as a project of its own.
pub fn discover_project_roots(folder: &Path) -> Vec<String> {
    let mut roots = Vec::new();
    search(folder, 0, &mut roots);
    roots.sort();
    roots.dedup();
    roots
}

fn search(directory: &Path, depth: usize, roots: &mut Vec<String>) {
    if is_project_root(directory) {
        roots.push(normalize_path(&directory.to_string_lossy()));
    }
    if depth >= MAX_SEARCH_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else { return };
    for entry in entries.flatten() {
        // Not following links keeps the search finite and stops one project
        // being found twice under two names.
        let Ok(file_type) = entry.file_type() else { continue };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('_') || name.starts_with('.') || SKIPPED_DIRECTORIES.contains(&name.as_ref()) {
            continue;
        }
        search(&entry.path(), depth + 1, roots);
    }
}

/// The root of the project `path` belongs to, found by walking up from it on
/// disk. Used for a file opened from outside every known project, e.g. when
/// the workspace folder is a subfolder of the project.
pub fn enclosing_project_root(path: &Path) -> Option<String> {
    let start = match path.is_dir() {
        true => Some(path),
        false => path.parent(),
    };
    start?
        .ancestors()
        .find(|directory| is_project_root(directory))
        .map(|root| normalize_path(&root.to_string_lossy()))
}

/// Whether the normalized `path` is `root` itself or lies below it.
pub fn is_within(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    let Some(head) = path.get(..root.len()) else { return false };
    let same_prefix = match cfg!(windows) {
        true => head.eq_ignore_ascii_case(root),
        false => head == root,
    };
    same_prefix && matches!(path.as_bytes().get(root.len()), None | Some(b'/'))
}

/// The nearest of `roots` that `path` lies in: the one its build answers
/// hover and completion from.
pub fn nearest_project<'a, I>(path: &str, roots: I) -> Option<String>
where
    I: IntoIterator<Item = &'a String>,
{
    roots
        .into_iter()
        .filter(|root| is_within(path, root))
        .max_by_key(|root| root.len())
        .cloned()
}

/// Every one of `roots` that `path` lies in. A project marked inside another
/// is also read by the outer one's build, so an edit concerns both.
pub fn projects_containing<'a, I>(path: &str, roots: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a String>,
{
    roots.into_iter().filter(|root| is_within(path, root)).cloned().collect()
}

/// Whether creating or deleting `path` can make a directory start or stop
/// being a project root.
pub fn can_change_project_roots(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let mut segments = normalized.split('/').filter(|segment| !segment.is_empty());
    let name = normalized.rsplit('/').next().unwrap_or_default();
    name == PROJECT_MARKER
        || STRUCTURE_FILES.contains(&name)
        || segments.any(|segment| segment == SCHEMA_DIRECTORY)
}

/// What each project last reported, and what every file should show.
///
/// A file inside a project marked within another one is read by both builds,
/// and an editor keeps only the last list it was sent for a file. Publishing
/// each project's findings as they are would let one project wipe out the
/// other's, so a file is always sent everything that every project reports
/// in it.
#[derive(Default)]
pub struct PublishedDiagnostics {
    by_project: BTreeMap<String, HashMap<Uri, Vec<Diagnostic>>>,
}

impl PublishedDiagnostics {
    /// The projects with findings on record.
    pub fn projects(&self) -> BTreeSet<String> {
        self.by_project.keys().cloned().collect()
    }

    /// Records what `project` reports now, and returns each file whose
    /// diagnostics must be sent again, with everything it should show. A file
    /// the project reported before and does not now is included, so that it
    /// is cleared.
    pub fn replace(
        &mut self,
        project: &str,
        diagnostics: HashMap<Uri, Vec<Diagnostic>>,
    ) -> Vec<(Uri, Vec<Diagnostic>)> {
        let previous = self.by_project.insert(project.to_string(), diagnostics).unwrap_or_default();
        let mut touched: BTreeSet<Uri> = previous.into_keys().collect();
        touched.extend(self.by_project[project].keys().cloned());
        self.merged(touched)
    }

    /// Forgets `project`, e.g. because its root is gone, and returns the
    /// files to send again.
    pub fn remove(&mut self, project: &str) -> Vec<(Uri, Vec<Diagnostic>)> {
        let previous = self.by_project.remove(project).unwrap_or_default();
        self.merged(previous.into_keys().collect())
    }

    fn merged(&self, files: BTreeSet<Uri>) -> Vec<(Uri, Vec<Diagnostic>)> {
        files
            .into_iter()
            .map(|url| {
                let mut combined: Vec<Diagnostic> = Vec::new();
                for reported in self.by_project.values().filter_map(|project| project.get(&url)) {
                    for diagnostic in reported {
                        let seen = combined
                            .iter()
                            .any(|kept| kept.message == diagnostic.message && kept.range == diagnostic.range);
                        if !seen {
                            combined.push(diagnostic.clone());
                        }
                    }
                }
                (url, combined)
            })
            .collect()
    }
}
