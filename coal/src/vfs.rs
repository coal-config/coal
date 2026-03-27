use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf, Component};

pub fn normalize_path(path: &str) -> String {
    let path_obj = Path::new(path);
    let abs_path = if path_obj.is_absolute() {
        path_obj.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path_obj)
    };

    // Resolve any "." or ".." in the path
    let mut components = Vec::new();
    for comp in abs_path.components() {
        match comp {
            Component::ParentDir => { components.pop(); }
            Component::CurDir => {}
            _ => components.push(comp),
        }
    }

    let clean_path = components.iter().collect::<PathBuf>();
    // Standardize slashes to avoid Windows/Linux desyncs in the VFS keys
    clean_path.to_string_lossy().replace("\\", "/")
}

pub struct VirtualFileSystem {
    pub files: HashMap<String, String>,
}

impl VirtualFileSystem {
    pub fn new() -> Self {
        Self { files: HashMap::new() }
    }

    pub fn get_file_content(&mut self, path: &str) -> Result<String, Box<dyn Error>> {
        let norm_path = normalize_path(path);

        if let Some(content) = self.files.get(&norm_path) {
            return Ok(content.clone());
        }

        match fs::read_to_string(&norm_path) {
            Ok(content) => {
                self.files.insert(norm_path.clone(), content.clone());
                Ok(content)
            }
            Err(e) => Err(Box::<dyn Error>::from(format!("VFS Error: Could not read file '{}': {}", norm_path, e))),
        }
    }

    pub fn set_file_content(&mut self, path: &str, content: String) {
        let norm_path = normalize_path(path);
        self.files.insert(norm_path, content);
    }
}