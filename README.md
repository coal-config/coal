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
* **`coal --param <path>=<value>`** (or `-p`) — Sets a value at a dotted path, seeded into the tree before the build so it can drive includes and inheritance. Repeatable. See [Command-Line Parameters](#command-line-parameters--param--p) below.

---

## Example

In `/example` there is an example coal confuration project. Run `coal` in this directory to see the output it creates. This output is in yaml format and can be directly imported with any yaml parser. 

---

## Command-Line Parameters (`--param` / `-p`)

The `-p` flag sets a value at a dotted path from the command line. The value is seeded into the tree **before** anything else is processed, so it can be used to *build* the rest of the configuration. This is useful for injecting an environment, a deployment region, or any other build-time input without editing the files.

```
coal -p environment.paths.config="/home/config"
```

The flag is repeatable, so you can set several values in one run:

```
coal -p environment.paths.config="/home/config" -p environment.region="eu-west"
```

**Syntax.** The argument is `path=value`. The path is dot-separated (an optional leading `ROOT.` is accepted). The value is everything after the first `=`, so values may themselves contain `=` (e.g. query strings). Missing intermediate objects along the path are created automatically, and if the target already exists its previous value — even a whole sub-tree — is replaced.

**Typing.** The value is parsed with the same rules as a YAML scalar: `true`/`false` become booleans, whole numbers become integers, decimals become floats, and everything else stays a string. So `-p service.port=8080` sets an integer. When `--validate` is used, parameters are validated against the schema like any other field (reported with an origin of `<command-line>`).

**Precedence.** A parameter behaves exactly as if its value had been written in the configuration at that path before the build started. It is *not* given any special authority: a later `$override` or an included file can still change it, just as they could change a value declared in the files. (`$inherit`, which keeps local values, will leave it untouched.)

### Parameters drive the build

Because a parameter is seeded before includes and inheritance are processed, it can decide *what* gets built. The common case is selecting an environment with a single parameter that chooses which files are included and which defaults are inherited.

```yaml
# main.yaml
environment: dev   # default; replaced by -p environment=prod
active:
  $include: "./envs/{{ ROOT.environment }}.yaml"
app:
  $inherit: "ROOT.{{ ROOT.environment }}_defaults"
```

```
coal -p environment=prod
```

Here `environment` is set before the include phase, so `./envs/prod.yaml` is the file that gets included and `ROOT.prod_defaults` is the node that gets inherited.

### Parameters and Jinja

Because parameters are seeded before Jinja templating, they integrate with it consistently:

1. **A parameter may contain a Jinja reference.** It is resolved against the final tree:

    ```
    coal -p log_directory="{{ ROOT.base_directory }}/logs"
    ```

2. **References elsewhere see the parameter value.** If another field is `{{ ROOT.base }}` and you set `base`, that field resolves to the parameter value, not the value originally in the file.

If a parameter's Jinja reference cannot be resolved (or forms a reference cycle), it is left in place as a literal string, exactly like any other unresolvable template.

---

## The Tree Building Pipeline

COAL processes configurations in a strict, multi-phase pipeline to ensure dependencies, structural morphing, and templates are resolved in the correct order.

### 0. Command-Line Parameters (seed)

Before anything else, any values supplied with `-p` (see [Command-Line Parameters](#command-line-parameters--param--p)) are seeded into the tree, so they can drive the rest of the build — for example an `environment` value referenced by Jinja inside `$include` paths or `$inherit` targets. From this point on they behave like any value declared in the configuration.

### 1. Expansion (Includes & AST Directives)

COAL parses the `main.yaml` file into a `CoalTree` root node and performs a depth-first traversal to expand the file system structure by processing `$include` directives, parsing those external files, and splicing them directly into the current tree. Once all files are loaded into memory, it performs another pass to evaluate AST structural directives (like `$inherit`, `$select`, and `$each`) to dynamically duplicate, merge, and generate nodes. ID directives resolve at two points in this pipeline: `$id` during the AST directive phase (with inheritance targets resolved just before they are cloned), and `$id_after` at the very end, once the full tree is built and all Jinja templates are resolved.

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
| `$parent` | Replaced with the key of the node's immediate parent (same as `$parent1`). |
| `$parentN` | Replaced with the key of the ancestor `N` levels up (e.g. `$parent2` is the grandparent). Placeholders that reach past the tree root are left untouched. |
| `$id` | Generates a sequentially incrementing integer, resolved **before** inheritance/overriding. |
| `$id_after` | Generates a sequentially incrementing integer, resolved **at the very end** of the pipeline. |

**`main.yaml`**

```yaml
environments:
  production:
    servers:
      web_server:
        name: "$parent"
        environment: "$parent3"
```

**`Compiled Output`**

```yaml
environments:
  production:
    servers:
      web_server:
        name: "web_server"
        environment: "production"
```

### ID Resolution Timing

Both ID variants share the same counter pool: a counter name like `"db_counters"` keeps incrementing no matter which variant references it. The counter name may contain `$parent`/`$parentN` placeholders (resolved relative to the `$id` field, so `$parent` is the node that holds the `$id`) and Jinja templates (e.g. `$id: "{{ ROOT.pool_name }}_pool"`); both are resolved before the counter is looked up. So `$id: "$parent"` gives each holder its own counter. The difference between the two variants is *when* the value is locked in:

* **`$id`** resolves during structural processing. Whenever a node is the target of `$inherit` or `$override`, every `$id` inside that target is resolved **just before** it is cloned — so the inheritor receives the actual ID value instead of the tag, and the target plus all of its inheritors share the same number.
* **`$id_after`** resolves at the very end, once the whole tree is built and all Jinja templates are resolved. The tag survives cloning, so **every occurrence — including every clone** made by `$each`, `$inherit`, or `$override` — receives its own unique number.

**`main.yaml`**

```yaml
base:
  shared_db:
    $id: "pool"        # Resolved before inheritance -> shared
  unique_db:
    $id_after: "pool"  # Resolved at the end -> unique per node
app_one:
  $inherit: "ROOT.base"
app_two:
  $inherit: "ROOT.base"
```

**`Compiled Output`**

```yaml
app_one:
  shared_db: 1
  unique_db: 3
app_two:
  shared_db: 1
  unique_db: 4
base:
  shared_db: 1
  unique_db: 2
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