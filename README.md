# Coalescence (COAL)

**COAL turns a directory of small YAML files into one validated YAML configuration.** You never edit a monolithic file again: the folders *are* the structure, anything shared is written once, values are computed with Jinja, and the schema checks the result against your own data.

```
curl -fsSL https://raw.githubusercontent.com/coal-config/coal/main/install.sh | bash
```

### Four ideas

1. **The directory is the configuration.** `services/api.yaml` *is* `services.api`. A folder is a level of nesting, a file is a field. Nothing declares the layout, because the folders already show it.
2. **No monolithic YAML.** Split the configuration wherever it reads best. Write shared settings once in an `_each` file, and they apply to every sibling. Each file then holds only what makes it different.
3. **Jinja everywhere.** Any value can be a template that reads any other part of the configuration: `"{{ databases[database].host }}"`, `"{{ parent().key() }}"`, loops, filters and conditions.
4. **Schemas that know your data.** The schema is YAML with Jinja too, so what counts as valid can come from the configuration itself. Add a database to `databases.yaml`, and every service may use it at once, with no schema edit.

### In one example

```
├── databases.yaml
├── services/
│   ├── _each.yaml       written once, applied to every service
│   ├── api.yaml
│   └── web.yaml
└── _schema/
    └── _self.yaml
```

`databases.yaml`
```yaml
main:
  host: db1.internal
analytics:
  host: db2.internal
```

`services/_each.yaml`: the shared part, computed per service
```yaml
port: 8080
url: "https://{{ parent().key() }}.example.com:{{ port }}"
db_host: "{{ databases[database].host }}"
```

`services/api.yaml` and `services/web.yaml`: only what differs
```yaml
database: main
```
```yaml
port: 443
database: main
```

`_schema/_self.yaml`: the allowed databases are *read from the data*
```yaml
databases: object
services:
  $each:
    port: integer
    url: string
    database: "{{ databases.keys() }}"
    db_host: string
```

`coal` compiles the directory into plain YAML:

```yaml
databases:
  analytics:
    host: db2.internal
  main:
    host: db1.internal
services:
  api:
    database: main
    db_host: db1.internal
    port: 8080
    url: https://api.example.com:8080
  web:
    database: main
    db_host: db1.internal
    port: 443
    url: https://web.example.com:443
```

Change `web.yaml` to `database: cache`, and `coal -v` reports each mistake at the line to fix:

```
services/_each.yaml:3:1  UnresolvedTemplate
  services.web.db_host -- undefined value

services/web.yaml:2:1  ValueNotAllowed
  services.web.database -- value: String("cache") -> not in allowed list
```

Nobody had to add `cache` to a list in the schema, or keep one up to date. The schema read `databases.yaml`.

The same approach scales up. [`_definitions`](#_definitions-shapes-with-a-name) hold reusable shapes that `$include` pulls in. [`_structure.yaml`](#_structureyaml-selecting-a-configuration) builds many configurations (environments, node types, modes) out of one tree. `coal -v` checks every one of them. The [VS Code extension](#in-the-editor) shows the same findings, plus completion, while you type.

**Why YAML?** It is clear, human-readable and familiar to anyone in operations. COAL keeps it that way: its input is plain YAML files, and so is its output.

---

## The Directory Is The Configuration

A build reads the directory it was started in. There is no entry file:

* every `.yaml` / `.yml` file becomes a **field named after the file**,
* every subdirectory becomes a **level of nesting**,
* `_self.yaml` holds the fields of **the directory itself**.

```
node_types.yaml                    ->  node_types
environments/production.yaml       ->  environments.production
environments/staging/_self.yaml    ->  environments.staging
environments/staging/limits.yaml   ->  environments.staging.limits
```

A file may sit beside a directory of the same name (`app.yaml` next to `app/`); the two are merged, and the directory wins where they disagree, because a directory says more about a field than the file beside it does. Declaring the same field twice with `.yaml` and `.yml` is an error rather than a coin toss.

A file usually holds a mapping, but it does not have to. One holding a single value is that field's value, which is the natural way to write `environment.yaml` containing `production`.

### Reserved names

A file or folder whose name starts with `_` is **never loaded as data**. Five names mean something; anything else starting with `_` is ignored entirely, which makes `_` the way to park a file the build should not read.

| Name | Meaning | Where |
| --- | --- | --- |
| `_self` | the fields of the directory itself | any directory |
| `_each` | a template merged into every sibling field | any directory |
| `_optional` | in a schema, the fields it permits without requiring | any schema directory |
| `_definitions` | content `$include` can name, in that directory and below | any directory |
| `_schema/` | the schema definitions | root only |
| `_structure.yaml` | the selection and layering rules | root only |

The first three may be written **either way round**: `_each.yaml` says exactly what `_each/` says, and a two-line template is far easier to read as one file than as a folder holding one file. Where both are present they merge, and the folder wins — the same rule as `app.yaml` beside `app/`. `_each` can also be written [inside a file, as `$each`](#each-written-in-the-file).

```
services/
├── _each.yaml         port: 8080
├── api.yaml
└── web.yaml
```

`_each` and `_definitions` are themselves loaded exactly as a configuration folder is, so everything on this page applies inside them too — including further `_each` folders.

---

## `_each`: said once, for every sibling

An `_each` folder is merged into **every sibling field of the directory that holds it**. In the root directory that means every top-level field.

```
services/
├── _each/
│   └── _self.yaml     port: 8080
├── api.yaml           port: 9000
└── web.yaml           name: web
```

```yaml
services:
  api:
    port: 9000        # what the file says wins
  web:
    name: web
    port: 8080        # supplied by _each
```

**A value already there always wins.** `_each` supplies what a field did not say for itself, it does not overrule it.

`parent()` inside an `_each` template is the field the copy landed in, not the template's own folder, which is how one template gives each sibling something of its own:

```yaml
# services/_each/_self.yaml
name: "{{ parent().key() }}"           # api, web, ...
log: "{{ paths.log_dir }}/{{ parent().key() }}.log"
```

`_each` folders nest: an `_each` inside an `_each` applies to the children of the field it was copied into. And because the template is applied for as long as the tree is still growing, a field that arrives later — from an `$include`, or from a structure layer — still receives it.

### `$each`, written in the file

The same thing can be said inside a file, as `$each`. It means exactly what the folder means — a template merged into every sibling — and for a small template that is usually the easier read, because the template and the fields it applies to are in front of you at once:

```yaml
# services.yaml
$each:
  port: 8080
  name: "{{ parent().key() }}"

api:
  role: front
web:
  role: back
  port: 9000       # still wins over the template
```

It applies to its siblings at whatever level it is written, so a file can carry one template per object:

```yaml
# regions.yaml
eu:
  $each:
    tier: standard
  zone_a: {}
  zone_b:
    tier: premium
```

Everything said above holds: values already there win, `parent()` is the field the copy landed in, and a directive inside the template — an `$inherit`, say — is evaluated in each copy rather than where it is written.

Where a directory uses more than one form, they merge, most specific last: an inline `$each`, then `_each.yaml`, then `_each/`.

An `$each` written in a [`_definitions`](#_definitions-shapes-with-a-name) entry travels with it. It is applied where the definition **lands**, not where it is written, so one definition can say what every entry it brings in should carry:

```yaml
# _definitions/alarm_sets.yaml
node_alarms:
  $each:
    alarm_id:
      $id_after: alarm_id
  disk_status: { level: critical }
  cms_status:  { level: critical }
```

```yaml
# nodes/_each/alarms/_self.yaml
$include: alarm_sets.node_alarms
```

Every node gets both alarms, and every alarm its own `alarm_id`.

---

## `_definitions`: shapes with a name

A `_definitions` folder is loaded like any configuration folder, into a namespace that `$include` reads. What is included is a name rather than a file location. Which names an include can see depends on where it is written — see [Definitions lower down override](#definitions-lower-down-override) below.

```
_definitions/
└── templates/
    ├── base.yaml       retries: 3
    └── heavy.yaml      workers: 12
```

```yaml
app:
  $include: templates.base       # retries: 3
```

A path may name a folder as readily as a file — `$include: templates` brings in both entries as `base` and `heavy` — because the namespace is a tree like any other.

### A path reaches as far down as it likes

The namespace is a tree, so a path is not limited to the name a file or folder gave it. Every level below that is addressable too, and a path may end on an object, on a plain value, or on one element of a list:

```
_definitions/
└── sizes.yaml
```

```yaml
# _definitions/sizes.yaml
heavy:
  workers: 12
  timeout: 30
  arguments: ["-v", "-production"]
```

```yaml
app:
  $include: sizes.heavy                  # the whole object
timeout:
  $include: sizes.heavy.timeout          # 30
arguments:
  $include: sizes.heavy.arguments        # the list
first_argument:
  $include: sizes.heavy.arguments.0      # "-v"
```

The first segment is the name the file or folder contributed, and the rest walks into what it holds — so `_definitions/alarms.yaml` written as `cms_alarm: {...}` is reached as `alarms.cms_alarm`. A file whose own top-level key repeats its name adds a level rather than collapsing into one: `alarms.yaml` holding `alarms:` is `alarms.alarms.cms_alarm`. A path that goes wrong says what the namespace does have at the point it stopped, which is the quickest way to see that.

### Definitions lower down override

A `_definitions` folder belongs to the directory that holds it. An `$include` sees the definitions of the directory its **file** sits in and of every directory above it, merged from the root down, so a lower level only writes what it changes:

```
_definitions/
└── templates/base.yaml          retries: 3, timeout: 10, logging: {level: info, format: json}
nodes/
├── _definitions/
│   └── templates/base.yaml      retries: 5, logging: {level: debug}
└── cop01.yaml                   $include: templates.base
app.yaml                         $include: templates.base
```

```yaml
app:
  retries: 3
  timeout: 10
  logging: {level: info, format: json}
nodes:
  cop01:
    retries: 5                   # nodes/ overrides the root
    timeout: 10                  # the rest still comes from the root
    logging: {level: debug, format: json}
```

* **Objects merge field by field; anything else is replaced.** A plain value or a list written lower down replaces the one above it as a whole.
* **The file decides, not where the field ends up.** An include in `nodes/_each.yaml` reads what `nodes/` sees, wherever `_each` copies it and whatever a structure layer later does with it.
* **A folder's definitions are invisible outside it.** `east/_definitions` and `west/_definitions` may both define `site` without clashing; an include in `app.yaml` sees neither. Within one directory, `_definitions.yaml` and `_definitions/` declaring the same field is still an error.
* **An override reaches through shared definitions.** An `$include` that arrives inside an included definition reads what the place that asked for it sees. If the root's `stack` includes `sizes.default`, then `stack` included from `nodes/` gets `nodes/_definitions`' `sizes.default`.

The schema follows the same rule inside `_schema/`: `_schema/services/_definitions/` overrides `_schema/_definitions/` for schema written under `_schema/services/`.

### Several paths, and who wins

`$include` accepts a list. Each path refines the one before it, and **what is written locally beats all of them**:

```yaml
app:
  $include:
    - templates.base       # workers: 4, site: shared
    - templates.heavy      # workers: 12
  site: local
```

```yaml
app:
  workers: 12    # the later path won
  site: local    # the local line won
```

`$include_optional` is the same directive for a name that may not be there; if it is not, nothing happens.

### An include can wait for a value

A path is a template like any other, so one part of the configuration can decide what another part includes. A path that reads a field the tree has not grown yet is not a missing name — it is an early one, and the build comes back to it:

```yaml
# environment.yaml holds "production", mode.yaml holds "operational"
$include:
  - "environments.{{ environment }}"
  - "modes.{{ mode }}"
```

A path that never becomes resolvable is a fatal error naming the reference it waited for, rather than a report that something called `{{ mode }}` is missing.

---

## `$inherit`: deriving from a value

`_structure.yaml` says how the configuration as a whole is assembled for the one thing being built. `$inherit` answers a different question: how one *object* derives from another, chosen by a value that object carries.

```
sensor_types.yaml
sensors/
├── _each.yaml
├── probe_a.yaml       sensor_type: thermal
└── probe_b.yaml       sensor_type: pressure
```

**`sensor_types.yaml`**

```yaml
thermal:
  unit: celsius
  poll_interval: 30
pressure:
  unit: bar
  poll_interval: 10
```

**`sensors/_each.yaml`**

```yaml
$inherit: "sensor_types.{{ parent().sensor_type }}"
```

One line, inside `_each`, and every sensor takes the values of whichever type it named:

```yaml
sensors:
  probe_a:
    sensor_type: thermal
    unit: celsius
    poll_interval: 30
  probe_b:
    sensor_type: pressure
    unit: bar
    poll_interval: 10
```

**`parent()` is the object carrying the directive** — the sensor — which is how the same line reads a different value in each of them. Writing `{{ sensor_type }}` would work too when the sensor is a single file, because a bare name is read at the file's own top level first; `parent()` says it outright and keeps working however the sensor's files are laid out.

What the target says is merged **underneath** the object, so anything the object states for itself wins. A list of targets refines left to right, exactly as `$include` does:

```yaml
$inherit:
  - defaults
  - heavy       # refines defaults
site: local     # beats both
```

A target may itself inherit, and the chain is followed before anything is taken from it. A target that names nothing is an error rather than a silent no-op: an object that says what it derives from and does not get it is wrong, not empty.

The path is read the way any other reference is (see [When a name is read](#when-a-name-is-read)): unless the file the directive is written in answers for it outright, it is worked out **after** `_structure.yaml` has assembled the configuration. An object therefore inherits from the type the finished configuration names it, not from whichever value the tree happened to hold before the layers were merged. One consequence is worth knowing: a `drop` that removes what an `$inherit` points at removes it before the directive runs, and the build says so rather than quietly leaving the object bare.

---

## `_structure.yaml`: selecting a configuration

A configuration usually describes many things — every environment, every mode, every node — while a build is nearly always *for* one of them. `_structure.yaml` says which parts of the tree name a selectable thing, and which parts are layered on top of each other to produce it.

Given a configuration shaped like this:

```yaml
node_types:
  cop:
    role: control
    workers: 2
environments:
  production:
    network:
      net: 10.0.0
    nodes:
      cop01:
        node_type: cop
        ip: "{{ network.net }}.1"
    modes:
      operational:
        workers: 12
```

```yaml
# _structure.yaml
selectors:
  - environments.{environment}.nodes.{node}
  - environments.{environment}.modes.{mode}
layers:
  - node_types.{{ node.node_type }}
  - environments.{{ environment }}
  - {node}
  - {mode}
drop: environments
```

`coal -s cop01 -s operational` finds the one place each selector matches, merges the layers onto the root weakest first, and drops the scaffolding.

### Selectors

A selector is a path with `{captures}` in it. Its **last** segment is the thing being selected, and names the selector — `environments.{environment}.nodes.{node}` selects a `node`. Every other capture is settled by the match and can be read by a layer.

A capture already settled constrains the ones after it. `{environment}` is bound to `production` by the node selector, so the mode is then looked for in production and nowhere else — a node in one environment is never built with a mode from another. Pinning one yourself works the same way: `-p environment=production`.

### Making the selection

A selection is not a separate mechanism: a selector names an **ordinary root field**, and the structure applies as soon as that field has a value. A `node.yaml` holding `cop01` commits a default selection to the repository, which `-s` then overrides, and `-p node=cop01` does the same thing since it is just a value.

A selection is **any right-aligned part of the selector's path**, so all of these name the same node:

```
coal -s cop01
coal -s nodes.cop01
coal -s production.nodes.cop01
coal -s environments.production.nodes.cop01
```

The longer forms exist for when the short one names more than one place — the same node declared in two environments, say. Everything a longer form names along the way is pinned too, so `-s production.nodes.cop01` settles `{environment}` as well. Where a bare name could answer either of two selectors, the configuration settles it: `-s cop01 -s operational` needs no help, because only one of them holds each name. `-s node=cop01` says it outright.

With no selection set, the tree compiles exactly as written.

### Layers

Layers are **one list, weakest first, shared by every selector**. That shared order is the point of allowing several selections at once: it is where the configuration says whether a node's own settings beat the mode it picked, or the other way round.

An entry is either of:

| Written | Means |
| --- | --- |
| `{node}` | the node that selector matched |
| a Jinja template | a path from the configuration root |

Inside a template, a capture reads as its value and a selector reads as its match:

| Written | Reads |
| --- | --- |
| `{{ environment }}` | `production` — a capture's value |
| `{{ node }}` | `cop01` — the match, which prints as its own key |
| `{{ node.node_type }}` | `cop` — a field of the match |

A bare name in a layer reads off the first selection made, so `{{ node_type }}` still works when there is one selector. With more than one, prefer the explicit `{{ node.node_type }}`: it says whose.

A selector that nothing was selected for contributes nothing, and its `{name}` layer is skipped. A selector that matched but is never named in `layers` is added at the end, since the selected thing itself is usually the most specific thing there is to say.

> **A note on quoting.** `- {node}` is a flow mapping as far as YAML is concerned. COAL reads a one-key mapping with nothing on the right as the selector it obviously is, so you can write it either way — `- {node}` or `- "{node}"`.

**Sparse by default.** A layer whose path names nothing is skipped rather than reported: most environments have nothing to say about most node types, and having to declare an empty object for each would defeat the point. A layer whose **first** name is not a field of the configuration at all is different — there is no corner for it to be missing from, so it can never contribute anything however the build is run, and that is reported:

```
❌ Fatal Error: The layer 'environment.{{ environment }}' reads 'environment', which is not
a field of the configuration, so it can never contribute anything. Did you mean 'environments'?
```

**Merging.** Objects are merged field by field, so a layer only has to say what it changes. Everything else replaces what was there — a list included, which is why a layer that gives a list gives the whole list.

A layer contributes *everything* under it, sub-structures included: `environments.production` above carries its own `nodes`, `modes` and `network`. That is what makes `{{ network.net }}` resolvable on the node — the base configuration need not have a `network` at all.

Directives come along with the object they were written on. An `$inherit` or an `$ignore` on the selected node is evaluated *after* the merge, against the configuration the merge produced — see [When a name is read](#when-a-name-is-read).

**`drop` / `keep`** decide what survives beside the merged result, and only one of the two may be given. Without either, everything stays. `drop` removes the named root paths; `keep` names the only ones that stay. Whatever the layers contributed always survives `keep` — it is the thing that was asked for. Either takes one name or a list:

```yaml
drop: environments
```

```yaml
drop:
  - environments
  - nodes
```

For the configuration above, `coal -s cop01 -s operational` produces:

```yaml
ip: 10.0.0.1
mode: operational
network:
  net: 10.0.0
node: cop01
node_type: cop
role: control
workers: 12      # the mode, layered over the node type's 2
```

### Building without choosing

`coal` with nothing selected builds the **first** configuration the structure describes rather than the directory as written, because the directory as written is not a configuration: nothing has been layered onto it, and the scaffolding the structure drops is still in it. It says so once the configuration has been printed:

```
Note: nothing was selected, so this is the first configuration the structure describes: cop01, operational.
      Say which one you want to be sure of it: coal -s node=cop01 -s mode=operational
```

### Validating every configuration

A schema describes what comes **out** of a selection. The fields a node inherits from its environment and its type only exist once the layers have merged, and a part the structure drops is in no configuration at all — so checking the directory as written would both invent problems no configuration has and miss the ones every configuration has.

`coal --validate` therefore compiles and checks **every** configuration the structure can produce — with several selectors, every combination of them, and only the combinations that can actually be built. It prints nothing to standard output; validating is a question about the configuration, not a way of producing it.

One mistake is one finding. A shared file is read once per selection, so reporting findings as they arrive would repeat the same line once per node, per mode, per environment that reaches it. Instead each is reported once, at the place it is written, with a line saying how far it reaches:

```
Checked 4 configurations.

Found 3 problems:

environments/production/_self.yaml:1:1  InvalidType
  region -- type: list -> expected string
  in all 4 configurations

environments/production/modes/test.yaml:1:1  InvalidType
  live -- type: string -> expected bool
  in 2 of 4 configurations, whenever mode = test

environments/production/nodes/cop02.yaml:2:1  InvalidType
  workers -- type: string -> expected integer
  in 1 of 4 configurations, whenever node = cop02 and mode = test
```

The reach is worked out from the selections themselves: where every affected configuration makes a selection and no unaffected one does, that selection is named — *whenever `mode = test`* — and where several are needed they are named together. A finding that follows no selection in particular names the configurations instead.

Narrow it by pinning a capture — `coal -v -p environment=production` checks only the nodes in production — or check exactly one with `coal -v -s cop01 -s operational`.

---

## Command Line Interface

* **`coal`** — Compiles the directory it is run in, resolves all directives and Jinja templates, and prints the final flattened YAML to standard output. With a `_structure.yaml` and nothing selected it builds the first configuration the structure describes, and says so.
* **`coal --select <[name=]value>`** (or `-s`) — Builds one of the configurations the tree describes, as declared in `_structure.yaml`. Repeat it once per selector: `coal -s cop01 -s operational`. The value is any right-aligned part of the selector's path, so `-s environments.production.nodes.cop01` says which one when the short name would name more than one place.
* **`coal --validate`** (or `-v`) — Checks the configuration against the schema definitions in `_schema/` instead of printing it. With nothing selected and a `_structure.yaml` present, it compiles and checks **every** configuration the structure describes — with several selectors, every combination of them that can actually be built — and reports each mistake once, in the file and on the line it is written, saying which configurations run into it and which selection is responsible. If problems are found it writes the report to `stderr` and exits with a non-zero code.
* **`coal --analyze <path>`** (or `-a`) — Dumps the internal Abstract Syntax Tree (AST) properties of a specific field (e.g., `coal -a processes.cms`). Displays the key, value, resolved schema type, origin file, allowed values, and physical line/column span.
* **`coal --param <path>=<value>`** (or `-p`) — Sets a value at a dotted path, seeded into the tree before the build so it can drive includes, layers and selections. Repeatable. See [Command-Line Parameters](#command-line-parameters--param--p) below.
* **`coal <path>`** — Prints only that part of the compiled configuration (e.g. `coal processes`).

---

## In the editor

The language server (`coal-lsp`, driven by the VS Code extension in `/coal_extension`) validates exactly what `coal --validate` validates: every configuration the `_structure.yaml` describes, merged. It is the same code path, so an editor and a build never disagree about whether something is wrong.

### Which folders are projects

The folder open in the editor does not have to be the configuration's root. The language server looks through the workspace for the folders that **are** roots, and validates each of them on its own, exactly as `coal --validate` run in that folder would. A folder is a COAL project when it holds

* `_structure.yaml`, or a `_schema/` folder — both belong only in a root, so a typical project is recognised without adding anything; or
* a file named `.coal` — an empty marker, for a project that has neither.

A file belongs to the nearest project above it: hovering and completion answer from that project's build. Several projects can sit side by side in one workspace, a project in a subfolder is found, and YAML outside every project is left alone. Opening a file from a project outside the workspace folders adds that project too. Folders whose name starts with `_` or `.`, `node_modules` and `target` are not searched. Adding or removing a marker is picked up without a restart; the server's log (*Output → COAL Language Server*) lists the projects it found.

An edit only rebuilds the projects that read the file.

Two things follow from that, and both are what you want:

* A squiggle sits on the line that has to change, which is very often **not** the file you selected. A node inherits most of what it is from its environment and its type, and that is usually where the mistake is.
* A part the structure **drops** is in no configuration, so nothing is reported in it. Scaffolding is not checked against a schema that never described it.

A finding read once per configuration is one squiggle, not a stack of them, and its message says how far it reaches — `(in all 12 configurations)`, or `(in 3 of 12 configurations, whenever mode = test)`.

Hovering and completion answer from the **first** configuration the structure describes, for the same reason `coal` with nothing selected builds it: a value is what its layers settled on, and outside a configuration there is no such value to show.

The cost is one compile per configuration. Validation is debounced while you type and runs one build at a time: an edit made while a build is running is picked up by the next build, which starts as soon as the running one has been published. Findings therefore always arrive in the order the edits were made, and a finding you have fixed is cleared by the next build. A directory describing hundreds of configurations will still take hundreds of compiles to check.

Completion (Ctrl+Space) offers:

* inside `{{ ... }}`, the names in scope, or the children of the path typed so far;
* after `$include:` or `$include_optional:` — inline or as items of a list beneath it — the definitions visible from that file (its own directory's and every directory's above it, the deeper ones overriding) at the level the path has reached: `templates.` offers what `templates` holds. A file inside `_schema/` is offered the schema's own `_definitions`. Only the directory is read for this, so a path that does not resolve yet does not stop its own completion;
* on an item of a `$keys` list, the keys of the mapping its `$select` names;
* on a field, or an item of a list, whose schema lists allowed values, those values — including an item that is still empty (`- `).

---

## Example

In `/example` there is an example COAL configuration project. Run `coal` in that directory to see the output it creates, and `coal -s staging` to see it built for one environment. The output is plain YAML and can be read by any YAML parser.

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

**Syntax.** The argument is `path=value`. The path is dot-separated and starts at the root. The value is everything after the first `=`, so values may themselves contain `=` (e.g. query strings). Missing intermediate objects along the path are created automatically, and if the target already exists its previous value — even a whole sub-tree — is replaced.

**Typing.** The value is parsed with the same rules as a YAML scalar: `true`/`false` become booleans, whole numbers become integers, decimals become floats, and everything else stays a string. So `-p service.port=8080` sets an integer. When `--validate` is used, parameters are validated against the schema like any other field (reported with an origin of `<command-line>`).

**Precedence.** A parameter behaves exactly as if its value had been written in the configuration at that path before the build started. It is *not* given any special authority: an `$include` or a structure layer can still change it, just as they could change a value declared in the files.

### Parameters drive the build

Because a parameter is seeded before includes and layering are processed, it can decide *what* gets built:

```yaml
# environment.yaml
dev
```

```yaml
# active.yaml
$include: "envs.{{ environment }}"
```

```
coal -p environment=prod
```

Here `environment` is set before the include phase, so `envs.prod` is what gets included. This is the same mechanism a `_structure.yaml` selection uses.

### Parameters and Jinja

Because parameters are seeded before Jinja templating, they integrate with it consistently:

1. **A parameter may contain a Jinja reference.** It is resolved against the final tree:

    ```
    coal -p log_directory="{{ base_directory }}/logs"
    ```

2. **References elsewhere see the parameter value.** If another field is `{{ base }}` and you set `base`, that field resolves to the parameter value, not the value originally in the file.

If a parameter's Jinja reference cannot be resolved (or forms a reference cycle), it is left in place as a literal string, exactly like any other unresolvable template.

---

## The Tree Building Pipeline

COAL processes configurations in a strict, multi-phase pipeline to ensure dependencies, structural morphing, and templates are resolved in the correct order.

### 0. Loading and seeding

The directory is read into a `CoalTree` (see [The Directory Is The Configuration](#the-directory-is-the-configuration)), and then any values supplied with `-p` or `-s` are seeded, so they can drive the rest of the build.

### 1-2. Expansion (includes, `_each`, directives, layering and references)

COAL then grows the tree until it stops growing. Each round:

* expands every `$include` whose path is known,
* merges every `_each` template into the fields it applies to,
* evaluates every structural directive whose target is known (`$extend`, `$append`),
* merges the structure layers, once the selection has settled,
* resolves every value whose references are resolved.

Anything that is not ready is left exactly where it is and tried again next round. This is what lets one file decide what another file reads:

```yaml
$include:
  - "environments.{{ environment }}"    # environment.yaml holds "production"
  - "modes.{{ mode }}"                  # mode arrives with the first include
```

`{{ mode }}` cannot be read on the first round — it arrives inside what the first path brings in. So the include waits, the round completes, and the next one reads it. The rounds end when one of them changes nothing.

A directive that never becomes resolvable is a **fatal error** naming the reference it was waiting for. An `$include_optional` that never becomes resolvable is simply skipped, which is what optional promises.

The structure is merged exactly **once**, and everything it brings in is expanded by the rounds that follow — so a layer may itself hold includes, `_each` templates and references.

That merge also splits the rounds in two. Until it has happened the tree is not yet the configuration being built, so a reference that reaches outside the file it is written in — and the `$inherit` and `$ignore` paths written with them — is held back and evaluated in the rounds that follow it. Only what a file says about itself resolves before the merge, because no layer can change that. See [When a name is read](#when-a-name-is-read). With no structure, or with none selected, there is no merge to wait for and everything resolves as soon as it can.

ID directives resolve at two points: `$id` during the directive phase, and `$id_after` at the very end, once the full tree is built and all references are resolved.

### 3. Validation

If validation is enabled, COAL reads the `_schema/` directory — which is shaped like the configuration it describes — and applies it to the root of the fully built tree, after every reference that can be resolved has been, so a schema always sees finished values. The schema therefore describes **the merged result** of a selection, not the source layout.

With nothing selected and a structure present, this phase runs once per configuration the structure describes. The directory is read and parsed a single time, the rounds that run before the merge are done a single time, and the tree is copied from there for each configuration — so the cost of checking the whole matrix is the merging and what follows it, not the file system.

A schema **describes** the configuration; it never writes to it. It enforces types, works out allowed values, evaluates `$template` blocks and marks invalid fields; a field the schema declared and the configuration never provided is reported where the schema asked for it, without a node being created for it. Compiling with and without `--validate` therefore produces exactly the same configuration.

### 4. Final Cleanup

Before outputting the final YAML, COAL traverses the tree one last time and scrubs any keys that start with `$`. This ensures all internal system directives are hidden from the final compiled configuration file.

### 5. Alphabetical Ordering

As a final step, COAL sorts the entire tree alphabetically by key. The compiled YAML output is therefore always printed in alphabetical order by default, regardless of the order keys were declared in or which files they were merged from. List items are the exception: they keep their numeric order, so item `10` never jumps in front of item `2`.

---

## Cycle Detection

COAL guards every recursive phase against directives that reference each other in a loop:

* **`$include` cycles** (an entry of the namespace that includes an entry that includes the first) are a **fatal error**, naming the path at which the chain became too deep.
* **Directory cycles** (a directory linked into itself) are a **fatal error**, naming the directory.
* **`$inherit` cycles** (two objects that derive from each other) are a **fatal error**, naming the object where inheritance nested too deeply.
* **`$extend` / `$append` cycles** (directives that keep re-creating themselves) are a **fatal error**, naming the node with the runaway directive.
* **Jinja reference cycles** (a field whose `{{ ... }}` resolves, directly or transitively, back to itself) are **not fatal**. Such a field never becomes resolvable, so the passes simply stop making progress: the template is left in the output as written, and a warning naming the field is printed to `stderr`. The same warning reports a template that fails for any other reason — a name that does not exist, a filter applied to the wrong type — with the engine's own explanation.

---

## System Directives (`$` Functions)

COAL reserves keys starting with `$` for system operations. Most of what earlier versions did with directives is now said by the directory layout instead; what remains is the handful of things a layout cannot express.

### What moved where

| Was | Is now |
| --- | --- |
| `$each` | an [`_each`](#_each-said-once-for-every-sibling) file or folder |
| `$include_directory`, `$include_subdirectories` | the directory layout itself |
| `$include: ./file.yaml` | `$include: <name>` into the [`_definitions`](#_definitions-shapes-with-a-name) namespace |
| `$override` | a layer in [`_structure.yaml`](#_structureyaml-selecting-a-configuration) |
| `$select`, `$keys` | a `$template` in the schema, or a structure selection |
| `$schema` (schema mixin) | [`$inherit`](#inherit---inherit) by path within `_schema/` |
| `$declare`, `$use` (schema) | `_schema/_definitions/` plus [`$include`](#include-a-definition---include) |
| `_schemas/` | `_schema/` — it is the schema of the whole configuration |
| `$any` (schema) | [`_each`](#every-key---_each--each) beside the field |
| `$keyed_by` (schema) | a `$template` writing one definition per permitted key |

`$inherit` survives, with the same meaning it always had, for [deriving one object from another](#inherit-deriving-from-a-value). What it no longer does is describe how the configuration as a whole is layered — that is `_structure.yaml`'s job, said once for the whole build instead of once per field.

### `$include` / `$include_optional`

Pull a piece of the `_definitions` namespace into the tree. Covered in full under [`_definitions`: shapes with a name](#_definitions-shapes-with-a-name).

### `$inherit`

Derive an object from another one, named by a path that may read the object's own values. Covered in full under [`$inherit`: deriving from a value](#inherit-deriving-from-a-value).

### `$ignore`

Leave fields out of the configuration. The names are read **relative to the object the list is written on**, so `$ignore` in a directory's `_self.yaml` names that directory's own entries:

```yaml
# environments/_self.yaml
$ignore:
  - old_env
  - retired_env
```

```
environments/
├── _self.yaml
├── production.yaml
├── old_env.yaml        never reaches the output
└── retired_env/        nor does this
```

A name that **matches nothing is not an error**. The point of the list is to say which of the things that might be there should not be built, and a configuration that has already dropped one should not have to keep the list in step.

A single name needs no list, and a dotted name reaches further down:

```yaml
$ignore: scratch
```

```yaml
$ignore:
  - environments.staging
```

Names may be worked out from the configuration, so a build can decide what to leave out:

```yaml
$ignore:
  - "{{ mode }}_only"
```

`$ignore` is applied **after `_each`**, so a field is dropped whether or not a template had something to say about it. The list is not consumed either, so a field arriving later — from an `$include`, or a structure layer — is dropped too. Written inside an `_each`, the list applies to every sibling separately:

```yaml
# regions/_each.yaml
$ignore:
  - scratch          # every region loses its own scratch
```

**A name given outright is applied while the directory is being read**, so the folder is never opened: nothing inside it is parsed, and it does not have to contain valid YAML. That holds for a dotted name and for one an `_each` template supplied, which is why the template is read before the level's own entries.

A name that **reads a value** cannot be, because at that point most of the configuration does not exist yet — honouring the ones that happened to resolve would make what gets built depend on the order the file system handed the entries back in. Those are applied once the tree is built, so the files under such a folder are read first and do have to be valid YAML. The same goes for a list arriving from an `$inherit` or a structure layer.

### List Directives - `$append` and `$extend`

Both add items to the end of a list. `$append` adds items written inline; `$extend` adds the items of lists elsewhere in the configuration, named by path. They may be used together, and the list is renumbered afterwards so it stays gapless.

**Configuration**

```yaml
base_sources:
  - alpha
  - beta
extra_sources:
  - gamma

combined:
  $append:
    - first
  $extend:
    - base_sources
    - extra_sources
```

**Compiled Output**

```yaml
combined:
  - first
  - alpha
  - beta
  - gamma
```

### Context Variables

| Variable | Description |
| --- | --- |
| `$id` | Generates a sequentially incrementing integer, resolved during the directive phase. |
| `$id_after` | Generates a sequentially incrementing integer, resolved **at the very end** of the pipeline. |

The key a node was written under and the file it came from are read with the `key()` and `file()` functions, described under [Jinja Templating](#jinja-templating):

**`environments/production/servers/web_server.yaml`**

```yaml
name: "{{ key() }}"                 # the field's own key
server: "{{ parent().key() }}"      # the object it lives in
environment: "{{ parent(3).key() }}"
```

**`Compiled Output`**

```yaml
environments:
  production:
    servers:
      web_server:
        name: name
        server: web_server
        environment: production
```

### ID Resolution Timing

Both ID variants share the same counter pool: a counter name like `"db_counters"` keeps incrementing no matter which variant references it. The counter name is a template like any other value (e.g. `$id: "{{ pool_name }}_pool"`), resolved before the counter is looked up. So `$id: "{{ parent(2).key() }}"` gives each holder its own counter. The difference between the two variants is *when* the value is locked in:

* **`$id`** resolves during the directive phase, before anything is copied. A tag inside an `_each` template is therefore resolved once, and every field the template is copied into shares that one number.
* **`$id_after`** resolves at the very end, once the whole tree is built and all Jinja templates are resolved. The tag survives copying, so **every occurrence** — including every copy made by an `_each` template or a structure layer — receives its own unique number.

```
services/
├── _each/
│   └── _self.yaml
├── api.yaml
└── web.yaml
```

**`services/_each/_self.yaml`**

```yaml
shared_pool:
  $id: "pool"        # resolved before copying -> the same for every service
own_pool:
  $id_after: "pool"  # resolved at the end -> unique per service
```

**`Compiled Output`**

```yaml
services:
  api:
    shared_pool: 1
    own_pool: 2
  web:
    shared_pool: 1
    own_pool: 3
```

---

## Jinja Templating

COAL embeds a complete Jinja2 engine. Anything Jinja can express is available in a configuration value, in a directive argument, and in a schema definition: filters, tests, arithmetic, comparisons, `{% for %}` and `{% if %}` blocks, `{% set %}`, and macros.

### What a name means

A name is looked up at the **top level of the file the template is written in**, and at the **configuration root** if that file does not answer for it. A file can therefore talk about its own fields without knowing where in the tree it was mounted, and still reach everything else.

```yaml
# paths.yaml, mounted as `paths`
base: /srv
logs: "{{ base }}/logs"      # this file's own `base` -> /srv/logs
```

"Does not answer for it" is decided by the **whole path**, not just the first name. A file that declares `paths` but no `paths.log_dir` is not what `{{ paths.log_dir }}` is asking about, so the lookup moves on to the root:

```yaml
# services/api.yaml
paths:
  socket: /run/api.sock
socket: "{{ paths.socket }}"    # this file's paths -> /run/api.sock
log:    "{{ paths.log_dir }}"   # not here, so the root's paths -> /var/log
```

`root()` skips the file outright and reads the configuration root, whatever the file happens to declare:

```yaml
forced: "{{ root().paths.log_dir }}"
```

To reach something relative to where the template sits, say so. These functions describe the template's own position:

| Written | Reads |
| --- | --- |
| `{{ parent().port }}` | the `port` beside this field — `parent()` is the object the field lives in |
| `{{ parent(3) }}` | the object three levels up, as a mapping |
| `{{ root() }}` | the configuration root, as a mapping |
| `{{ key() }}` | this field's own key |
| `{{ parent(2).key() }}` | the key of the object two levels up |
| `{{ file() }}` | the file this field was written in, without its extension |
| `{{ path() }}` | that file's path |

`parent()` with nothing in the brackets is `parent(1)`. Counting is always explicit, so a reference says exactly which enclosing object it means:

```yaml
# outer/inner/_self.yaml
name: inner_name
own_object: "{{ parent().name }}"      # inner_name
grandparent: "{{ parent(2).name }}"    # outer_name
from_root: "{{ root().name }}"         # root_name
which_field: "{{ key() }}"             # which_field
which_object: "{{ parent().key() }}"   # inner
written_in: "{{ file() }}"             # _self
```

This is what a file dropped into a directory uses to talk about itself. It does not know the key it will be mounted under, so it asks:

```yaml
# processes/encoder.yaml, loaded as processes.encoder
name: "{{ parent().key() }}"       # encoder
log: "{{ paths.log_dir }}/{{ file() }}.log"
```

A copy carries its file with it: a template copied out of an `_each` folder, or pulled in by a structure layer, resolves against that file as it sits **where the copy landed**.

### When a name is read

A configuration with a [`_structure.yaml`](#_structureyaml-selecting-a-configuration) is *assembled* before it is read: the selection is made, the layers are merged, and only then is there a configuration for a reference to be about. So references resolve in **two moments**.

| Written | Read |
| --- | --- |
| a bare name the file answers for | straight away — the file is already whole |
| a bare name the file does not answer for | after the structure has merged, from the configuration that was built |
| `root().anything` | after the structure has merged |
| `parent().anything` | after the structure has merged |

Nothing a layer contributes changes what a file says about itself, which is why the first row need not wait. Everything else does, because the merge is precisely what decides it:

```yaml
# log_level.yaml holds "info"
# environments/production/_self.yaml says log_level: error

# environments/production/banner.yaml
line: "level={{ log_level }}"     # level=error, the merged value
```

`$inherit` and `$ignore` name what they act on with the same references, so they follow the same two moments: whichever of them cannot be worked out from the file it is written in is left where it is and evaluated once the configuration has been assembled.

A configuration with no `_structure.yaml` is never rearranged, so there is nothing to wait for and everything resolves at once.

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

Templates are rendered in repeated passes (see [The Tree Building Pipeline](#the-tree-building-pipeline)), and a field is only rendered once every field it reads is itself resolved. A chain of references therefore resolves regardless of the order it is written in, and it never matters which file each link came from.

```yaml
last: "{{ middle }}/last"     # /start/middle/last
middle: "{{ first }}/middle"  # /start/middle
first: /start
```

Waiting is per field, not per object: `{{ parent().port }}` waits for `port` alone, never for the rest of the object it lives in. That holds when `port` is not there at all — a field the object does not have is nothing to wait for, not a reason to wait for the whole object. Two siblings that each read a name their parent does not have therefore resolve independently, rather than each waiting on the other and being reported as a loop. Reading a whole enclosing object — `{{ parent() | length }}` — is the one place this bends. Such a field waits for the rest of its container as usual, but once nothing else can be resolved it is allowed to look at the container as it stands, since waiting for itself is the only thing left to wait for.

### A field that may not be there

A reference to a field that does not exist is an error, because a value quietly going missing is nearly always a mistake. When it is not — an optional field, one only some of the objects an `_each` covers will carry — say so with a fallback, and Jinja's ordinary ways of doing that all apply:

```yaml
nodes:
  cop01:
    node_type: control
    label: "{{ parent().node_value | default('') }}"        # '' when absent
    site: "{{ parent().site_name | default('unknown') }}"
    fetched: "{{ parent().get('node_value', 'absent') }}"
    asked: "{{ parent().node_value is defined }}"           # false
```

A guarded reference resolves to its fallback and is not reported. Without one, the same line is left in the output as written and reported as the missing field it names — `it reads 'nodes.cop01.node_value', which does not exist` — never as a loop between the fields that share the habit. The same goes for `root().anything` and for a plain dotted name.

A guard means the same thing **at any depth**. It is usually not the last name in a path that is missing but one in the middle — an optional object that some of the things an `_each` covers simply do not have — and a fallback covers that too:

```yaml
port: "{{ node.telemetry.port | default(9000) }}"    # no telemetry at all: 9000
has: "{{ node.telemetry.port is defined }}"          # false
```

Only a template that writes a guard is read this way. One that does not is unchanged: an absent name is an error wherever it appears, in a comparison or a filter as readily as in the output. And in a guarded template, a read the guard does not cover is still reported — `{{ a.b | default(1) }}{{ c.d }}` says what it thinks of `c.d`.

### Calling a method on a container

`services.keys()`, `nodes.values()`, `paths.items()` call a method on the value named to their left. There is no field called `keys` — the name being read is `services` — so such a line waits for the whole container and is reported against it. That matters most when the field is written *inside* the container it reads:

```yaml
# nodes/_each/_self.yaml
label: "node-{{ parent().key() }}"
peers: |
  {%- for peer in nodes.values() %}
  - {{ peer.label }}
  {%- endfor %}
```

`peers` reads every node, including its own, and every node is still working out its `label`. It is the [enclosing read](#order-does-not-matter) described above: it waits for the rest of the container as usual, and once nothing else can move it goes ahead against the container as it stands — by which time every `label` is settled.

A field whose answer would *contain* its own reference is refused rather than resolved, and reported like any other unresolved template:

```yaml
snapshot: "{{ parent() }}"   # the answer would include this very line
```

A field that can never be resolved — a reference cycle, a name that does not exist, a filter applied to the wrong type — is left in the output exactly as written and reported as an error. The configuration is still printed, because the fields that did resolve are still worth having while the reference is being fixed, but `coal --validate` fails and the language server marks the line.

### One error per cause

One field that cannot resolve stops every field that reads it, and every field that reads *those*. Reporting each of them separately buries the one that matters, so COAL reports the head of the chain and lists the rest underneath it.

```yaml
# env/base.yaml
deployment:
  name: prod
  base_dir: "{{ deployment.root_dir }}/{{ deployment.name }}"

# paths.yaml
paths:
  root: "{{ deployment.base_dir }}/app"
  log: "{{ parent().root }}/service.log"
  archive: "{{ parent().log }}.gz"
```

```
Error: template: deployment.base_dir -> it reads 'deployment.root_dir', which does not exist [env/base.yaml, line 3, col 3]
  3 other fields are left unresolved by it:
    paths.root
    paths.log (through paths.root)
    paths.archive (through paths.log -> paths.root)
```

There is one thing to fix here, in one file, and the error names it: `deployment.root_dir`. The fields listed underneath are consequences, and resolve on their own once the head does. Only the head is reported to the editor, so the language server underlines the line that has to change and nothing else.

An error always names the reference that is actually missing rather than reporting that something was, and a set of fields that reference each other in a circle is reported once as the loop it is:

```
Error: template: loop_a -> it is part of a reference loop: loop_a -> loop_b -> loop_a [_self.yaml, line 7, col 1]
```

### Templates in directives

Directive arguments are templates too, so a single value can decide what gets built:

```yaml
environment: prod
active:
  $include: "envs.{{ environment }}"
tier: "{{ 'high' if environment == 'prod' else 'low' }}"
```

This works for `$include` and `$include_optional` paths, for `$extend` targets, for `$id`/`$id_after` counter names, and for the `layers` of a `_structure.yaml` selection.

---

## Schema Engine & Validation

The schema engine strictly defines what the compiled YAML is allowed to look like.

### The schema directory mirrors the configuration

`_schema/` is laid out exactly like the configuration it describes: a **file** named after a field describes that field, a **folder** is a level of nesting, **`_each`** describes every key of the object holding it, and **`_optional`** holds the fields that are permitted without being required. The root of `_schema/` is the schema of the root of the configuration, so there is no entry file to name.

```
_schema/
├── _self.yaml               fields of the root itself
├── environment.yaml         describes `environment`
├── paths.yaml               describes `paths`
├── _optional/
│   └── log_level.yaml       permitted, not required
├── _definitions/
│   └── host.yaml            a shape that is not a field
└── processes/
    ├── _each.yaml           describes every process
    └── _each/
        └── _optional/
            └── arguments.yaml
```

A file whose whole content is a type describes a field of that type, so a one-line schema really is one line:

**`_schema/environment.yaml`**

```yaml
string
```

**`_schema/paths.yaml`**

```yaml
log_dir: string
data_dir: string
```

**`_schema/processes/_each.yaml`**

```yaml
executable: string
$optional:
  arguments: "[string]"
```

`_each` written inline is the same thing as the folder, which is often easier to read for a small object:

```yaml
# _schema/_self.yaml
processes:
  $each:
    executable: string
```

### A schema describes what comes *out* of a selection

When `_structure.yaml` selects something, the schema describes **the merged result** — not the source layout. Building a node produces a configuration with no `environments` field, so `_schema/` has no `environments` in it either. What the schema sees is what the build actually produced.

Findings still point at the file the value came from, which is very often *not* the selected thing's own file:

```
staging:
  [environments/staging/_self.yaml] processes.encoder.tuning.threads
  type: string -> expected integer at [environments/staging/_self.yaml, line 10, col 7]
```

The field is `processes.encoder.tuning.threads` in the merged configuration; the mistake is in the environment that was layered in, and that is where the error points.

### Where a missing field is reported

A field the schema asked for and the configuration never provided is unlike every other finding: there is no line to point at, because the mistake is an absence. So the report goes to the nearest place the field could actually be written.

| The object that should have held it | Reported in |
| --- | --- |
| a `.yaml` file | that file |
| a folder with a `_self.yaml` | that `_self.yaml` |
| a folder without one | the `_schema` line that asked for the field |

A folder has no line of its own, and the fix there is to *create* a file rather than edit one — so the finding goes to the one place that always exists and always has a line, and the message says where the field would go:

```
[_schema/nodes/_each.yaml] nodes.cop01.hostname [_schema.nodes.$each.hostname]
value: None -> expected string -> write it in 'nodes/cop01/_self.yaml' or 'nodes/cop01/hostname.yaml'
```

The second bracket is the other side of the story either way: the schema that asked for the field when the finding sits on the configuration, and the path through the schema when it already sits on the schema.

### Validating every configuration at once

A configuration that describes many things has to be right for **all** of them: a mistake in one node's environment says nothing about another's. So `coal --validate` with nothing selected compiles every selection the structure allows and checks each.

What is checked is always the **merged** configuration, never the files as written. A node inherits most of what it is from its environment and its type, and none of that exists until the layers have merged; a part the structure drops is in no configuration at all and is not checked.

One mistake is reported once, wherever it is written:

```
Checked 5 configurations.

Found 1 problem:

environments/staging/_self.yaml:4:3  InvalidType
  workers -- type: string -> expected integer
  in 2 of 5 configurations, whenever environment = staging
```

The line under the finding is worked out from the selections themselves. Where every affected configuration makes some selection and no unaffected one does, that selection is what the finding follows and is named; where several are needed together they are named together; and where the affected configurations have no selection in common they are listed by name instead.

Reading and parsing the directory is done **once**, and so is everything the build can do before the structure merges — a file's own includes, its `_each` templates, and every reference that stays inside it. None of that can see a selection, so the result is the same for all of them and the tree is copied from there. Nothing past the merge is shared, and deliberately so: the structure merges *before* the templates that read across files are rendered, precisely so a field reads the value its layers settled on, and a further-resolved tree would have frozen one selection's answers into all of them.

Narrow what is checked by pinning a capture — `coal -v -p environment=production` checks only the nodes in production — or check exactly one with `coal -v -s cop01 -s operational`.

### Basic Types & Lists (`[]`)

You can define a field's required type using simple strings: `string`, `integer`, `float`, `number`, `bool`, `list`, `object`, or `any`. To enforce that a list only contains a specific type, wrap the type in brackets (`[]`).

**`_schema/_self.yaml`**

```yaml
server_name: string
ports: [integer]  # Must be a list of integers
tags: [string]    # Must be a list of strings
```
##### Valid:
**`Configuration`**

```yaml
server_name: "prod-01"
ports: [80, 443]
tags: ["web", "frontend"]
```
---

### Advanced Schema Directives

#### Inherit - `$inherit`

Deep-merges another part of the schema tree into this one, named by its path within `_schema/`. It is the same word, and the same behaviour, that a configuration uses.

**`_schema/replica.yaml`**

```yaml
$inherit: primary     # every field _schema/primary.yaml declares
$optional:
  lag_seconds: integer
```

What the block says for itself wins over what it inherits, and a chain is followed to the end.

Because the schema directory is a mirror, a path names a shape that also describes a real field. For a shape that is *not* a field of the configuration — one that exists purely to be reused — put it in `_schema/_definitions/` and pull it in with `$include`, below.

#### Include a definition - `$include`

A shape that is not a field of the configuration lives in `_schema/_definitions/` and is pulled in with `$include` — the same folder and the same directive a configuration uses, doing the same job.

**`_schema/_definitions/ip_config.yaml`**

```yaml
address: string
gateway: string
```

**`_schema/_self.yaml`**

```yaml
node_1:
  $include: ip_config
```

A definition is merged **underneath** the block that includes it, so fields written next to `$include` stay valid and win where both define the same field:

```yaml
node_1:
  $include: ip_config
  vlan: integer      # beats whatever the definition said about vlan
```

`$include` accepts a list, merged left to right:

```yaml
node_1:
  $include:
    - addressing
    - naming
```

A definition does not have to be an object. When it is a type name or a list of allowed values, including it simply *is* that definition:

**`_schema/_definitions/port_number.yaml`**

```yaml
integer
```

**`_schema/_definitions/speed.yaml`**

```yaml
- fast
- slow
```

```yaml
port:
  $include: port_number
mode:
  $include: speed
```

Definitions are resolved when the schema directory is read, and again for schema a `$template` writes — so a template may include a definition too. A name the namespace does not have is a fault in the schema, and the build stops rather than validating against something it could not assemble.

`$inherit` and `$include` differ only in where they look: `$inherit` names another part of the schema tree, `$include` names a definition. Both merge underneath, both accept a list.

**A definition may be built from other definitions**, and may carry anything a schema block can — `_optional` included:

```
_schema/_definitions/
├── addressing.yaml       address: string
├── naming.yaml           hostname: string
└── host/
    ├── _self.yaml        $include: [addressing, naming]
    └── _optional/
        └── vlan.yaml     integer
```

```yaml
# _schema/_self.yaml
node_1:
  $include: host        # address, hostname, and an optional vlan
```

The same holds the other way round: an `_optional` block may include a definition, and everything the definition brings is then optional too.

#### Every key - `_each` / `$each`

The definition every key of an object must match, whatever the keys turn out to be called. It is what a schema folder named `_each` becomes, and it can be written inline as `$each` wherever that reads better.

**`_schema/process/_each.yaml`**

```yaml
exec: string
args: [string]
```

or, saying the same thing in one file:

**`_schema/_self.yaml`**

```yaml
process:
  $each:
    exec: string
    args: [string]
```
##### Valid:
**`Configuration`**

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

**`_schema/_self.yaml`**

```yaml
name: string
$optional:
  description: string
```

`_optional` is the directory spelling of the same thing — a folder or a file, wherever a schema block can be written:

```
_schema/
├── _self.yaml            name: string
└── _optional/
    ├── description.yaml  string
    └── nickname.yaml     string
```

or, in one file:

**`_schema/_optional.yaml`**

```yaml
description: string
nickname: string
```

Optional does not mean unchecked: a field inside `_optional` still has to match what it says when it is there.

It nests like everything else. `_optional` inside an `_each` gives every key of an object the same optional fields, and a field inside `_optional` may have an `_optional` of its own:

```
_schema/_optional/telemetry/
├── _self.yaml            endpoint: string     required, if telemetry is there at all
└── _optional/
    └── port.yaml         integer
```

That is also the answer when a field is optional *and* has optional fields of its own: describe it inside the enclosing `_optional`, and give it an `_optional` of its own. There is one way to say a field is optional — the enclosing object says it — so a field's own schema never has to be read to find out whether the field has to be there.
##### Valid:
**`Configuration`**

```yaml
name: "Server 1"
```
##### Valid:
**`Configuration`**

```yaml
name: "Server 2"
description: "Valid server"
```

`$optional` is a full schema block rather than a plain list of fields, so it may carry `$include`, `$each` and `$template` of its own. Anything it brings in is optional too:

**`_schema/_definitions/extras.yaml`**

```yaml
description: string
nickname: string
```

**`_schema/_self.yaml`**

```yaml
name: string
$optional:
  $include: extras
  labels:
    $each: string
```

### Allowed Values from the Configuration

A schema field may be a Jinja template instead of a type name. Whatever list the template produces *is* the set of values the field may hold. Schemas read the configuration exactly as configuration files do: a name is a top-level field, and `parent()` is the object being validated — so a schema can constrain one field by another field of the same object.

**`_schema/_self.yaml`**

```yaml
clusters: "[string]"
assigned_cluster: "{{ clusters }}"
```
##### Valid:
**`Configuration`**
```yaml
clusters:
  - "prod"
  - "staging"
assigned_cluster: "staging"
```

##### Invalid:
**`Configuration`**
```yaml
clusters:
  - "prod"
  - "staging"
assigned_cluster: "test"
```

Because it is ordinary Jinja, the schema says exactly which values it means. A mapping has two sides, so name the one that constrains the field:

```yaml
transports:
  $each:
    hosts: "[string]"
link:
  transport: "{{ transports.keys() }}"          # one of the declared transports
  host: "{{ transports[parent().transport].hosts }}"     # one of *that* transport's hosts
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

Where a template like this constrains the *values* of a field, a `$template` block constrains its *keys*: write out a definition per key the configuration is allowed to carry, and any other key is flagged.

```yaml
regions: "[string]"
deployments:
  $template: |
    {%- for region in regions %}
    {{ region }}:
      replicas: integer
    {%- endfor %}
```

Wrapping the template in brackets constrains every element of a list instead of the field itself, so `assigned_clusters: "[{{ clusters }}]"` requires a list whose items are each one of the referenced values.

### Schema from the Data (`$template`)

A schema block may write part of itself. `$template` is an ordinary Jinja template whose *output is schema*, rendered against the configuration being validated and then applied on top of the block that carries it.

It reads the data the way the field definitions beside it do: a bare name is a top-level field, and `parent()` is the object being described.

**`_schema/_self.yaml`**

```yaml
type: string
$template: |
  {%- if parent().type == 'database' %}
  db_port: integer
  db_password: string
  {%- endif %}
```
##### Valid:
**`Configuration`**

```yaml
type: "database"
db_port: 5432
db_password: "secure"
```
##### Invalid:
**`Configuration`**

```yaml
type: "database"   # db_port and db_password are now mandatory and missing
```

Because it is a template rather than a fixed set of cases, `and`, `or`, `not`, comparisons, membership, loops and filters all work, and several decisions can live in one block:

```yaml
$template: |
  {%- if parent().type in ['database', 'cache'] and parent().port > 1024 %}
  credentials: string
  {%- endif %}
  {%- if environment == 'production' %}
  replicas: integer
  {%- endif %}
  {%- for region in regions %}
  {{ region }}_endpoint: string
  {%- endfor %}
```

What it writes is a full schema block, so it may declare fields, `$optional`, `$each`, `$include` — anything a schema can say. A template that decides nothing applies simply produces nothing.

---

### Nesting the object directives

`$inherit`/`$include`, `$optional`, `$template` and `$each` all describe the *same* node, and each of them accepts a full schema block. They can therefore be nested inside one another to whatever depth the configuration calls for:

**`_schema/_definitions/tuning_block.yaml`**

```yaml
cache_size: integer
$optional:
  swap: bool
```

**`_schema/_self.yaml`**

```yaml
kind: string
$template: |
  {%- if parent().kind == 'database' %}
  db_port: integer
  $optional:
    tuning:
      $include: tuning_block
  {%- endif %}
```

Optionality is inherited: the `notes` field above sits inside an `$optional`, so the conditional that declares it grants it without ever requiring it.

Any `$` key the schema engine does not recognize is reported as a schema error rather than skipped, so a typo — or a directive under its old name — surfaces instead of quietly doing nothing.
