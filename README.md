# Coalescence (COAL)

Coalescence (COAL) is a powerful, Rust-based configuration builder and validation engine. It is designed to take modular, highly dynamic configuration definitions and compile them into a single, fully resolved, and strictly validated configuration tree. It uses yaml based schemas that include data from the tree to validate the entire tree. With built in jinja2 expressions data within the tree can easily be interlinked. This allows for reusable data so everything only needs to be defined once.

**Why YAML?** Everything in COAL is driven by YAML. YAML was chosen because it is incredibly clear, human-readable, and easy to use. It allows developers and operators to define complex hierarchical structures, references, and logic without the visual clutter of traditional programming languages or strictly bracketed data formats like JSON.

---

## Command Line Interface

COAL provides a straightforward CLI for compiling and inspecting your configurations:

- **`coal`** — Compiles the `main.yaml` file in the current directory, resolves all directives and Jinja templates, and prints the final flattened YAML to standard output.
- **`coal --validate`** (or `-v`) — Compiles the tree and runs it against the schema definitions in the `schemas/` directory, entering at `schemas/main.schema`. If errors are found, it outputs a detailed diagnostic report to `stderr` and exits with a non-zero code.
- **`coal --analyze <path>`** (or `-a`) — Dumps the internal Abstract Syntax Tree (AST) properties of a specific field (e.g., `coal -a processes.cms`). Displays the key, value, resolved schema type, origin file, allowed values, and physical line/column span.

---

## The Tree Building Pipeline

COAL processes configurations in a strict, multi-phase pipeline to ensure dependencies, structural morphing, and templates are resolved in the correct order.

### 1. Expansion (Includes & AST Directives)

COAL parses the `main.yaml` file into a `CoalTree` root node and performs a depth-first traversal to expand the file system structure by processing `$include` directives, parsing those external files, and splicing them directly into the current tree. Once all files are loaded into memory, it performs another pass to evaluate AST structural directives (like `$inherit` and `$override`) to dynamically duplicate, merge, and generate nodes.

### 2. Processing (Synchronization and Jinja)

With the full structure finalized, COAL synchronizes the raw YAML representations with the deeply nested `CoalTree` nodes. It converts nodes with numeric keys into YAML lists and others into standard mapping objects, then resolves all Jinja templates (`{{ ... }}`). Because the structure is locked at this point, absolute paths can be reliably mapped to their target nodes and values are safely substituted.

### 3. Validation

If validation is enabled, COAL loads all `.schema` or `.yaml` files from the `schemas/` directory. It takes the main schema and recursively applies it to the root of the generated tree. This step enforces types, injects default values, evaluates conditional logic, and marks invalid fields. The diagnostics engine then scans the tree and collects any validation errors.

### 4. Final Cleanup

Before outputting the final YAML, COAL traverses the tree one last time and scrubs any keys that start with `$`. This ensures all internal system directives are hidden from the final compiled configuration file.

---

## System Directives (`$` Functions)

COAL reserves keys starting with `$` for system operations, allowing you to manipulate the tree dynamically.

### File Inclusion Directives

These directives pull external YAML files into the current node.

| Directive | Description |
|---|---|
| `$include` | Parses a specific file and merges its contents into the current node. |
| `$include_directory` | Reads all `.yaml` files in a directory. Each file becomes a child object keyed by the file's base name. |
| `$include_subdirectories` | Looks at all subdirectories in the target path. If a subdirectory contains a `.yaml` file matching the directory's name, it is included as a child object under that name. |

> **Note:** All of the above have `_optional` variants (e.g. `$include_optional`) which silently ignore missing files or directories instead of throwing a fatal error.

**Example:**

```yaml
network_config:
  $include_optional: "./environments/production/network.yaml"
```

### AST Structural Directives

These directives clone and manipulate the tree structure dynamically.

| Directive | Description |
|---|---|
| `$inherit` | Clones the target node's children into the current node. If the current node already has a child with the same key, it deep-merges them, preserving the existing child's deeper leaves. |
| `$override` | Similar to `$inherit`, but prioritizes the target. If there are key conflicts, the target's structure completely overwrites the current node's structure. |
| `$each` | Iterates over its own template children and clones them into every sibling of the `$each` directive. Useful for applying a standard set of properties to a list of dynamically declared items. |

**Example:**

```yaml
node_types:
  standard_server:
    port: 8080
    logging: true

nodes:
  web_01:
    $inherit: "ROOT.node_types.standard_server"
    ip: "192.168.1.10"  # Inherits port and logging, adds ip
```

### Context Variables

During parsing, COAL automatically injects these string replacements:

| Variable | Description |
|---|---|
| `$filename` | Replaced with the base name (stem) of the file the node originated from. |
| `$parent` | Replaced with the key of the node's immediate parent. |

**Example:**

```yaml
processes:
  cms_service:
    process_name: "$parent"  # Evaluates to "cms_service"
```

---

## Schema Engine & Validation

The schema engine strictly defines what the compiled YAML is allowed to look like. Schemas are defined as YAML files inside the `schemas/` directory, with the entry point named `main.schema` or `main.yaml`.

### Basic Types

You can define a field's required type using simple strings: `string`, `integer`, `float`, `number`, `bool`, `list`, `object`, or `any`.

**Example:**

```yaml
server_name: string
port: integer
is_active: bool
```
### Jinja2 Integration

COAL seamlessly bridges your data and your validation rules. You can use Jinja2 expressions (`{{ ... }}`) directly inside your schemas to use your own configuration data as validation input.

For example, to ensure a node is only assigned to a cluster that actually exists elsewhere in your configuration:

```yaml
clusters: "[string]"
assigned_cluster: "{{ ROOT.clusters }}"
```

### Advanced Schema Directives

Schemas use their own `$` directives to enforce complex logic.

| Directive | Description |
|---|---|
| `$schema: "schema_name"` | Acts as a mixin, deep-merging another schema file from the `schemas/` directory into the current location. |
| `$declare` / `$use` | Defines reusable sub-schemas within the same file. Define a block under `$declare: { my_type: ... }` and apply it elsewhere with `$use: my_type`. |
| `$any` | Permits the object to contain undeclared fields without throwing an error. |
| `$optional` | A mapping of fields that are permitted but not required. |

### Detailed Field Constraints (`$advanced_fields`)

Allows you to attach metadata and strict constraints to a specific field:

```yaml
$advanced_fields:
  environment:
    type: string
    default_value: "development"
    allowed_values: ["development", "staging", "production"]
```

### Conditional Logic (`$conditional_fields`)

Allows schemas to mutate based on the data present in the compiled tree. Uses an `$if` block (which supports `$and` and `$or`) to check sibling values. If the condition is met, the `$then` block applies a sub-schema.

```yaml
$conditional_fields:
  - $if:
      type: "database"
    $then:
      db_port: integer
      db_password: string
```
