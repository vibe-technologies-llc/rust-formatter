# Configuration

How `rust-formatter` decides what to do, and where each answer can come from.
Every rule below is the behaviour of the current implementation.

The tool stays zero-config: with no configuration at all it formats Rust with
`StdExternalCrate` import grouping and `Crate` import merging, and TOML at its
documented defaults. Nothing here writes a file into a project — a repository
states its settings in a table cargo already reads, or in a file it commits on
purpose.

## Contents

- [Precedence](#precedence)
- [Where settings live](#where-settings-live)
- [The settings](#the-settings)
- [Presets](#presets)
- [Overriding a setting for one run](#overriding-a-setting-for-one-run)
- [Seeing what applied](#seeing-what-applied)
- [How values combine](#how-values-combine)
- [Relationship to rustfmt.toml](#relationship-to-rustfmttoml)
- [What is not configurable](#what-is-not-configurable)
- [Errors](#errors)

## Precedence

Lowest first. A setting given higher up wins.

| | Source |
| --- | --- |
| 1 | built-in defaults |
| 2 | `rust-toolchain.toml` (or `rust-toolchain`), which sets `toolchain` and nothing else |
| 3 | `[workspace.metadata.rust-formatter]` in the workspace manifest |
| 4 | `[package.metadata.rust-formatter]` in the package manifest |
| 5 | a discovered `rust-formatter.toml` or `.rust-formatter.toml` |
| 6 | `--config-file FILE` |
| 7 | `RUST_FORMATTER_*` in the environment |
| 8 | flags typed on the command line |

A **preset applies immediately below the layer that named it**. Naming one
therefore never discards the keys that layer also set, and `--preset` on the
command line outranks every file:

```toml rf:fixed-point --toml-align-comments
# rust-formatter.toml
preset = ["cargo"]  # applies under this file
toml-max-width = 70 # and this still wins over the preset's 100
```

`--no-config` skips layers 2 to 6 outright. It cannot be combined with
`--config-file`, which exists to name one.

### The toolchain pin

rustup and cargo both read `rust-toolchain.toml`, so a checkout that carries one
has already said which toolchain its tooling should use, and layer 2 is that
answer reaching rustfmt as well. `rust-toolchain.toml` is read for the
`channel` of its `[toolchain]` table. The legacy extensionless `rust-toolchain`
may carry the same table, or its first line may be a bare channel name; that
fallback belongs to the legacy file alone, so a `rust-toolchain.toml` that does
not parse as TOML contributes nothing even when it holds a bare name. Neither
does a file whose `[toolchain]` table has no `channel`. A file that contributes
nothing is passed over and the search goes on: rustup reports its own malformed
file, and a formatter that refused to run over one would be reporting someone
else's problem twice.

It is the lowest layer above the defaults, so anything this tool owns outranks
it. A repository that pins `stable` for its build but wants nightly formatting
says so once:

```toml rf:fixed-point
# rust-formatter.toml
toolchain = "nightly"
```

The search stops where the settings-file search stops -- at the workspace root,
else at the repository boundary -- rather than at the filesystem root rustup
would walk to.

## Where settings live

### Cargo metadata

The zero-pollution spelling: a table cargo already ignores, in a manifest the
repository already has.

```toml rf:fixed-point
# Cargo.toml
[workspace.metadata.rust-formatter]
toolchain = "nightly-2026-06-01"
exclude = ["vendor/**"]
sort-deps = true

[workspace.metadata.rust-formatter.config]
max_width = 120
```

Both tables are read: the workspace one from the workspace root's manifest, the
package one from the nearest manifest above the path being formatted.

Settings are resolved **once per run**, from the first `PATH` argument, else
the directory of `--stdin-filepath`, else the current directory; a file argument
resolves from the directory that holds it. So a run from a workspace root reads
the workspace table and the root package's table; a run inside a member reads that member's as well.
Under a workspace-wide run, other members' `[package.metadata]` tables are not
read — one run has one set of settings.

### A file of its own

`rust-formatter.toml`, or `.rust-formatter.toml`, searched from the path being
formatted upwards. The search stops at the workspace root when there is one, and
otherwise at the repository boundary, so a checkout never inherits a file from
outside the project it belongs to.

```toml rf:fixed-point
# rust-formatter.toml
preset = ["cargo"]
exclude = ["vendor/**", "third_party/**"]
toml-max-width = 100

[config]
max_width = 120
imports_granularity = "Crate"

[presets.house]
sort-keys = true
toml-align-entries = true
```

`--config-file FILE` names one directly and outranks the discovered one.

### The environment

Every setting except `presets` reads one variable, named from the setting
itself: `sort-deps` is `RUST_FORMATTER_SORT_DEPS`, `toml-max-width` is
`RUST_FORMATTER_TOML_MAX_WIDTH`. There is no table to memorise — uppercase the
key and replace `-` with `_`. A preset is defined only in a file; the
environment can still name one with `RUST_FORMATTER_PRESET`.

A value is read as the type its own setting has, so `RUST_FORMATTER_EDITION=2024`
is the string `2024` rather than the number, and a list is written as a TOML
array:

```bash
RUST_FORMATTER_SORT_DEPS=1
RUST_FORMATTER_TOOLCHAIN=nightly-2026-06-01
RUST_FORMATTER_EXCLUDE='["vendor/**", "generated/**"]'
RUST_FORMATTER_CONFIG='{ max_width = 120 }'
```

Booleans accept `true`, `false`, `yes`, `no`, `on`, `off`, `1` and `0`, in any
case, in a file and in the environment alike. The command line accepts those and
also `y`, `n`, `t` and `f`.

## The settings

The key is the flag's own name without the dashes, so `--toml-max-width` is
`toml-max-width`. Flags whose name is already negative keep it: `--no-all` is
`no-all = true`, and `no-all = false` is the default.

**Rust**: `toolchain`, `edition`, `style-edition`, `rust-style`, `config` (a table
of rustfmt options), `unset-config`, `no-all`.

`toolchain` defaults to `auto`: `$RUSTFMT` if it is set, then rustup's `nightly`
if there is one, then rustup's active toolchain, then a `rustfmt` on `PATH`.
Naming one instead asks rustup for it directly, and falls back to `$RUSTFMT` or `PATH` with
a warning when there is no rustup to ask.

**Selecting files**: `include`, `exclude`, `ignore-path`, `max-depth`, `hidden`,
`no-ignore`, `skip-toml`, `no-default-toml-skips`, `languages`
(`both`, `rust`, `toml`).

**TOML layout**: `toml-indent`, `toml-tab-width`, `toml-max-width`,
`toml-arrays`, `toml-inline-tables`, `toml-version`, `toml-trailing-comma`,
`toml-blank-line-before-tables`, `toml-max-blank-lines`, `toml-array-spacing`,
`toml-inline-table-spacing`, `toml-align-entries`, `toml-align-comments`,
`toml-indent-tables`, `toml-indent-entries`, `toml-normalize-keys`,
`toml-directives`.

**TOML ordering**: `sort-deps`, `sort-package`, `package-order`,
`sort-dep-fields`, `sort-features`, `sort-arrays`, `sort-targets`, `sort-tables`,
`sort-keys`, `sort-grouped`, `cargo-conventions`.

**Execution and output**: `jobs`, `cache`, `fail-fast`, `offline`, `color`,
`message-format`, `diff-context`, `verbose`, `quiet`.

**Naming other settings**: `preset`, `presets`.

Values are spelled the way the flag spells them: `toml-arrays = "auto"`,
`toml-version = "1.0"`, `package-order = "style-guide"`. `toml-indent` takes a
number of spaces from 0 to 16 or the string `"tab"`.

The numeric settings have the same ranges wherever they are set -- on the command
line, in a file, in a `[presets]` table or in the environment: `toml-tab-width`
1 to 16, `toml-max-width` 1 to 4096, `toml-max-blank-lines` 0 to 32 and
`diff-context` 0 to 4096. A value outside its range is a configuration error
that names where it came from.

## Presets

`--preset NAME`, repeatable and layered. A preset is a named group of settings
covering both languages.

| Preset | What it sets |
| --- | --- |
| `default` | nothing |
| `cargo` (alias `style-guide`) | the Rust Style Guide's `Cargo.toml` chapter |
| `compact` | inline tables never wrap, so no TOML 1.1 construct is written |
| `expand` | any inline table with two or more keys wraps |
| `everything` | `toml-arrays = "auto"`, `toml-trailing-comma = "multiline"`, `toml-blank-line-before-tables`, `toml-normalize-keys`, `sort-deps`, `sort-package`, `sort-dep-fields`, `sort-features`, `sort-tables`, `sort-keys`, `sort-grouped` |
| `narrow-tabs` | tab indent, 60-column budget |
| `aligned-indented` | `toml-arrays = "expand"`, `toml-array-spacing = "spaced"`, `toml-inline-table-spacing = "compact"`, `toml-max-blank-lines = 2`, `toml-align-entries`, `toml-align-comments`, `toml-indent-tables`, `toml-indent-entries` |
| `comments` | rustfmt's comment options |
| `literals` | rustfmt's literal options |
| `strict` | `comments`, `literals`, plus `format_strings`, `reorder_impl_items` and `blank_lines_upper_bound = 1` |

`--style-guide` and `--rust-style NAME` remain as aliases for the corresponding
presets.

A configuration file may define presets of its own:

```toml rf:fixed-point
[presets.house]
sort-keys = true
toml-max-width = 120
```

A preset may not redefine a built-in name, and may not name another preset —
which is what removes any question of a cycle.

The corpus and property suites run every TOML document through each of the
seven TOML presets, so a TOML preset is exercised by the same checks the
defaults are.

## Overriding a setting for one run

Once a repository can turn a rewrite on, one run has to be able to turn it off.
Every boolean flag takes an optional value, and most have a hidden negative
spelling:

```bash
rust-formatter --sort-deps              # on
rust-formatter --sort-deps=false        # off, overriding a configured `true`
rust-formatter --sort-deps=0            # the same; 1/0, yes/no, on/off all work
rust-formatter --no-sort-deps           # the same
RUST_FORMATTER_SORT_DEPS=off rust-formatter
```

The last spelling on the command line wins, so a wrapper script may append
either without the two colliding.

The flags whose name is already negative -- `--no-ignore`,
`--no-default-toml-skips` and `--no-all` -- have no second negation; they are
turned off with `=false`. `--toml-directives` takes `on` or `off` rather than a
boolean, and `--style-guide` takes no value at all: it only names the preset.

## Seeing what applied

```bash
rust-formatter --print-settings
rust-formatter --print-settings --message-format json
```

Prints the settings the run resolves to, each with the layer it came from and
the environment variable that would override it. Anything absent is at its
built-in default. It runs before the flag-combination checks, so it still
describes a configuration that would be refused.

`-v` names the configuration sources that were read, in the order they applied.

`--print-config` is a different question: it asks *rustfmt* for its own resolved
configuration.

## How values combine

- **Scalars replace.** A higher layer's value is the whole answer.
- **Tables merge key by key.** A package can add one rustfmt option to the
  workspace's `[config]` without restating the rest.
- **Filter lists accumulate**: `include`, `exclude`, `ignore-path`, `skip-toml`,
  `unset-config` and `rust-style`. An exclude is a rule about the tree rather
  than an answer that can be superseded, which is also how rustfmt's own
  `ignore` list is folded into `--exclude`.
- **Presets accumulate.** Each layer's `preset` list applies just below that
  layer, so a preset named by a lower layer stays in force under a higher one
  that names another; the higher layer's presets and keys win where both set
  the same thing. A `[presets]` definition can be named by any layer, and a
  higher layer that defines the same name replaces the lower definition whole.

Narrowing the selection is what makes an empty result a successful no-op rather
than "there is nothing formattable here". Only the caller narrows: a repository's
`exclude` describes its tree, not this run, so it does not change the exit code.

## Relationship to `rustfmt.toml`

`rust-formatter` never writes into a project. The settings it resolves become
rustfmt `--config` arguments, and rustfmt ranks its command line above its own
configuration file — so a rust-formatter setting wins over a `rustfmt.toml` key
of the same name.

`edition` is the documented exception, because it is inferred rather than
chosen. An edition that came from the top-level `edition` key of a configuration
source describes a tree rather than a run, so it ranks below the project's own
`rustfmt.toml` and below each package's `edition`, and only replaces the
built-in default. A typed `--edition` outranks both.

An `edition` inside a `[config]` table is not that fallback: it is a rustfmt
option like the rest of the table, and applies the way a typed `--edition`
does, above `rustfmt.toml` and every package's `edition`. A typed `--edition`
that disagrees with it is refused. When a source also sets the top-level
`edition`, that value is the one applied, at the same rank.

A rustfmt option that *this* rustfmt does not have is treated by where it came
from. Typed on the command line it is a typo, and fatal. Named by a configuration
file it is a portability problem — the file is read by everyone who checks the
repository out, on whatever rustfmt they have — so it is dropped with a warning
and the run continues.

## What is not configurable

Settings describe how to format and what to walk. What a particular invocation
*does* stays on the command line, so a configuration file can never change what
a command means: `--check`, `--emit`, `--list-files`, `--list-different`,
`--stdin`, `--stdin-filepath`, `--range`, `--watch`, `--files-from`, `--since`,
`--staged`, `--restage`, `--no-untracked`, `--recurse-submodules`,
`--full-versions` and its options, `--registry-url`, `--install-toolchain`,
`--print-config`, `--print-settings`, `--config-file`, `--no-config`, the paths,
and the trailing `-- <rustfmt args>`.

The subcommands -- `completions`, `man` and `hook` -- read no settings at all.
They are wiring for something else, so a repository with a typo in its own
configuration can still be asked for a completion script or told to install a
hook. `--watch` is on that list for a sharper reason: a `rust-formatter.toml`
that could set it would turn `rust-formatter .` into a process that never exits,
for everyone who checked the repository out.

## Errors

A configuration problem exits `2` and names the file:

```
error: /repo/rust-formatter.toml: 3:1: unknown setting `sort_deps`; did you mean `sort-deps`?
error: /repo/Cargo.toml: [workspace.metadata.rust-formatter]: unknown setting `toml-maxwidth`; did you mean `toml-max-width`?
error: unknown preset `nope`.
Known presets: aligned-indented, cargo, comments, compact, default, everything, expand, literals, narrow-tabs, strict, style-guide
```

A flag combination that cannot be honoured is refused the same way whether it was
typed or configured, because the check runs on the settings the run resolved to.
A number outside its range is refused the same way too, naming the file, table
or variable it came from:

```
error: /repo/rust-formatter.toml: toml-max-width: `0` is not in 1..=4096
error: $RUST_FORMATTER_TOML_MAX_WIDTH: `0` is not in 1..=4096
```
