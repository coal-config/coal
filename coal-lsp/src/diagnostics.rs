//! What validation reports, as editor diagnostics.

use coal::tree_builder::{BuildScope, compile_configurations};
use coal::type_aliases::VirtualFileSystemReference;
use coal::validation_report::{GroupedFinding, group_findings};
use coal::virtual_file_system::normalize_path;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Uri};

/// Every diagnostic the directory has, keyed by the file it belongs in.
///
/// A `_structure.yaml` turns one directory into many configurations, and a
/// schema describes what comes *out* of a selection: the fields a node
/// inherits from its environment and its type only exist once the layers
/// have merged, and a part the structure drops is not in any of them. So
/// checking the directory as written would both invent problems that no
/// configuration has and miss the ones that every configuration has.
///
/// Every configuration is therefore built and checked, and the findings are
/// gathered before they are published: the same line read once per
/// selection is one squiggle that says how far it reaches, not one squiggle
/// per selection stacked on top of each other.
///
/// A build that cannot finish at all is reported once, in the file its
/// message names, or else in `fallback_file` — the file last edited, which is
/// the likeliest place the build was broken.
pub fn collect_workspace_diagnostics(
    workspace_root: &str,
    vfs: &VirtualFileSystemReference,
    fallback_file: Option<&str>,
) -> HashMap<Uri, Vec<Diagnostic>> {
    let mut by_file: Vec<(String, Diagnostic)> = Vec::new();

    match compile_configurations(
        workspace_root,
        Arc::clone(vfs),
        BuildScope::EveryCombination,
        true,
        &[],
        &[],
    ) {
        Ok(configurations) => {
            for finding in group_findings(&configurations) {
                let (line, col) = match &finding.span {
                    Some(span) => (
                        span.line.saturating_sub(1) as u32,
                        span.col.saturating_sub(1) as u32,
                    ),
                    None => (0, 0),
                };

                by_file.push((
                    resolve_origin(workspace_root, &finding.file_origin),
                    Diagnostic {
                        range: Range::new(Position::new(line, col), Position::new(line, col + 50)),
                        severity: Some(DiagnosticSeverity::ERROR),
                        code: Some(NumberOrString::String(finding.error_type.clone())),
                        source: Some("COAL".to_string()),
                        message: describe_finding(&finding),
                        ..Default::default()
                    },
                ));
            }
        }
        Err(error) => {
            let message = error.to_string();
            let file = file_named_in(&message, vfs)
                .or_else(|| fallback_file.map(normalize_path))
                .unwrap_or_else(|| default_fatal_file(workspace_root));

            by_file.push((
                file,
                Diagnostic {
                    range: Range::new(Position::new(0, 0), Position::new(0, 100)),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("COAL Parser".to_string()),
                    message: format!("Fatal Error: {}", message),
                    ..Default::default()
                },
            ));
        }
    }

    let mut diagnostics: HashMap<Uri, Vec<Diagnostic>> = HashMap::new();
    for (origin_path, diagnostic) in by_file {
        if let Some(url) = Uri::from_file_path(Path::new(&origin_path)) {
            let entry = diagnostics.entry(url).or_default();
            if !entry
                .iter()
                .any(|seen| seen.message == diagnostic.message && seen.range == diagnostic.range)
            {
                entry.push(diagnostic);
            }
        }
    }
    diagnostics
}

/// A finding's origin as an absolute path. Most origins already are; the few
/// that name a root file by itself (`_structure.yaml`) are read from the root.
fn resolve_origin(workspace_root: &str, origin: &str) -> String {
    match Path::new(origin).is_absolute() || origin.starts_with('/') {
        true => normalize_path(origin),
        false => normalize_path(&Path::new(workspace_root).join(origin).to_string_lossy()),
    }
}

/// The first file the file system knows of that a fatal error names.
fn file_named_in(message: &str, vfs: &VirtualFileSystemReference) -> Option<String> {
    let known_files: Vec<String> = vfs.lock().unwrap().files.keys().cloned().collect();
    let normalized_message = message.replace('\\', "/");
    known_files
        .into_iter()
        .filter(|file| normalized_message.contains(file.as_str()))
        .max_by_key(|file| file.len())
}

fn default_fatal_file(workspace_root: &str) -> String {
    for candidate in ["_structure.yaml", "_self.yaml"] {
        let path = Path::new(workspace_root).join(candidate);
        if path.is_file() {
            return normalize_path(&path.to_string_lossy());
        }
    }
    normalize_path(&Path::new(workspace_root).join("_self.yaml").to_string_lossy())
}

/// How a finding reads in an editor: what is wrong first, then how far it
/// reaches, since the second only matters once the first has been read.
fn describe_finding(finding: &GroupedFinding) -> String {
    let mut message = match finding.describe_fields() {
        fields if fields.is_empty() => finding.detail.clone(),
        fields => format!("{} -- {}", fields, finding.detail),
    };

    if !finding.elaboration.trim().is_empty() {
        message.push('\n');
        message.push_str(finding.elaboration.trim_end());
    }

    if let Some(reach) = finding.describe_reach() {
        message.push_str(&format!("\n({})", reach));
    }

    message
}
