use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Converts a path into an absolute, normalized form with forward slashes.
/// Resolving "." and ".." segments and standardizing separators keeps the
/// virtual file system keys identical across Windows and Linux.
pub fn normalize_path(path: &str) -> String {
    let absolute_path = make_path_absolute(Path::new(path));
    let resolved_path = resolve_relative_components(&absolute_path);
    resolved_path.to_string_lossy().replace('\\', "/")
}

fn make_path_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path)
    }
}

fn resolve_relative_components(path: &Path) -> PathBuf {
    let mut resolved_components = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved_components.pop();
            }
            Component::CurDir => {}
            _ => resolved_components.push(component),
        }
    }
    resolved_components.iter().collect()
}

/// An in-memory cache of file contents keyed by normalized path. Files are
/// read from disk on first access; callers (such as the language server) can
/// also overwrite entries directly to validate unsaved editor buffers.
pub struct VirtualFileSystem {
    pub files: HashMap<String, String>,
}

impl Default for VirtualFileSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualFileSystem {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
        }
    }

    pub fn get_file_content(&mut self, path: &str) -> Result<String, Box<dyn Error>> {
        let normalized_path = normalize_path(path);

        if let Some(content) = self.files.get(&normalized_path) {
            return Ok(content.clone());
        }

        match fs::read_to_string(&normalized_path) {
            Ok(content) => {
                self.files.insert(normalized_path, content.clone());
                Ok(content)
            }
            Err(error) => Err(Box::<dyn Error>::from(format!(
                "VFS Error: Could not read file '{}': {}",
                normalized_path, error
            ))),
        }
    }

    pub fn set_file_content(&mut self, path: &str, content: String) {
        let normalized_path = normalize_path(path);
        self.files.insert(normalized_path, content);
    }
}
