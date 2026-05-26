Here is the updated `README.md` with the separated code blocks, explicit file names preceding every example, and the new sections for `$include_directory` and `$include_subdirectories`.

---

# Coalescence (COAL)

Coalescence (COAL) is a powerful, Rust-based configuration builder and validation engine. It is designed to take modular, highly dynamic configuration definitions and compile them into a single, fully resolved, and strictly validated configuration tree. It uses yaml based schemas that include data from the tree to validate the entire tree. With built in jinja2 expressions, data within the tree can easily be reused. This allows for a define-it-once philosophy.

**Why YAML?** Everything in COAL is driven by YAML. YAML was chosen because it is incredibly clear, human-readable, and easy to use. It allows developers and operators to define complex hierarchical structures, references, and logic without the visual clutter of traditional programming languages.

---

## Command Line Interface

COAL provides a straightforward CLI for compiling and inspecting your configurations:

* **`coal`** — Compiles the `main.yaml` file in the current directory, resolves all directives and Jinja templates, and prints the final flattened YAML to standard output.
* **`coal --validate`** (or `-v`) — Compiles the tree and runs it against the schema definitions in the `schema/` directory. If errors are found, it outputs a detailed diagnostic report to `stderr` and exits with a non-zero code.
* **`coal --analyze <path>`** (or `-a`) — Dumps the internal Abstract Syntax Tree (AST) properties of a specific field (e.g., `coal -a processes.cms`). Displays the key, value, resolved schema type, origin file, allowed values, and physical line/column span.

---

## The Tree Building Pipeline

COAL processes configurations in a strict, multi-phase pipeline to ensure dependencies, structural morphing, and templates are resolved in the correct order.

### 1. Expansion (Includes & AST Directives)

COAL parses the `main.yaml` file into a `CoalTree` root node and performs a depth-first traversal to expand the file system structure by processing `$include` directives, parsing those external files, and splicing them directly into the current tree. Once all files are loaded into memory, it performs another pass to evaluate AST structural directives (like `$inherit`, `$select`, and `$map`) to dynamically duplicate, merge, and generate nodes.

### 2. Processing (Synchronization and Jinja)

With the full structure finalized, COAL synchronizes the raw YAML representations with the deeply nested `CoalTree` nodes. It converts nodes with numeric keys into YAML lists and others into standard mapping objects, then resolves all Jinja templates (`{{ ... }}`). Because the structure is locked at this point, absolute paths can be reliably mapped to their target nodes and values are safely substituted.

### 3. Validation

If validation is enabled, COAL loads all `.schema` or `.yaml` files from the `schema/` directory. It takes the main schema and recursively applies it to the root of the generated tree. This step enforces types, injects default values, evaluates conditional logic, and marks invalid fields. The diagnostics engine then scans the tree and collects any validation errors.

### 4. Final Cleanup

Before outputting the final YAML, COAL traverses the tree one last time and scrubs any keys that start with `$`. This ensures all internal system directives are hidden from the final compiled configuration file.

---

## System Directives (`$` Functions)

COAL reserves keys starting with `$` for system operations, allowing you to manipulate the tree dynamically.

### File Inclusion Directives

These directives pull external YAML files into the current node.

> **Note:** All of these have `_optional` variants (e.g. `$include_optional`) which silently ignore missing files or directories instead of throwing a fatal error.

#### `$include`

Parses a specific file and merges its contents into the current node.

**`main.yaml`**

```yaml
network:
  $include: "./network.yaml"

```

**`network.yaml`**

```yaml
port: 8080

```

**`Compiled Output`**

```yaml
network:
  port: 8080

```

#### `$include_directory`

Reads all `.yaml` files within the target directory. Each file is injected as a child object where the key is the file's base name (without the `.yaml` extension).

**`main.yaml`**

```yaml
environments:
  $include_directory: "./envs"

```

**`envs/prod.yaml`**

```yaml
url: "prod.local"

```

**`envs/dev.yaml`**

```yaml
url: "dev.local"

```

**`Compiled Output`**

```yaml
environments:
  prod:
    url: "prod.local"
  dev:
    url: "dev.local"

```

#### `$include_subdirectories`

Looks at all subdirectories within the target path. If a subdirectory contains a `.yaml` file matching the directory's exact name, it includes it as a child object under that directory's name.

**`main.yaml`**

```yaml
services:
  $include_subdirectories: "./modules"

```

**`modules/auth/auth.yaml`**

```yaml
port: 5000

```

**`modules/api/api.yaml`**

```yaml
port: 8000

```

**`Compiled Output`**

```yaml
services:
  auth:
    port: 5000
  api:
    port: 8000

```

---

### AST Structural Directives

These directives clone and manipulate the tree structure dynamically.

#### `$inherit`

Clones the target node's children into the current node. If the current node already has a child with the same key, it deep-merges them, preserving the local values over the inherited ones.

**`main.yaml`**

```yaml
base:
  port: 80
  logging: true
app:
  $inherit: "ROOT.base"
  port: 8080

```

**`Compiled Output`**

```yaml
base:
  port: 80
  logging: true
app: 
  port: 8080 # Local port wins
  logging: true  

```

#### `$override`

Similar to `$inherit`, but prioritizes the target. If there are key conflicts, the target's structure completely overwrites the local node's structure.

**`main.yaml`**

```yaml
base:
  port: 80
app:
  $override: "ROOT.base"
  port: 8080
  logging: true

```

**`Compiled Output`**

```yaml
base:
  port: 80
app: 
  port: 80 # Base port wins
  logging: true  

```

#### `$each`

Iterates over its own template children and clones them into every sibling of the `$each` directive. Useful for applying a standard set of properties to a list of dynamically declared items.

**`main.yaml`**

```yaml
servers:
  $each:
    status: "active"
  srv1: 
    ip: "1.1.1.1"
  srv2: 
    ip: "2.2.2.2"

```

**`Compiled Output`**

```yaml
servers:
  srv1: 
    ip: "1.1.1.1"
    status: "active" 
  srv2: 
    ip: "2.2.2.2"
    status: "active"

```

#### `$select` and `$keys`

Transforms a list into an object by looking up specific keys in a target mapping and cloning their values. If a key is not found, an empty field is created and flagged by the schema validator.

**`main.yaml`**

```yaml
available_roles:
  admin: 
    permissions: "all" 
  viewer: 
    permissions: "read"

my_roles:
  $select: "ROOT.available_roles"
  $keys:
    - admin

```

**`Compiled Output`**

```yaml
available_roles:
  admin: 
    permissions: "all" 
  viewer: 
    permissions: "read"
my_roles:
  admin: 
    permissions: "all"

```

---

### Context Variables

During parsing, COAL automatically injects these string replacements:

| Variable | Description |
| --- | --- |
| `$filename` | Replaced with the base name (stem) of the file the node originated from. |
| `$parent` | Replaced with the key of the node's immediate parent. |
| `$id` | Generates a sequentially incrementing integer. |

**`main.yaml`**

```yaml
processes:
  cms_service:
    process_name: "$parent"
    db_id:
      $id: "db_counters"
  auth_service:
    process_name: "$parent"
    db_id:
      $id: "db_counters"

```

**`Compiled Output`**

```yaml
processes:
  cms_service:
    process_name: "cms_service"
    db_id: 1
  auth_service:
    process_name: "auth_service"
    db_id: 2

```

---

## Schema Engine & Validation

The schema engine strictly defines what the compiled YAML is allowed to look like. Schemas are defined as YAML files inside the `schema/` directory, with the entry point named `main.schema`.

### Basic Types & Lists (`[]`)

You can define a field's required type using simple strings: `string`, `integer`, `float`, `number`, `bool`, `list`, `object`, or `any`. To enforce that a list only contains a specific type, wrap the type in brackets (`[]`).

**`schema/main.schema`**

```yaml
server_name: string
ports: [integer]  # Must be a list of integers
tags: [string]    # Must be a list of strings

```

**`main.yaml`**

```yaml
server_name: "prod-01"
ports: [80, 443]
tags: ["web", "frontend"]

```

---

### Advanced Schema Directives

#### `$schema`

Acts as a mixin, deep-merging another schema file from the `schema/` directory into the current location.

**`schema/main.schema`**

```yaml
my_server:
  $schema: "server_schema"  # Merges schema/server_schema.yaml

```

#### `$declare` / `$use`

Defines reusable sub-schemas within the same file.

**`schema/main.schema`**

```yaml
$declare:
  ip_config:
    address: string
    gateway: string

node_1:
  $use: "ip_config"

```

#### `$any`

Permits the object to contain undeclared fields without throwing an error.

**`schema/main.schema`**

```yaml
metadata:
  $any: string  # Allows any key, as long as the value is a string

```

**`main.yaml`**

```yaml
metadata:
  custom_tag: "v1.2"
  owner: "admin"

```

#### `$optional`

A mapping of fields that are permitted but not strictly required. If they are missing, the diagnostics engine will not flag them as errors.

**`schema/main.schema`**

```yaml
type: object
name: string
$optional:
  description: string

```

**`main.yaml`**

```yaml
name: "Server 1"
# description is omitted, but validation still passes!

```

---

### Advanced one-lines with Jinja Integration

Allows you to use Jinja2 expressions (`{{ ... }}`) directly inside schemas to use your configuration data as validation input.

**`schema/main.schema`**

```yaml
assigned_cluster: "{{ ROOT.clusters }}" # allowed_value for assigned_clusters are the keys of the children of clusters in the configuration tree

```

---

### Detailed Constraints (`$advanced_fields`) & Jinja Integration

Allows you to attach metadata, defaults, and strict constraints to a specific field. You can also use Jinja2 expressions (`{{ ... }}`) directly inside schemas to use your configuration data as validation input.

**`schema/main.schema`**

```yaml
$advanced_fields:
  environment:
    type: string
    default_value: "development"
    allowed_values: ["development", "staging", "production"]

```

---

### Conditional Logic (`$conditional_fields`)

Allows schemas to mutate based on the data present in the compiled tree. Uses an `$if` block (which supports `$and` and `$or`) to check sibling values. If the condition is met, the `$then` block applies a sub-schema.

**`schema/main.schema`**

```yaml
$conditional_fields:
  - $if:
      type: "database"
    $then:
      db_port: integer
      db_password: string

```

**`main.yaml`**

```yaml
type: "database"
db_port: 5432
db_password: "secure"

```