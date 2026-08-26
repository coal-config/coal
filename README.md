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

With the full structure finalized, COAL synchronizes the raw YAML representations with the deeply nested `CoalTree` nodes. It converts nodes with numeric keys into YAML lists and others into standard mapping objects, then hands every field that holds a template to the Jinja engine.

Templates are rendered in **passes**. Each pass renders every field whose own references are already resolved, and another pass follows for as long as that moves something forward. A field that reads a field which is itself still a template is simply put off until the pass after it, so declaration order never matters — `last: "{{ middle }}"` written above `middle: "{{ first }}"` resolves exactly as if it had been written below.

Each pass renders against the tree as it stood when the pass began and writes the results only afterwards, so no field ever sees half of another field's resolution.

### 3. Validation

If validation is enabled, COAL loads the `main.schema` file from the `schema/` directory. It takes the main schema and applies it to the root of the generated tree. This step enforces types, injects default values, evaluates conditional logic, and marks invalid fields. The diagnostics engine then scans the tree and collects any validation errors.

### 4. Final Cleanup

Before outputting the final YAML, COAL traverses the tree one last time and scrubs any keys that start with `$`. This ensures all internal system directives are hidden from the final compiled configuration file.

### 5. Alphabetical Ordering

As a final step, COAL sorts the entire tree alphabetically by key. The compiled YAML output is therefore always printed in alphabetical order by default, regardless of the order keys were declared in or which files they were merged from. List items are the exception: they keep their numeric order, so item `10` never jumps in front of item `2`.

---

## Cycle Detection

COAL guards every recursive phase against directives that reference each other in a loop:

* **`$include` cycles** (a file that includes a file that includes the first) are a **fatal error**. The build stops and names the file at which the include chain became too deep.
* **`$inherit` / `$override` cycles** (two nodes that inherit from each other) are a **fatal error**, naming the node where inheritance nested too deeply.
* **`$extend` / `$append` cycles** (directives that keep re-creating themselves) are a **fatal error**, naming the node with the runaway directive.
* **Jinja reference cycles** (a field whose `{{ ... }}` resolves, directly or transitively, back to itself) are **not fatal**. Such a field never becomes resolvable, so the passes simply stop making progress: the template is left in the output as written, and a warning naming the field is printed to `stderr`. The same warning reports a template that fails for any other reason — a name that does not exist, a filter applied to the wrong type — with the engine's own explanation.

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

## Jinja Templating

COAL embeds a complete Jinja2 engine. Anything Jinja can express is available in a configuration value, in a directive argument, and in a schema definition: filters, tests, arithmetic, comparisons, `{% for %}` and `{% if %}` blocks, `{% set %}`, and macros.

### What a name means

A template is always written *somewhere* in the tree, and that place decides what plain names mean. Lookup walks outward from the field the template sits on: its **siblings** first, then the siblings of its parent, and so on out to the root. The first scope that declares the name wins.

```yaml
paths:
  base: /srv
  logs: "{{ base }}/logs"      # 'base' is the sibling above
```

Two names are always available, whatever the scope:

| Name | Refers to |
| --- | --- |
| `ROOT` | the whole configuration tree |
| `PARENT`, `PARENT2`, `PARENT3`, ... | the object N levels up (`PARENT` is the object the field is written in) |

```yaml
name: root_name
outer:
  name: outer_name
  inner:
    name: inner_name
    nearest: "{{ name }}"          # inner_name  (nearest scope wins)
    parent: "{{ PARENT.name }}"    # inner_name  (PARENT is 'inner')
    grandparent: "{{ PARENT2.name }}"  # outer_name
    root: "{{ ROOT.name }}"        # root_name
```

### What a template produces

| The value | The result |
| --- | --- |
| exactly one `{{ ... }}` expression | the expression's own type — a list stays a list, an integer stays an integer, an object stays an object |
| text mixed with `{{ ... }}` | a string |
| anything using `{% ... %}` blocks | the rendered text, read back as YAML when that yields a list or a mapping, and left as text otherwise |

```yaml
services:
  api:
    port: 8080
  worker:
    port: 9090

names: "{{ services.keys() | list }}"        # a real list: [api, worker]
count: "{{ services | length }}"             # an integer: 2
label: "{{ services | length }} services"    # a string: "2 services"

endpoints: |                                  # a real list of objects
  {% for name, service in services.items() %}
  - name: {{ name }}
    url: "http://{{ name }}:{{ service.port }}"
  {% endfor %}
```

> **Note:** quote a value that begins with `{{`, or YAML reads it as a flow mapping. A block scalar (`|`) is the natural home for a `{% for %}` loop.

### Filters and methods

Every filter Jinja ships with is available — `map`, `select`, `selectattr`, `join`, `sort`, `unique`, `length`, `default`, `replace`, `split`, `int`, `list`, `items`, `tojson`, and the rest. COAL adds a few of its own:

| Filter | Does |
| --- | --- |
| `keys` | the keys of a mapping, as a list |
| `values` | the values of a mapping, as a list |
| `dict2items` | a mapping as a list of `{key, value}` entries |
| `items2dict` | the inverse of `dict2items` |
| `from_yaml` | parses a string as YAML |
| `to_yaml` | renders a value as YAML text |

Mappings and strings also answer the methods you would reach for out of habit: `.keys()`, `.values()`, `.items()`, `.get(key, default)`, and `.upper()`, `.lower()`, `.strip()`, `.split()`, `.startswith()`, `.endswith()`.

```yaml
eu_hosts: "{{ hosts | selectattr('zone', 'eq', 'eu') | map(attribute='name') | list }}"
first_name: "{{ hosts[0].name.upper() }}"
```

### Order does not matter

Templates are rendered in repeated passes (see [Processing](#2-processing-synchronization-and-jinja)), and a field is only rendered once every field it reads is itself resolved. A chain of references therefore resolves regardless of the order it is written in, and it never matters which file each link came from.

```yaml
last: "{{ middle }}/last"     # /start/middle/last
middle: "{{ first }}/middle"  # /start/middle
first: /start
```

Reading the object a field lives in — `{{ PARENT | length }}`, or anything that takes `ROOT` as a whole — is the one place this bends. Such a field waits for the rest of its container as usual, but once nothing else can be resolved it is allowed to look at the container as it stands, since waiting for itself is the only thing left to wait for.

A field whose answer would *contain* its own reference is refused rather than resolved, and reported like any other unresolved template:

```yaml
snapshot: "{{ PARENT }}"   # the answer would include this very line
```

A field that can never be resolved — a reference cycle, a name that does not exist, a filter applied to the wrong type — is left in the output exactly as written, and a warning explaining why is printed to `stderr`.

### Templates in directives

Directive arguments are templates too, so a single value can decide what gets built:

```yaml
environment: prod
active:
  $include: "./envs/{{ environment }}.yaml"
app:
  $inherit: "ROOT.defaults.{{ environment }}_defaults"
tier: "{{ 'high' if environment == 'prod' else 'low' }}"
```

This works for `$include` and its variants, for `$inherit`, `$override`, `$extend` and `$select` targets, and for `$id`/`$id_after` counter names.

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

`$declare` defines reusable sub-schemas; `$use` pulls one in.

**`schema/main.schema`**

```yaml
$declare:
  ip_config:
    address: string
    gateway: string

node_1:
  $use: "ip_config"
```

A used definition is merged **underneath** the block that uses it, so fields written next to `$use` stay valid and win where both define the same field — the rule `$inherit` follows in configuration files:

```yaml
$declare:
  ip_config:
    address: string
    gateway: string
    vlan: string

node_1:
  $use: "ip_config"
  vlan: integer      # overrides the declared 'vlan'
  role: string       # an extra field, perfectly valid
```

`$use` accepts a list, merged left to right:

```yaml
node_1:
  $use:
    - addressing
    - naming
```

A declaration does not have to be an object. When it is a type name or a list of allowed values, `$use` simply *is* that definition:

```yaml
$declare:
  port_number: integer
  speed:
    - fast
    - slow

port:
  $use: "port_number"
mode:
  $use: "speed"
```

`$declare` blocks are scoped: a declaration is visible to the block that declares it and to everything nested inside it, so a sub-schema can declare helpers of its own without leaking them.

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

Everything inside `$optional` is permitted but never required: if it is missing, the diagnostics engine does not flag it.

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

`$optional` is a full schema block rather than a plain list of fields, so it may carry `$use`, `$any`, `$keyed_by`, `$advanced_fields` and `$conditional` of its own. Anything it brings in is optional too:

```yaml
$declare:
  extras:
    description: string
    nickname: string

name: string
$optional:
  $use: "extras"
  labels:
    $any: string
```

### Allowed Values from the Configuration

A schema field may be a Jinja template instead of a type name. Whatever list the template produces *is* the set of values the field may hold, and the template is evaluated in the field's own scope — so it can read the field's siblings by name and reach anywhere else through `ROOT`.

**`schema/main.schema`**

```yaml
clusters: "[string]"
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

Because it is ordinary Jinja, the schema says exactly which values it means. A mapping has two sides, so name the one that constrains the field:

```yaml
transports:
  $any:
    hosts: "[string]"
link:
  transport: "{{ ROOT.transports.keys() }}"          # one of the declared transports
  host: "{{ ROOT.transports[transport].hosts }}"     # one of *that* transport's hosts
```

Here `transport` is the sibling field's value, so the hosts a `link` may choose from follow from the transport it picked:

```yaml
transports:
  tcp:
    hosts: [alpha, beta]
  udp:
    hosts: [gamma]
link:
  transport: tcp
  host: beta        # 'gamma' would be flagged: it belongs to udp
```

A template that resolves to a mapping is reported as a schema error rather than guessed at — add `.keys()` or `.values()` to say which side you meant.

Wrapping the template in brackets constrains every element of a list instead of the field itself, so `assigned_clusters: "[{{ ROOT.clusters }}]"` requires a list whose items are each one of the referenced values.

### Keyed Objects - `$keyed_by`

Where a template constrains the *values* of a field, `$keyed_by` constrains the *keys* of an object: the field must be a mapping whose keys are drawn from a fixed list, or from the list a template produces.

Pair it with `$any` to validate every value with the same sub-schema. Keys are checked as a subset: unknown keys are flagged, but a permitted key that is absent is not required.

**`schema/main.schema`**

```yaml
regions: "[string]"
deployments:
  $keyed_by: "{{ ROOT.regions }}"   # keys must be region names
  $any:                             # each value validated by this
    replicas: integer
```
##### Valid:
**`main.yaml`**
```yaml
regions: ["us", "eu"]
deployments:
  us:
    replicas: 3
  eu:
    replicas: 5
```
##### Invalid:
**`main.yaml`**
```yaml
regions: ["us", "eu"]
deployments:
  us:
    replicas: 3
  mars:        # 'mars' is not one of ROOT.regions -> flagged
    replicas: 1
```

`$keyed_by` also accepts an explicit list of keys (`$keyed_by: [primary, secondary]`). To key an object by the names declared in another object, ask for its keys: `$keyed_by: "{{ ROOT.available_roles.keys() }}"`.

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

### Conditional Logic (`$conditional`)

Lets a schema mutate based on the data in the compiled tree. Each entry pairs an `$if` condition with a `$then` block; when the condition holds, the `$then` block is applied on top of what the schema already says.

`$if` is a Jinja expression evaluated in the node's own scope, so it reads the node's fields by name and can reach anywhere else through `ROOT`. A field that is absent simply makes the condition false.

**`schema/main.schema`**

```yaml
type: string
$conditional:
  - $if: "{{ type == 'database' }}"
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
##### Invalid:
**`main.yaml`**

```yaml
type: "database"   # db_port and db_password are now mandatory and missing
```

Because the condition is an expression, `and`, `or`, `not`, comparisons, membership and filters all work directly:

```yaml
$conditional:
  - $if: "{{ type in ['database', 'cache'] and port > 1024 }}"
    $then:
      credentials: string
  - $if: "{{ ROOT.environment == 'production' }}"
    $then:
      replicas: integer
```

---

### Nesting the object directives

`$declare`/`$use`, `$optional`, `$conditional`, `$any`, `$keyed_by` and `$advanced_fields` all describe the *same* node, and each of them accepts a full schema block. They can therefore be nested inside one another to whatever depth the configuration calls for:

```yaml
$declare:
  tuning_block:
    cache_size: integer
    $optional:
      swap: bool

kind: string
$conditional:
  - $if: "{{ kind == 'database' }}"
    $then:
      db_port: integer
      $optional:
        tuning:
          $use: "tuning_block"
        $conditional:
          - $if: "{{ db_port > 1024 }}"
            $then:
              notes: string
```

Optionality is inherited: the `notes` field above sits inside an `$optional`, so the conditional that declares it grants it without ever requiring it.

Any `$` key the schema engine does not recognize is reported as a schema error rather than skipped, so a typo — or a directive under its old name — surfaces instead of quietly doing nothing.
