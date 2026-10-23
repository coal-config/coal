//! Building the configuration tree out of a directory.
//!
//! The directory *is* the configuration. Every `.yaml` / `.yml` file becomes a
//! field named after the file, every subdirectory becomes a level of nesting,
//! and that is the whole of the structure:
//!
//! ```text
//! node_types.yaml                 -> node_types
//! environments/production.yaml    -> environments.production
//! environments/staging/_self.yaml -> environments.staging
//! ```
//!
//! # Reserved names
//!
//! A file or folder whose name starts with `_` is never loaded as data. Five
//! names are reserved and mean something; anything else starting with `_` is
//! ignored entirely, which is what makes `_` the way to park a file the build
//! should not read.
//!
//! | Name | Meaning |
//! | --- | --- |
//! | `_self` | fields of the directory itself, rather than of a field inside it |
//! | `_each` | loaded like a config folder, then merged into every sibling field |
//! | `_optional` | in a schema, the fields it permits without requiring |
//! | `_definitions` | loaded like a config folder, into the namespace `$include` reads in that directory and below |
//! | `_schema/` | the schema definitions (root directory only) |
//! | `_structure.yaml` | the selection and layering rules (root directory only) |
//!
//! The first three may be written either way round: `_each.yaml` says the same
//! thing as `_each/`, and a small template is far easier to read as one file
//! than as a folder holding one. Where both are present they are merged, and
//! the folder wins — the same rule as `app.yaml` beside `app/`.
//!
//! # File roots
//!
//! The node a file's contents land on is marked as a *file root*. A template
//! written in that file resolves a bare name there first, and only then at the
//! configuration root — so a file can talk about its own fields without
//! knowing where in the tree it was mounted.

use crate::coal_tree::{CoalTree, Span};
use crate::definition_scopes::DefinitionScopes;
use crate::tree_operations::{add_or_replace_child_node_in_parent, build_tree_from_marked_node};
use crate::type_aliases::{TreeNodeReference, VirtualFileSystemReference, new_tree_node_reference};
use crate::virtual_file_system::normalize_path;
use crate::yaml_values::{convert_marked_yaml_node_to_value, extract_string_list_from_value};
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// Fields of the directory itself. A file or a folder.
pub const SELF_NAME: &str = "_self";
/// A template merged into every sibling field of the directory holding it.
/// A file or a folder.
pub const EACH_NAME: &str = "_each";
/// Content addressable by `$include` from anywhere in the configuration.
/// A file or a folder.
pub const DEFINITIONS_NAME: &str = "_definitions";
/// The schema definitions. Root directory only.
pub const SCHEMA_DIRECTORY_NAME: &str = "_schema";
/// Fields a schema permits without requiring. A file or a folder.
pub const OPTIONAL_NAME: &str = "_optional";
/// The selection and layering rules. Root directory only.
pub const STRUCTURE_FILE_STEM: &str = "_structure";

/// The key an `_each` template is parked under while the tree is being built.
///
/// It is kept as an ordinary system directive so it is cloned along with the
/// node that carries it, stays invisible to templates and to the compiled
/// output, and is swept up by the same cleanup as every other `$` key.
pub const EACH_TEMPLATE_KEY: &str = "$each";

/// The key an `_optional` block is parked under. It is the directory spelling
/// of the `$optional` a schema can also write inline, and it means exactly the
/// same thing.
pub const OPTIONAL_BLOCK_KEY: &str = "$optional";

/// The key a list of fields to leave out is written under.
pub const IGNORE_KEY: &str = "$ignore";

/// Whether this node is an `_each` template rather than configuration.
///
/// A template is a stencil: nothing inside it is evaluated where it sits,
/// because `parent()` there is the folder that declared it rather than any of
/// the fields it describes. Every phase that walks the tree steps over it, and
/// the copies it makes are evaluated instead.
pub fn is_each_template(node: &TreeNodeReference) -> bool {
    node.borrow().key.as_deref() == Some(EACH_TEMPLATE_KEY)
}

/// How deep the loader will descend. Deep enough for any real configuration,
/// shallow enough that a directory linked into itself stops.
const MAX_DIRECTORY_DEPTH: usize = 64;

/// Everything a directory turned out to hold.
pub struct LoadedConfiguration {
    /// The configuration itself.
    pub root_node: TreeNodeReference,
    /// The contents of every `_definitions` folder, by the directory holding it.
    pub definition_scopes: DefinitionScopes,
    /// The parsed `_structure.yaml`, if the root directory had one.
    pub structure_document: Option<Value>,
    /// The path of `_schema`, if the root directory had one.
    pub schema_directory: Option<String>,
}

/// Reads a configuration directory into a tree.
pub fn load_configuration_directory(
    directory_path: &str,
    virtual_file_system: VirtualFileSystemReference,
) -> Result<LoadedConfiguration, Box<dyn Error>> {
    let normalized_directory = normalize_path(directory_path);
    if !Path::new(&normalized_directory).is_dir() {
        return Err(format!(
            "'{}' is not a directory. COAL builds a configuration out of the directory it is \
             pointed at, so that directory has to exist.",
            directory_path
        )
        .into());
    }

    let root_node = new_tree_node_reference(CoalTree {
        key: Some("ROOT".to_string()),
        origin: Some(normalized_directory.clone()),
        span: Some(Span { line: 1, col: 1 }),
        // The configuration root is a file root of last resort, so the walk
        // that looks for the nearest one always ends somewhere.
        file_root: true,
        ..Default::default()
    });

    let mut loaded = LoadedConfiguration {
        root_node: Rc::clone(&root_node),
        definition_scopes: DefinitionScopes::new(&normalized_directory),
        structure_document: None,
        schema_directory: None,
    };

    load_directory_into_node(
        &normalized_directory,
        root_node,
        &mut loaded,
        virtual_file_system,
        0,
        &[],
        true,
    )?;

    Ok(loaded)
}

/// What one entry of a directory turned out to be.
enum DirectoryEntry {
    /// A `.yaml`/`.yml` file that becomes a field named after it.
    ConfigurationFile { field_name: String, path: PathBuf },
    /// A subdirectory that becomes a level of nesting.
    ConfigurationDirectory { field_name: String, path: PathBuf },
    /// `_self`: fields of the enclosing directory itself.
    SelfEntry { path: PathBuf, is_directory: bool },
    EachEntry { path: PathBuf, is_directory: bool },
    OptionalEntry { path: PathBuf, is_directory: bool },
    DefinitionsEntry { path: PathBuf, is_directory: bool },
    SchemaDirectory { path: PathBuf },
    StructureFile { path: PathBuf },
    /// Anything else starting with `_`, or a file COAL does not read.
    Ignored,
}

/// Reads one directory onto one node.
///
/// `inherited_ignore_paths` are the names an enclosing level said to leave
/// out, already stripped of the part that named the way here.
/// `filters_by_ignore` is false for the reserved blocks — an `_each` template
/// naming `scratch` is talking about the fields it will be copied onto, not
/// about a file sitting beside it.
fn load_directory_into_node(
    directory_path: &str,
    node: TreeNodeReference,
    loaded: &mut LoadedConfiguration,
    virtual_file_system: VirtualFileSystemReference,
    depth: usize,
    inherited_ignore_paths: &[String],
    filters_by_ignore: bool,
) -> Result<(), Box<dyn Error>> {
    if depth >= MAX_DIRECTORY_DEPTH {
        return Err(format!(
            "Directory nesting exceeded {} at '{}'. Check for a directory linked into itself.",
            MAX_DIRECTORY_DEPTH, directory_path
        )
        .into());
    }

    // A field that comes from a folder has somewhere it lives even though no
    // file declares it. Saying so is what keeps a finding about that field
    // from having to report "Unknown Origin". The `_self.yaml` read just below
    // replaces this with a real file and a real line, and a same-named `.yaml`
    // beside the folder has already done so, which is why this only fills a
    // gap rather than overwriting.
    {
        let mut node_borrowed = node.borrow_mut();
        if node_borrowed.origin.is_none() {
            node_borrowed.origin = Some(normalize_path(directory_path));
        }
    }

    let entries = classify_directory_entries(directory_path, depth)?;

    // `_self.yaml` first, then the files, then the directories. A field
    // declared in more than one of those is merged rather than refused, and
    // the deeper declaration is the one that wins: a directory says more about
    // a field than the file beside it does.
    // A reserved name written as a file is read first and the folder of the
    // same name second, so the two merge with the folder winning — exactly as
    // `app.yaml` and `app/` do.
    for wants_directory in [false, true] {
        for entry in &entries {
            let DirectoryEntry::SelfEntry { path, is_directory } = entry else {
                continue;
            };
            if *is_directory != wants_directory {
                continue;
            }

            if *is_directory {
                load_directory_into_node(
                    &normalize_path(path.to_str().unwrap_or_default()),
                    Rc::clone(&node),
                    loaded,
                    Arc::clone(&virtual_file_system),
                    depth + 1,
                    &[],
                    false,
                )?;
            } else {
                let content = read_file(&virtual_file_system, path)?;
                merge_document_into_node(&content, path, Rc::clone(&node))?;
            }
        }
    }

    // The reserved blocks are read before the level's own entries, so that a
    // template's `$ignore` — which belongs to the fields it will be copied
    // onto — is known before those fields are read.
    for wants_directory in [false, true] {
        for entry in &entries {
            match entry {
                DirectoryEntry::EachEntry { path, is_directory }
                    if *is_directory == wants_directory =>
                {
                    load_directive_block(
                        path,
                        *is_directory,
                        &node,
                        EACH_TEMPLATE_KEY,
                        loaded,
                        Arc::clone(&virtual_file_system),
                        depth,
                    )?;
                }
                DirectoryEntry::OptionalEntry { path, is_directory }
                    if *is_directory == wants_directory =>
                {
                    load_directive_block(
                        path,
                        *is_directory,
                        &node,
                        OPTIONAL_BLOCK_KEY,
                        loaded,
                        Arc::clone(&virtual_file_system),
                        depth,
                    )?;
                }
                DirectoryEntry::DefinitionsEntry { path, is_directory }
                    if *is_directory == wants_directory =>
                {
                    load_definitions_namespace(
                        path,
                        *is_directory,
                        loaded,
                        Arc::clone(&virtual_file_system),
                        depth,
                    )?;
                }
                _ => {}
            }
        }
    }

    for entry in &entries {
        match entry {
            DirectoryEntry::SchemaDirectory { path } => {
                loaded.schema_directory = Some(normalize_path(path.to_str().unwrap_or_default()));
            }
            DirectoryEntry::StructureFile { path } => {
                let content = read_file(&virtual_file_system, path)?;
                loaded.structure_document = Some(parse_structure_document(&content, path)?);
            }
            _ => {}
        }
    }

    // What this level has been told to leave out: whatever an enclosing level
    // said, plus whatever this level says for itself.
    let mut ignore_paths: Vec<String> = inherited_ignore_paths.to_vec();
    if filters_by_ignore {
        ignore_paths.extend(literal_ignore_entries(&node));
    }
    let template_ignore_paths = literal_ignore_entries_of_child(&node, EACH_TEMPLATE_KEY);

    for entry in &entries {
        if let DirectoryEntry::ConfigurationFile { field_name, path } = entry {
            if is_ignored(&ignore_paths, field_name) {
                continue;
            }
            let content = read_file(&virtual_file_system, path)?;
            let field_node = find_or_create_child(&node, field_name);
            merge_document_into_node(&content, path, field_node)?;
        }
    }

    for entry in &entries {
        if let DirectoryEntry::ConfigurationDirectory { field_name, path } = entry {
            // A folder that has been left out is never opened, so nothing
            // inside it is read, parsed, or has to be valid YAML.
            if is_ignored(&ignore_paths, field_name) {
                continue;
            }

            let field_node = find_or_create_child(&node, field_name);
            load_directory_into_node(
                &normalize_path(path.to_str().unwrap_or_default()),
                field_node,
                loaded,
                Arc::clone(&virtual_file_system),
                depth + 1,
                &ignore_paths_for_child(&ignore_paths, &template_ignore_paths, field_name),
                true,
            )?;
        }
    }

    Ok(())
}

/// The names an `$ignore` list on this node gives outright.
///
/// A name that reads a value is left to the pass that runs once the whole tree
/// is built. At this point most of the configuration does not exist yet, and
/// honouring the ones that happen to resolve would make what gets built depend
/// on the order the file system handed the entries back in.
fn literal_ignore_entries(node: &TreeNodeReference) -> Vec<String> {
    let ignore_node = node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key.as_deref() == Some(IGNORE_KEY))
        .cloned();

    let Some(ignore_node) = ignore_node else {
        return Vec::new();
    };
    let raw_value = ignore_node.borrow().value.clone();

    raw_value
        .as_ref()
        .map(extract_string_list_from_value)
        .unwrap_or_default()
        .into_iter()
        .filter(|name| !name.contains("{{") && !name.contains("{%"))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// The same, read off one of the node's `$` blocks.
fn literal_ignore_entries_of_child(node: &TreeNodeReference, block_key: &str) -> Vec<String> {
    let block = node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key.as_deref() == Some(block_key))
        .cloned();

    match block {
        Some(block) => literal_ignore_entries(&block),
        None => Vec::new(),
    }
}

/// Whether a name at this level has been left out. Only a name given whole
/// applies here; a dotted one is passed further down.
fn is_ignored(ignore_paths: &[String], field_name: &str) -> bool {
    ignore_paths.iter().any(|path| path == field_name)
}

/// What a child level has been told to leave out: the remainder of every
/// dotted name that pointed through it, plus everything the `_each` template
/// said — that template is copied onto the child, so its list names the
/// child's own fields.
fn ignore_paths_for_child(
    ignore_paths: &[String],
    template_ignore_paths: &[String],
    child_name: &str,
) -> Vec<String> {
    let prefix = format!("{}.", child_name);

    let mut child_paths: Vec<String> = ignore_paths
        .iter()
        .filter_map(|path| path.strip_prefix(&prefix).map(|remainder| remainder.to_string()))
        .collect();

    child_paths.extend(template_ignore_paths.iter().cloned());
    child_paths
}

/// Loads a reserved folder or file onto the directory's own node, under the
/// `$` key it is the directory spelling of.
///
/// `_each` becomes `$each`, the template merged into every sibling field.
/// `_optional` becomes `$optional`, the fields a schema permits without
/// requiring. In both cases the block is read exactly as a configuration
/// folder is, so it may itself hold `_self`, further reserved names, and
/// anything else this loader understands.
fn load_directive_block(
    path: &Path,
    is_directory: bool,
    node: &TreeNodeReference,
    block_key: &str,
    loaded: &mut LoadedConfiguration,
    virtual_file_system: VirtualFileSystemReference,
    depth: usize,
) -> Result<(), Box<dyn Error>> {
    let template_node = find_or_create_child(node, block_key);

    if !is_directory {
        let content = read_file(&virtual_file_system, path)?;
        return merge_document_into_node(&content, path, template_node);
    }

    load_directory_into_node(
        &normalize_path(path.to_str().unwrap_or_default()),
        template_node,
        loaded,
        virtual_file_system,
        depth + 1,
        &[],
        false,
    )
}

/// A `_definitions` folder contributes to the namespace of the directory that
/// holds it, which every include written in that directory or below it can
/// read — merged over the namespaces of the directories above (see
/// `definition_scopes`). An include names what it wants rather than which file
/// it lives in.
fn load_definitions_namespace(
    path: &Path,
    is_directory: bool,
    loaded: &mut LoadedConfiguration,
    virtual_file_system: VirtualFileSystemReference,
    depth: usize,
) -> Result<(), Box<dyn Error>> {
    let entry_path = normalize_path(path.to_str().unwrap_or_default());

    let staging_node = new_tree_node_reference(CoalTree {
        key: Some("DEFINITIONS".to_string()),
        origin: Some(entry_path.clone()),
        file_root: true,
        ..Default::default()
    });

    if is_directory {
        load_directory_into_node(
            &entry_path,
            Rc::clone(&staging_node),
            loaded,
            virtual_file_system,
            depth + 1,
            &[],
            false,
        )?;
    } else {
        // A single `_definitions.yaml` contributes its top-level keys as the names
        // `$include` reaches, which is what the folder of the same name does
        // one file at a time.
        let content = read_file(&virtual_file_system, path)?;
        merge_document_into_node(&content, path, Rc::clone(&staging_node))?;
    }

    let declaring_directory = path
        .parent()
        .map(|parent| parent.to_string_lossy().to_string())
        .unwrap_or_default();
    let own_namespace = loaded.definition_scopes.own_namespace_of(&declaring_directory);
    merge_into_definitions_namespace(staging_node, own_namespace, "")
}

/// Merges one `_definitions` entry into its directory's namespace. A directory
/// may hold both `_definitions.yaml` and `_definitions/`; objects on both sides
/// are merged, and anything else declared twice is a genuine clash between the
/// two and is reported rather than silently resolved. (A clash between
/// *different* directories is not one: the deeper directory overrides.)
fn merge_into_definitions_namespace(
    source_node: TreeNodeReference,
    target_node: TreeNodeReference,
    path_so_far: &str,
) -> Result<(), Box<dyn Error>> {
    let source_children = source_node.borrow().children.clone();

    for source_child in source_children {
        let child_key = source_child.borrow().key.clone().unwrap_or_default();
        let child_path = if path_so_far.is_empty() {
            child_key.clone()
        } else {
            format!("{}.{}", path_so_far, child_key)
        };

        let existing_child = target_node
            .borrow()
            .children
            .iter()
            .find(|child| child.borrow().key.as_deref() == Some(child_key.as_str()))
            .cloned();

        let Some(existing_child) = existing_child else {
            source_child.borrow_mut().parent = Some(Rc::downgrade(&target_node));
            target_node.borrow_mut().children.push(source_child);
            continue;
        };

        let both_are_objects =
            !existing_child.borrow().children.is_empty() && !source_child.borrow().children.is_empty();

        if !both_are_objects {
            let existing_origin = existing_child.borrow().origin.clone().unwrap_or_default();
            let incoming_origin = source_child.borrow().origin.clone().unwrap_or_default();
            return Err(format!(
                "Two _definitions entries of the same directory both provide '{}':\n  {}\n  {}\n\
                 Within one directory a name may only be declared once. A _definitions folder in a \
                 subdirectory may override it instead.",
                child_path, existing_origin, incoming_origin
            )
            .into());
        }

        merge_into_definitions_namespace(source_child, existing_child, &child_path)?;
    }

    Ok(())
}

// =====================================================================
// READING THE DIRECTORY
// =====================================================================

fn classify_directory_entries(
    directory_path: &str,
    depth: usize,
) -> Result<Vec<DirectoryEntry>, Box<dyn Error>> {
    let raw_entries = fs::read_dir(Path::new(directory_path))
        .map_err(|error| format!("Could not read directory '{}': {}", directory_path, error))?;

    // Sorted so a build is reproducible whatever order the file system hands
    // its entries back in.
    let mut paths: Vec<PathBuf> = Vec::new();
    for raw_entry in raw_entries {
        paths.push(raw_entry?.path());
    }
    paths.sort();

    let mut entries = Vec::new();
    // Field name -> the file that claimed it, so `x.yaml` beside `x.yml` is
    // reported instead of one of them silently winning.
    let mut claimed_field_names: BTreeMap<String, PathBuf> = BTreeMap::new();

    for path in paths {
        let entry = classify_entry(&path, depth)?;

        if let DirectoryEntry::ConfigurationFile { field_name, .. } = &entry
            && let Some(previous_path) = claimed_field_names.insert(field_name.clone(), path.clone())
        {
            return Err(format!(
                "Two files both define the field '{}':\n  {}\n  {}\nOnly one of .yaml and .yml \
                 may declare a field.",
                field_name,
                normalize_path(&previous_path.to_string_lossy()),
                normalize_path(&path.to_string_lossy())
            )
            .into());
        }

        entries.push(entry);
    }

    Ok(entries)
}

fn classify_entry(path: &Path, depth: usize) -> Result<DirectoryEntry, Box<dyn Error>> {
    let entry_name = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => name.to_string(),
        None => return Ok(DirectoryEntry::Ignored),
    };

    let is_directory = path.is_dir();

    // A dot-name belongs to something other than the configuration — `.git`,
    // `.vscode`, an editor's swap file — so the build never reads it and never
    // reports it. Unlike `_`, it means nothing to COAL at all: there are no
    // reserved dot-names to fall through to.
    if entry_name.starts_with('.') {
        return Ok(DirectoryEntry::Ignored);
    }

    if !entry_name.starts_with('_') {
        if is_directory {
            return Ok(DirectoryEntry::ConfigurationDirectory {
                field_name: entry_name,
                path: path.to_path_buf(),
            });
        }
        return Ok(match yaml_file_stem(path) {
            Some(field_name) => DirectoryEntry::ConfigurationFile {
                field_name,
                path: path.to_path_buf(),
            },
            None => DirectoryEntry::Ignored,
        });
    }

    // From here on the name is reserved-looking. The five names COAL knows
    // mean something; everything else starting with '_' is deliberately
    // invisible to the build.
    // `_self`, `_each` and `_definitions` mean the same thing whether they are a
    // file or a folder, so the name is what is matched on and the shape only
    // decides how it is read.
    let reserved_name = if is_directory {
        Some(entry_name.clone())
    } else {
        yaml_file_stem(path)
    };

    Ok(match reserved_name.as_deref() {
        Some(SELF_NAME) => DirectoryEntry::SelfEntry {
            path: path.to_path_buf(),
            is_directory,
        },
        Some(EACH_NAME) => DirectoryEntry::EachEntry {
            path: path.to_path_buf(),
            is_directory,
        },
        Some(OPTIONAL_NAME) => DirectoryEntry::OptionalEntry {
            path: path.to_path_buf(),
            is_directory,
        },
        Some(DEFINITIONS_NAME) => DirectoryEntry::DefinitionsEntry {
            path: path.to_path_buf(),
            is_directory,
        },
        Some(SCHEMA_DIRECTORY_NAME) if is_directory => {
            require_root_directory(SCHEMA_DIRECTORY_NAME, path, depth)?;
            DirectoryEntry::SchemaDirectory {
                path: path.to_path_buf(),
            }
        }
        Some(STRUCTURE_FILE_STEM) if !is_directory => {
            require_root_directory(STRUCTURE_FILE_STEM, path, depth)?;
            DirectoryEntry::StructureFile {
                path: path.to_path_buf(),
            }
        }
        _ => DirectoryEntry::Ignored,
    })
}

/// `_schema` and `_structure.yaml` describe the configuration as a whole, so
/// they only mean anything at the top. Finding one further down is a mistake
/// worth reporting: ignoring it would leave the schemas or the structure
/// quietly doing nothing.
fn require_root_directory(name: &str, path: &Path, depth: usize) -> Result<(), Box<dyn Error>> {
    if depth == 0 {
        return Ok(());
    }
    Err(format!(
        "'{}' only applies to the configuration as a whole, so it belongs in the root \
         directory. Found at '{}'.",
        name,
        normalize_path(&path.to_string_lossy())
    )
    .into())
}

fn yaml_file_stem(path: &Path) -> Option<String> {
    let extension = path.extension().and_then(|extension| extension.to_str())?;
    if !matches!(extension, "yaml" | "yml") {
        return None;
    }
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.to_string())
}

fn read_file(
    virtual_file_system: &VirtualFileSystemReference,
    path: &Path,
) -> Result<String, Box<dyn Error>> {
    virtual_file_system
        .lock()
        .unwrap()
        .get_file_content(&normalize_path(path.to_str().unwrap_or_default()))
}

// =====================================================================
// PARSING A DOCUMENT ONTO A NODE
// =====================================================================

/// Parses one YAML document and merges it onto the node that stands for it.
///
/// The node is marked as a file root, which is what lets a template written in
/// the file resolve a bare name against the file's own top level before
/// falling back to the configuration root.
pub fn merge_document_into_node(
    content: &str,
    path: &Path,
    node: TreeNodeReference,
) -> Result<(), Box<dyn Error>> {
    let file_path = normalize_path(path.to_str().unwrap_or_default());

    // A file usually holds a mapping, which is what the position-tracking
    // parser understands. One that holds a single value — `production` on its
    // own line, saying which environment to build — or a list is read by the
    // plain parser instead, and gives up its line numbers in exchange.
    let marked_document = match marked_yaml::parse_yaml(0, content) {
        Ok(marked_document) => Some(marked_document),
        Err(marked_error) => {
            let plain_document: Value = serde_yaml::from_str(content).map_err(|_| {
                format!("YAML Parse Error in '{}': {}", file_path, marked_error)
            })?;
            write_plain_document_onto_node(plain_document, &file_path, Rc::clone(&node))?;
            place_top_level_list_items(content, &node);
            return Ok(());
        }
    };
    let marked_document = marked_document.expect("the parse either produced a document or returned");
    let document_value = convert_marked_yaml_node_to_value(&marked_document);

    {
        let mut node_borrowed = node.borrow_mut();
        node_borrowed.origin = Some(file_path.clone());
        node_borrowed.file_root = true;
        if node_borrowed.span.is_none() {
            node_borrowed.span = Some(Span { line: 1, col: 1 });
        }
        node_borrowed.value = Some(document_value.clone());
    }

    // Parsed into a holding node first so the document's own entries merge
    // with anything already on the field rather than replacing it.
    let staging_node = new_tree_node_reference(CoalTree {
        key: node.borrow().key.clone(),
        origin: Some(file_path.clone()),
        ..Default::default()
    });
    build_tree_from_marked_node(&marked_document, Rc::clone(&staging_node), &file_path, content);

    let staged_children = staging_node.borrow().children.clone();
    for child in staged_children {
        child.borrow_mut().parent = Some(Rc::downgrade(&node));
        add_or_replace_child_node_in_parent(Rc::clone(&node), child, false);
    }

    Ok(())
}

/// Writes a document the position-tracking parser could not take: a single
/// value, or a list. Line numbers are not available for these, so anything
/// reported against them points at the file rather than at a line in it.
/// Gives the items of a file that holds a list the lines they are written on.
///
/// The position-tracking parser only takes a list at the top level when it is
/// asked for one, so a list file is read by the plain parser and every item
/// would otherwise claim line 1: a finding about the third item would sit on
/// the first line, and an editor asking what is written on the third line
/// would find nothing there. The values stay exactly what the plain parser
/// made of them; only the positions are taken from the tracked parse.
fn place_top_level_list_items(content: &str, node: &TreeNodeReference) {
    let Ok(marked_document) = marked_yaml::parse_yaml_with_options(
        0,
        content,
        marked_yaml::LoaderOptions::default().toplevel_sequence(),
    ) else {
        return;
    };

    let positions = new_tree_node_reference(CoalTree::default());
    build_tree_from_marked_node(&marked_document, Rc::clone(&positions), "", content);
    copy_positions(&positions, node);
}

/// Copies each position in `source` onto the node at the same key in `target`,
/// all the way down.
fn copy_positions(source: &TreeNodeReference, target: &TreeNodeReference) {
    let source_children = source.borrow().children.clone();
    let target_children = target.borrow().children.clone();

    for source_child in source_children {
        let key = source_child.borrow().key.clone();
        let Some(target_child) = target_children
            .iter()
            .find(|candidate| candidate.borrow().key == key)
        else {
            continue;
        };
        target_child.borrow_mut().span = source_child.borrow().span.clone();
        copy_positions(&source_child, target_child);
    }
}

fn write_plain_document_onto_node(
    document: Value,
    file_path: &str,
    node: TreeNodeReference,
) -> Result<(), Box<dyn Error>> {
    {
        let mut node_borrowed = node.borrow_mut();
        node_borrowed.origin = Some(file_path.to_string());
        node_borrowed.file_root = true;
        node_borrowed.span = Some(Span { line: 1, col: 1 });
        node_borrowed.value = Some(document.clone());
    }

    build_children_from_plain_value(&node, &document, file_path);
    Ok(())
}

/// Turns a list or mapping read without position information into tree nodes.
///
/// Unlike the structure a template produces, this is configuration somebody
/// wrote, so it is emphatically *not* marked as resolved: a template inside it
/// still has to be rendered.
fn build_children_from_plain_value(
    node: &TreeNodeReference,
    value: &Value,
    file_path: &str,
) {
    let entries: Vec<(String, Value)> = match value {
        Value::Sequence(sequence) => sequence
            .iter()
            .enumerate()
            .map(|(index, item)| (index.to_string(), item.clone()))
            .collect(),
        Value::Mapping(mapping) => mapping
            .iter()
            .filter_map(|(key, entry)| match key {
                Value::String(key) => Some((key.clone(), entry.clone())),
                Value::Number(key) => Some((key.to_string(), entry.clone())),
                Value::Bool(key) => Some((key.to_string(), entry.clone())),
                _ => None,
            })
            .collect(),
        _ => return,
    };

    for (key, entry_value) in entries {
        let child_node = new_tree_node_reference(CoalTree {
            key: Some(key),
            value: Some(entry_value.clone()),
            parent: Some(Rc::downgrade(node)),
            origin: Some(file_path.to_string()),
            span: Some(Span { line: 1, col: 1 }),
            ..Default::default()
        });
        build_children_from_plain_value(&child_node, &entry_value, file_path);
        node.borrow_mut().children.push(child_node);
    }
}

fn parse_structure_document(content: &str, path: &Path) -> Result<Value, Box<dyn Error>> {
    serde_yaml::from_str(content).map_err(|error| {
        Box::<dyn Error>::from(format!(
            "Invalid YAML in '{}': {}",
            normalize_path(path.to_str().unwrap_or_default()),
            error
        ))
    })
}

fn find_or_create_child(parent_node: &TreeNodeReference, key: &str) -> TreeNodeReference {
    let existing_child = parent_node
        .borrow()
        .children
        .iter()
        .find(|child| child.borrow().key.as_deref() == Some(key))
        .cloned();

    if let Some(existing_child) = existing_child {
        return existing_child;
    }

    let new_child = new_tree_node_reference(CoalTree {
        key: Some(key.to_string()),
        parent: Some(Rc::downgrade(parent_node)),
        span: Some(Span { line: 1, col: 1 }),
        ..Default::default()
    });
    parent_node.borrow_mut().children.push(Rc::clone(&new_child));
    new_child
}
