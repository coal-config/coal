# Coalescence (COAL)

Coalescence (COAL) is a powerful, Rust-based configuration builder and validation engine. It solves the biggest problem with infrastructure-as-code: sprawling, unmaintainable, monolithic YAML files.

COAL allows you to split your configurations into logical, modular directories, organize them exactly how you want, and use powerful inheritance to keep your code strictly DRY (Don't Repeat Yourself). It compiles your modular structure into a single, fully resolved, and strictly validated standard YAML file.

**Why YAML?** Everything in COAL is driven by YAML. YAML was chosen because it is incredibly clear, human-readable, and easy to use. It allows developers and operators to define complex hierarchical structures, references, and logic without the visual clutter of traditional programming languages.

### Quick Install
```
curl -fsSL https://raw.githubusercontent.com/coal-config/coal/main/install.sh | bash
```
### 🛑 Before: The Monolithic YAML Nightmare
Standard YAML forces you to cram everything into massive files, relying on copy-pasting for shared configuration parts between the files. This makes it incredibly hard to read and can cause configuration drift.

`config.yaml`
```yaml
environments:
prod:
  url: "prod.local"
  port: 80
  logging: true
  database: "postgres"
dev:
  url: "dev.local"
  port: 80
  logging: true
  database: "sqlite"
staging:
  url: "staging.local"
  port: 80
  logging: true
  database: "postgres"
```

### ✨ After: Modular Organization & Inheritance with COAL
COAL lets you split this into a clean directory structure. You define your base configuration once, dynamically include your environment files, and inherit the base settings.

Your File Structure:
```
├── main.yaml
├── base.yaml
└── envs/
    ├── prod.yaml (empty file in this example)
    ├── dev.yaml
    └── staging.yaml (empty file in this example)
```
`base.yaml` Define it once
```yaml
port: 80
logging: true
database: "postgres"
```
`envs/dev.yaml` Only define what changes
```yaml
url: "dev.local"
database: "sqlite" # Overrides base
```
`main.yaml` Bring it all together dynamically
```yaml
environments:
  $include_directory: "./envs"
  $each:
    $inherit: "ROOT.base"
```
Run `coal` and it compiles to the yaml shown in the before example.

---

## Command Line Interface

COAL provides a straightforward CLI for compiling and inspecting your configurations:

* **`coal`** — Compiles the `main.yaml` file in the current directory, resolves all directives and Jinja templates, and prints the final flattened YAML to standard output.
* **`coal --validate`** (or `-v`) — Compiles the tree and runs it against the schema definitions in the `schema/` directory. If errors are found, it outputs a detailed diagnostic report to `stderr` and exits with a non-zero code.
* **`coal --analyze <path>`** (or `-a`) — Dumps the internal Abstract Syntax Tree (AST) properties of a specific field (e.g., `coal -a processes.cms`). Displays the key, value, resolved schema type, origin file, allowed values, and physical line/column span.

---

## Example

In `/example` there is an example coal confuration project. Run `coal` in this directory to see the output it creates. This output is in yaml format and can be directly imported with any yaml parser. 

---

## The Tree Building Pipeline

COAL processes configurations in a strict, multi-phase pipeline to ensure dependencies, structural morphing, and templates are resolved in the correct order.

### 1. Expansion (Includes & AST Directives)

COAL parses the `main.yaml` file into a `CoalTree` root node and performs a depth-first traversal to expand the file system structure by processing `$include` directives, parsing those external files, and splicing them directly into the current tree. Once all files are loaded into memory, it performs another pass to evaluate AST structural directives (like `$inherit`, `$select`, and `$each`) to dynamically duplicate, merge, and generate nodes. ID directives resolve at fixed points in this pipeline: `$id_now` during include expansion, `$id` right after all includes are loaded, and `$id_end` once the AST directives have finished building the tree.

### 2. Processing (Synchronization and Jinja)

With the full structure finalized, COAL synchronizes the raw YAML representations with the deeply nested `CoalTree` nodes. It converts nodes with numeric keys into YAML lists and others into standard mapping objects, then resolves all Jinja templates (`{{ ... }}`). Because the structure is locked at this point, absolute paths can be reliably mapped to their target nodes and values are safely substituted.

### 3. Validation

If validation is enabled, COAL loads the `main.schema` file from the `schema/` directory. It takes the main schema and applies it to the root of the generated tree. This step enforces types, injects default values, evaluates conditional logic, and marks invalid fields. The diagnostics engine then scans the tree and collects any validation errors.

### 4. Final Cleanup

Before outputting the final YAML, COAL traverses the tree one last time and scrubs any keys that start with `$`. This ensures all internal system directives are hidden from the final compiled configuration file.

### 5. Alphabetical Ordering

As a final step, COAL sorts the entire tree alphabetically by key. The compiled YAML output is therefore always printed in alphabetical order by default, regardless of the order keys were declared in or which files they were merged from. List items are the exception: they keep their numeric order, so item `10` never jumps in front of item `2`.

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
| `$id_now` | Generates a sequentially incrementing integer **as soon as the directive is detected**, during the include expansion phase. |
| `$id` | Generates a sequentially incrementing integer **after all includes are resolved**, but before AST structural directives run. |
| `$id_end` | Generates a sequentially incrementing integer **once the whole tree is built**, after all AST structural directives have run. |

### ID Resolution Timing

All three ID variants share the same counter pool: a counter name like `"db_counters"` keeps incrementing no matter which variant references it. The difference is *when* the value is locked in, which matters as soon as cloning directives (`$each`, `$inherit`, `$override`) are involved:

* **`$id_now`** resolves during Phase 1 (Expansion), the moment the node is encountered. Every physical occurrence in a source file gets its own number, in file-discovery order.
* **`$id`** resolves in a dedicated pass right after Phase 1 completes. Because this happens *before* `$each`/`$inherit` cloning, any clones made afterwards **share** the already-resolved number.
* **`$id_end`** resolves in a dedicated pass right after Phase 2 (AST Directives) completes. Because clones still carry the unresolved directive at that point, **every clone gets its own unique number**.

**`main.yaml`**

```yaml
servers:
  $each:
    static_id:
      $id: "pool"        # Resolved before cloning -> shared
    unique_id:
      $id_end: "pool"  # Resolved after cloning -> unique per server
  srv_a:
    ip: "1.1.1.1"
  srv_b:
    ip: "2.2.2.2"
```

**`Compiled Output`**

```yaml
servers:
  srv_a:
    ip: "1.1.1.1"
    static_id: 1
    unique_id: 2
  srv_b:
    ip: "2.2.2.2"
    static_id: 1
    unique_id: 3
```

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
##### Valid:
**`main.yaml`**

```yaml
server_name: "prod-01"
ports: [80, 443]
tags: ["web", "frontend"]
```
---

### Advanced Schema Directives

#### Schema - `$schema`

Acts as a mixin, deep-merging another schema file from the `schema/` directory into the current location.

**`schema/main.schema`**

```yaml
my_server:
  $schema: "server_schema"  # Merges schema/server_schema.yaml
```

#### Declare - `$declare` / `$use`

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

#### Any - `$any`

Permits the object to contain undeclared fields without throwing an error.

**`schema/main.schema`**

```yaml
process:
  $any: 
    exec: string
    args: [string]
```
##### Valid:
**`main.yaml`**

```yaml
process:
  encoder:
    exec: "/bin/encoder"
    args: 
      - "-v"
  decoder:
    exec: "/bin/decoder"
    args: 
      - "-s"
```

#### Optional - `$optional`

A mapping of fields that are permitted but not strictly required. If they are missing, the diagnostics engine will not flag them as errors.

**`schema/main.schema`**

```yaml
name: string
$optional:
  description: string
```
##### Valid:
**`main.yaml`**

```yaml
name: "Server 1"
```
##### Valid:
**`main.yaml`**

```yaml
name: "Server 2"
description: "Valid server"
```
---

### Advanced one-lines with Jinja Integration

Allows you to use Jinja2 expressions (`{{ ... }}`) directly inside schemas to use your configuration data as validation input.

**`schema/main.schema`**

```yaml
clusters: [string]
assigned_cluster: "{{ ROOT.clusters }}"

```
##### Valid:
**`main.yaml`**
```yaml
clusters: 
  - "prod"
  - "staging"
assigned_cluster: "staging"
```

##### Invalid:
**`main.yaml`**
```yaml
clusters: 
  - "prod"
  - "staging"
assigned_cluster: "test"
```
---

### Advanced Fields - `$advanced_fields`

Allows you to attach metadata, defaults, and strict constraints to a specific field.

**`schema/main.schema`**

```yaml
process: string
$advanced_fields:
  environment:
    type: string
    default_value: "development"
    allowed_values: ["development", "staging", "production"]
```

##### Valid:
**`main.yaml`**
```yaml
process: "encoder"
environment: "staging"
```

##### Valid:
**`main.yaml`**
```yaml
process: "encoder"
```
**`Compiled Output`**

```yaml
process: "encoder"
environment: "development"
```
##### Invalid:
**`main.yaml`**
```yaml
process: "encoder"
environment: "test"
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
##### Valid:
**`main.yaml`**

```yaml
type: "database"
db_port: 5432
db_password: "secure"
```