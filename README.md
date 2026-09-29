# rust-formatter

Formats Rust and TOML. Drives `rustfmt` without writing `rustfmt.toml` or changing the project's toolchain.

## Quick start

```bash
cargo install --locked --git https://github.com/vibe-technologies-llc/rust-formatter
rust-formatter            # format the workspace or directory you are in
rust-formatter --check    # print a diff and exit 1 if anything is unformatted
```

## Features

- **Standard Import Organization**:
  - **StdExternalCrate grouping**: Groups imports into `std` (and `core`/`alloc`), external third-party crates, and local `crate::` modules, separated by blank lines.
  - **Crate-level Granularity Merging**: Automatically merges individual imports into `use crate::{x, y};` instead of multiple separate `use crate::x;` and `use crate::y;` lines.
- **Zero Config Pollution**: Configures rustfmt via dynamic CLI flags. Never creates, modifies, or leaves behind any `rustfmt.toml` / `.rustfmt.toml` files. If a project already has `rustfmt.toml` / `.rustfmt.toml`, rustfmt still loads it. Keys passed via `--config` (including the default `group_imports` / `imports_granularity`) override that file; other keys still apply.
- **Layered Configuration**: A repository pins its style in a table cargo already ignores -- `[workspace.metadata.rust-formatter]` or `[package.metadata.rust-formatter]` -- or in a `rust-formatter.toml` it commits on purpose. Named presets, `RUST_FORMATTER_*` environment variables and a documented precedence order complete it, and `--print-settings` says where every value came from. See [Configuration](#configuration) and [docs/configuration.md](docs/configuration.md).
- **Zero Toolchain Conversion**: Resolves a rustfmt and runs it directly. Never modifies `rust-toolchain.toml` or the project's pinned compiler channel -- it reads the pin, as the lowest configuration layer, and honours it.
- **Any toolchain**: `--toolchain` defaults to `auto`, which takes `$RUSTFMT` first, then rustup's `nightly`, then rustup's active toolchain, and then a `rustfmt` on `PATH` -- so a Nix profile, a distro package or `rust:alpine` needs no rustup at all. Both options this tool sets are delivered through `--config`, which **stable rustfmt honours**, so a stable toolchain produces the same bytes; only `--range` needs nightly, and it says so by name. A `rust-toolchain.toml` pin in the repository replaces `auto` with the channel it names, unless `cargo +toolchain` chose one for the run ([details](docs/configuration.md#the-toolchain-pin)).
- **Warm runs**: a content cache keyed on each file's bytes and the whole effective configuration skips files a previous run already proved were formatted, and remembers the resolved toolchain so a repeat invocation spawns no `rustup which` and no capability probe. `--no-cache` turns it off.
- **Smart Target Detection**:
  - Automatically identifies Cargo workspaces & crates and formats workspace members with rustfmt.
  - Seamlessly formats loose `.rs` files or non-Cargo directories using a parallel recursive walk (respecting `.gitignore`, including directories that are not a git checkout, unless `--no-ignore`).
  - Formats TOML under `.cargo/` and `.config/` at any depth. Skips `.git/`, cargo build directories, vendored trees, and any directory that contains `.cargo-checksum.json`.
  - A build directory is identified by evidence, not by name: cargo's `CACHEDIR.TAG`, a resolved `CARGO_TARGET_DIR` / `build.target-dir`, or a `target/` sitting next to a `Cargo.toml`. A source module called `target` is formatted like any other.
  - The search for the owning `Cargo.toml` stops at the enclosing git repository and never reaches `$HOME`, so a stray `~/Cargo.toml` cannot turn a scratch directory into a project.
  - A file argument that is a symlink is formatted through the link: the target is rewritten and the link stays. A walk, `--staged` and `--since` do not follow file symlinks, so a linked file outside the tree is left alone.
- **CI / Check Mode**: Full `--check` support for exit-code based verification in CI/CD workflows.
- **TOML formatting**: Formats `.toml` files in-process (no extra toolchain). An inline table stays on one line while it fits in 100 columns and wraps past that, the same rule arrays follow; single-key tables such as `{ path = "..." }` collapse to dotted keys (`project.path = "..."`) wherever a dotted key fits, and keep their braces where one does not, as in an array element (`{ triple = "..." }`). Extra blank lines are collapsed, `#` comments get a leading space, and the padding inside a key path or a table header goes (`[  a  .  b  ]` becomes `[a.b]`); comments keep their place, including the ones sitting just before a closing `]` or `}`. Indentation, width, array and inline-table layout, trailing commas, blank lines before table headers, bare-key normalization and cargo-aware ordering are all configurable, and `--style-guide` implements the `Cargo.toml` chapter of the Rust Style Guide in full -- see [TOML style](#toml-style) and [docs/toml-style.md](docs/toml-style.md). A `# fmt: off` comment exempts everything up to the matching `# fmt: on` from all of it, byte for byte -- the TOML counterpart of `#[rustfmt::skip]`. `Cargo.lock`, `clippy.toml`, `rustfmt.toml`, and `.rustfmt.toml` are skipped by default; see `--skip-toml` and `--no-default-toml-skips`.
- **Full crate versions** (opt-in): `rust-formatter --full-versions` resolves `Cargo.toml` dependency requirements against the crates.io sparse index and writes them out in full (`"4.6"` becomes `"4.6.6"`), keeping the `^`/`~`/`=` operator and cargo's own meaning for it. It honours the manifest's `rust-version`, cargo's proxy, timeout, CA-bundle and offline settings, reads cargo's on-disk index before the network, and says what it left alone and why. `--upgrade` bumps requirements that are already complete. See [Dependency versions](#dependency-versions) and [docs/versions.md](docs/versions.md).

---

## Installation & Prerequisites

### Prerequisites

A `rustfmt`. Any of these is enough, and they are tried in this order:

1. `$RUSTFMT`, cargo's own spelling for "use this one";
2. rustup's `nightly` toolchain;
3. rustup's active toolchain;
4. a `rustfmt` on `PATH`.

That is the order `--toolchain auto` follows. A `rust-toolchain.toml` in the
repository, or a `--toolchain NAME`, names a toolchain instead, and rustup is
asked for that one.

Nightly is preferred because it is the only channel with `--file-lines`, which
`--range` needs. Everything else -- including the `StdExternalCrate` grouping
and `Crate` merging this tool exists for -- works identically on stable, because
they are passed as `--config` rather than written into a `rustfmt.toml`.

The usual setup, if you have rustup:

```bash
rustup toolchain install nightly --component rustfmt
```

`--install-toolchain` will run that for you when the toolchain you named has no
rustfmt; without it, an interactive terminal is offered the choice and a hook or
CI job is simply told the command.

### Building & Installing

```bash
cargo install --locked --git https://github.com/vibe-technologies-llc/rust-formatter
```

It is not published on crates.io. From a clone, `cargo install --locked --path .`
does the same.

That installs two binaries: `rust-formatter`, and `cargo-rust-formatter`, which
is how cargo finds a subcommand. So `cargo rust-formatter` works anywhere
`~/.cargo/bin` is on `PATH`, and takes exactly the same arguments:

```bash
cargo rust-formatter --check
cargo rust-formatter src/lib.rs
```

### Completions and the man page

Both are generated on demand rather than committed, so they always describe the
binary you have:

```bash
rust-formatter completions bash > ~/.local/share/bash-completion/completions/rust-formatter
rust-formatter completions zsh  > ~/.zfunc/_rust-formatter          # with ~/.zfunc on $fpath
rust-formatter completions fish > ~/.config/fish/completions/rust-formatter.fish
rust-formatter completions elvish
rust-formatter completions powershell
```

`--out-dir DIR` writes the conventionally named file for you instead of printing
it. The man page works the same way:

```bash
rust-formatter man > ~/.local/share/man/man1/rust-formatter.1
rust-formatter man --out-dir ~/.local/share/man/man1   # one page per command
```

### Subcommands and paths

`completions`, `man` and `hook` are subcommands, and one of those names is read
as the subcommand until the first path argument -- so `rust-formatter completions`
and `rust-formatter --check completions` both mean the subcommand, while
`rust-formatter src completions` formats two directories. Write `./completions` or
`completions/` when you mean the directory. `--` is not the escape: it already
opens the [rustfmt pass-through](#custom-options--pass-through), so
`-- completions` is an argument for rustfmt.

---

## Usage

### Basic Formatting

Format the current directory / workspace:

```bash
rust-formatter
```

Format a specific directory or Cargo project:

```bash
rust-formatter /path/to/project
```

Format a single `.rs` file:

```bash
rust-formatter src/lib.rs
```

Format a single `.toml` file (a path named `Cargo.toml` still means the Cargo project):

```bash
rust-formatter rust-toolchain.toml
```

Complete Cargo.toml dependency requirements to full `x.y.z` (opt-in; still formats TOML/Rust):

```bash
rust-formatter --full-versions
```

### Selecting files

Any number of paths can be given, and they may be directories, Cargo projects, or single files. Overlapping paths are collapsed, so each file is formatted once:

```bash
rust-formatter crates/core crates/cli src/main.rs
```

A directory that is a package root means the package, and by default that widens to every member of its workspace. `--no-all` keeps the run to the package the manifest describes:

```bash
rust-formatter --no-all
```

Any other directory means exactly itself: `rust-formatter crates/core/src` formats what is under `crates/core/src` and nothing else, even inside a workspace, and a directory that merely holds members is limited to itself in the same way, so there is nothing for `--no-all` to narrow.

Read the list of paths from a file, or from standard input. Entries may be separated by newlines or NUL bytes; the separator is detected automatically, so `git ... -z` pipes work as-is:

```bash
rust-formatter --files-from paths.txt
git diff --name-only -z | rust-formatter --files-from -
```

Format only what changed. `--since` compares against the merge base of `<ref>` and `HEAD`; `--staged` takes the paths that have staged changes:

```bash
rust-formatter --check --since main
rust-formatter --staged
```

`--since` also selects untracked files. They differ from every ref — no ref describes them — so the set is the same whichever ref you name. `--no-untracked` leaves them out, which is usually what a CI check wants:

```bash
rust-formatter --check --since origin/main --no-untracked
```

When `merge-base` finds no fork point — a `fetch-depth: 1` checkout, or a ref with no common ancestor — the comparison falls back to the ref itself and says so, rather than quietly changing meaning:

```
warning: no merge base between origin/main and HEAD, so the comparison is against origin/main itself; a shallow clone has no fork point to find
```

Both scopes select *paths* and rewrite the **working tree**. `--restage` re-adds each formatted path to the index it came from, which is what makes `--staged` usable as a pre-commit hook — without it the commit carries the bytes the formatter replaced:

```bash
rust-formatter --staged --restage
```

If a staged file also has unstaged edits, the bytes formatted are not the bytes that would be committed. rust-formatter says so on stderr, and `--restage` deliberately leaves that path alone: staging it would also stage the edits you kept out of the commit.

```
warning: src/lib.rs has unstaged changes; formatted the working tree
warning: src/lib.rs was not re-added to the index; staging it would also stage its unstaged changes
```

Submodules are separate repositories, so neither scope reaches inside one by default. `--recurse-submodules` applies the same scope in every initialized submodule — for `--since`, against the commit the superproject recorded — and `--restage` then writes to each submodule's own index:

```bash
rust-formatter --staged --recurse-submodules --restage
```

The git environment is inherited on purpose. `git commit -- <paths>` points its hooks at a temporary index through `GIT_INDEX_FILE`, and that is the index `--staged` reads and `--restage` writes. That environment describes the superproject, so every git command run inside a submodule drops the repository-local variables `git rev-parse --local-env-vars` lists (`GIT_INDEX_FILE`, `GIT_DIR`, `GIT_WORK_TREE` and the rest) and reads and writes the submodule's own index. Every git subprocess also passes `--no-optional-locks`, so a run inside a hook never rewrites the index cache under the commit in progress.

Narrow or widen the set with gitignore-syntax globs. A pattern without `/` matches a name at any depth; a pattern with `/` is anchored at the path you named; `!` re-includes; the last match wins. `--exclude` also prunes matching directories, so `--include` cannot reach back inside one:

```bash
rust-formatter --exclude 'vendor/**' --exclude '*.generated.rs'
rust-formatter --include 'crates/core/**'
```

Restrict by language. `--toml-only` skips the toolchain preflight entirely, so it works with no rustup installed:

```bash
rust-formatter --rust-only
rust-formatter --toml-only
```

Walker controls. `--max-depth` is measured from each directory you name and does not apply to the per-package walks inside a Cargo workspace. Neither `--hidden` nor `--no-ignore` will descend into `.git/` or a build directory:

```bash
rust-formatter --hidden                     # walk dot-directories and dotfiles
rust-formatter --no-ignore                  # ignore .gitignore, .ignore and git excludes
rust-formatter --ignore-path .myignore      # extra gitignore-syntax pattern file
rust-formatter --max-depth 2
```

Adjust the TOML skip list:

```bash
rust-formatter --skip-toml 'fixtures/*.toml'
rust-formatter --no-default-toml-skips          # also format clippy.toml, rustfmt.toml, ...
rust-formatter --skip-toml '!clippy.toml'       # drop one default, keep the rest
```

### Installing a pre-commit hook

```bash
rust-formatter hook install              # format what is staged, and stage it
rust-formatter hook install --check      # refuse the commit instead
rust-formatter hook status               # exit 0 if installed, 1 if not
rust-formatter hook uninstall
```

`hook install` writes a `pre-commit` hook that runs `--staged --restage`, so the
commit carries formatted bytes on the first try. `--check` installs the
non-mutating variant instead: it runs `--staged --check` and aborts the commit,
leaving the fix to you.

The hook is found through `git rev-parse --git-path hooks`, so `core.hooksPath`
is honoured and a linked worktree gets the hooks the repository shares. It
carries a marker line, which is what lets a re-install upgrade it in place and
what stops `uninstall` from deleting a hook this tool did not write:

| State | `install` | `uninstall` | `status` |
| --- | --- | --- | --- |
| nothing installed | writes it | says so, exit `0` | exit `1` |
| ours, current | leaves the bytes alone | removes it | exit `0` |
| ours, older or other mode | upgrades in place | removes it | exit `0` |
| somebody else's | **refused**, exit `2` | **refused**, exit `2` | exit `1` |

`--force` replaces a foreign hook, saving it as `pre-commit.rust-formatter.bak`
first, and `uninstall` puts that back. A second `--force` over another foreign
hook is refused rather than overwriting the only copy of the first.

The hook resolves `rust-formatter` on `PATH` and falls back to the absolute path
it was installed from. If neither exists it fails the commit rather than passing
it unchecked.

For repositories that use the [pre-commit framework](https://pre-commit.com)
instead, `.pre-commit-hooks.yaml` in this repository defines two hook ids:

```yaml
repos:
  - repo: https://github.com/vibe-technologies-llc/rust-formatter
    rev: <tag or commit SHA to pin>
    hooks:
      - id: rust-formatter          # --staged --restage
      # - id: rust-formatter-check  # --staged --check
```

A caveat about that route: pre-commit stashes unstaged changes, runs the
hook, and then fails the run when a hook modified anything -- so the first
`git commit` reports `files were modified by this hook` even though
`--restage` already staged the formatted bytes, and a second `git commit`
succeeds. Use `rust-formatter-check` if you would rather the commit never be
rewritten, or the native installer above if you would rather it succeed first
time.

### Output

The two streams are separated by what they are for. **stdout carries the
product** -- diffs, file lists, JSON, formatted source -- so it can be piped.
**stderr carries the commentary** -- the summary, warnings and errors -- so it
never pollutes that pipe.

```bash
rust-formatter --list-different . | xargs $EDITOR   # stdout is just paths
rust-formatter --check . 2>/dev/null                # just the diff
rust-formatter --check . >/dev/null                 # just the verdict
```

By default, rust-formatter prints one summary line to stderr:

- `Formatted N files` after a write run, counting only the files it actually rewrote
- `Already formatted: ...` when nothing needed changing, in both write and `--check` mode
- `Check failed: N files need formatting` when `--check` finds work
- `Format failed: ...` when rustfmt itself failed
- `Previewed: ...` after `--emit stdout`, which writes nothing and so reports no verdict
- `Nothing to format` when every path named resolved to no formattable file

A run that rewrote nothing never claims otherwise, so `Formatted 3 files` means
three files changed on disk. When nothing changed there is no count to report
and the line names what was covered instead, for example
`Already formatted: 2 targets, 37 files`. A Cargo project is named by the
directory that was actually formatted -- the workspace root unless `--no-all`
narrowed the run, not the member you happened to point at.

On a TTY the status word is colored (green on success, red on check failure).
Color follows `NO_COLOR` / `CLICOLOR_FORCE` and can be forced. Both languages'
diffs are painted by rust-formatter itself -- rustfmt is always asked for plain
text -- so `--color never` really is plain:

```bash
rust-formatter --color always
rust-formatter --color never
```

Silence the commentary. `-q` suppresses the summary, warnings and verbose
output on stderr; it never changes what is formatted or checked, and it does not
suppress the product on stdout, so `--check` still prints its diff:

```bash
rust-formatter -q
rust-formatter -q --check >/dev/null   # exit code only
```

Print the detected target, plus the resolved rustfmt path, effective config,
worker count, edition and execution strategy:

```bash
rust-formatter -v
```

#### Listing files

```bash
rust-formatter --list-files .        # every file the selection resolves to; formats nothing
rust-formatter --list-different .    # only files whose formatting differs; exits 1 if any
rust-formatter -l .                  # -l is --list-different, as in rustfmt and gofmt
```

Both print one path per line on stdout. `--list-different` is check semantics
without the diff, which makes it the cheap form for hooks and scripts. Either
one also honours `--message-format json`, where the list becomes the envelope's
`files` array and still carries no diffs.

#### Machine-readable output

```bash
rust-formatter --check --message-format json . | jq '.files[] | select(.status == "needs-formatting")'
```

One JSON document on stdout, with all human output suppressed:

```json
{
  "version": 3,
  "mode": "check",
  "files": [
    { "path": "/work/app/src/lib.rs", "language": "rust", "status": "needs-formatting", "diff": "..." },
    { "path": "/work/app/bad.toml", "language": "toml", "status": "error" }
  ],
  "errors": [
    { "code": "toml-parse", "path": "/work/app/bad.toml", "line": 2, "column": 5, "message": "expected value" }
  ],
  "warnings": [],
  "summary": { "selected": 12, "changed": 1, "errors": 1 },
  "exit_code": 2
}
```

Every `path` is absolute, whatever form the path was given in on the command
line. `status` is `formatted`, `needs-formatting` or `error`. Only files the run
reached a verdict about are listed; `summary.selected` counts everything the
selection covered. Diffs embedded here are never colored.

`errors` is data, not prose. Every entry carries a stable `code` --
`toml-parse`, `rustfmt-diagnostic`, `tool-failed`, `io` and so on -- and a
`path`, `line` and `column` whenever the failure has them, so a consumer never
has to take a rendered sentence apart. `warnings` remains an array of strings.

The flag is honoured everywhere, not only for a run over files. `mode` says
which shape to expect:

| `mode` | Asked for by | Carries |
| --- | --- | --- |
| `write` | the default | `files` with `formatted` verdicts |
| `check` | `--check` | `files` with `needs-formatting` and a `diff` each |
| `list-files` | `--list-files` | `files` with no verdict |
| `list-different` | `--list-different` | `files` with verdicts and no diffs |
| `print-config` | `--print-config` | `config`, rustfmt's settings as a JSON object |
| `stdin` | `--stdin` | one file, plus `content` (or a `diff` under `--check`) |
| `preview` | `--emit stdout` | one file, plus `content` |

A run that fails outright reports that as JSON too: the envelope carries the
failure in `errors`, `exit_code` is `2`, and stderr stays empty. That includes a
configuration file or environment variable that cannot be read (`config`) and a
combination of flags or values the run refuses (`usage`). Only what the argument
parser itself rejects -- an unknown flag, a missing or malformed value, two flags
it declares as conflicting -- stays a plain `error:` on stderr, since the parser
fails before `--message-format` has been read.

`version` is `3`. `version`, `mode`, `files`, `errors`, `warnings`, `summary`
and `exit_code` are always present. `config` and `content` appear only in the
modes that carry them, and `versions` only when `--full-versions` considered at
least one dependency.

`--print-settings` is the one exception: under `--message-format json` it prints
its own `{"settings": …, "sources": …}` object rather than an envelope. See
[Configuration](#configuration).

#### Standard input

```bash
printf 'fn  main( ){}\n' | rust-formatter --stdin
rust-formatter --stdin --stdin-filepath src/lib.rs < src/lib.rs
```

`--stdin` reads the buffer, formats it and writes the result to stdout, leaving
stderr empty -- which is what an editor's format-on-save, an LSP wrapper or vim's
`formatprg` needs. `--stdin-filepath` names the buffer so the language is picked
from the extension and the edition from the owning `Cargo.toml`; without it the
input is treated as Rust. `--stdin --check` exits `1` when the buffer would
change and prints nothing.

A buffer is treated exactly as the file it names would be. A byte-order mark and
CRLF line endings survive the round trip, so `--stdin --check` on a CRLF buffer
that is already formatted exits `0` rather than reporting a difference nobody
made -- unless rustfmt's `newline_style` (from `--config` or a `rustfmt.toml`)
names `Unix`, `Windows` or `Native`, which then decides the line endings of a
Rust file as it does for rustfmt; and `--full-versions` completes dependency requirements when
`--stdin-filepath` names a `Cargo.toml`, which is the one buffer that has any.
`--message-format json` wraps the result, with the formatted text under
`content` and a unified diff under `files[0].diff` when `--check` is given.

```vim
set formatprg=rust-formatter\ --stdin\ --stdin-filepath\ %
```

Without `--stdin-filepath` the buffer is treated as Rust, and its edition comes
from the project's own settings or from `2024` -- never from rustfmt's default of
2015, in which `async` is not a keyword. A relative `--stdin-filepath` is
resolved against the directory the editor started the formatter in.

#### Editor integration

[docs/editors.md](docs/editors.md) has the configuration for VS Code, Neovim and
Helix, and a table of what each of them is able to pass -- which is the
difference between an inferred edition and a configured one. `--watch` below
needs no editor configuration at all.

#### Watch mode

```bash
rust-formatter --watch                     # reformat on save, in this tree
rust-formatter --watch --check             # keep saying what is unformatted
rust-formatter --watch --toml-only src/
```

`--watch` re-runs *the same run* on every debounced batch of changes: it
re-walks and re-plans from the paths you named. That is what makes a new file, a
deletion, a rename and an edited `.gitignore` behave here exactly as they do in
a one-shot run -- feeding the changed paths back in as a list would apply the
`--include`/`--exclude` globs and nothing else, and so would format files the
walk ignores. The content cache makes the repeat almost free: every file a batch
did not touch is a fingerprint hit.

What is watched: the workspace root for a cargo target by default, the package
alone under `--no-all`, the directory you named when you
named one, and the parent directory when you named a single file. `.git/`, cargo
build directories and vendored trees never wake it, so a `cargo build` in the
same tree costs nothing. `--watch FILE` watches that file; modules it declares
are formatted with it, but editing one of them does not trigger a run -- watch
the directory instead.

- Batches are debounced, so an editor's write-then-rename save is one run.
- A batch that changed nothing prints nothing. Only a run with something to say
  says it.
- A file that fails to format is reported and the watcher keeps going; the next
  save is usually the fix. Ctrl-C ends it. Exit `0` on a clean stop, `2` if the
  watcher could never start -- including the kernel's limit on watches, which
  the error names along with the `sysctl` that raises it.
- It rewrites files and keeps running, so it cannot be combined with
  `--emit stdout`, `--stdin`, `--list-files`, `--print-config`,
  `--print-settings`, `--range`, `--files-from`, `--restage` or the git scopes.
  `--check` and `--list-different` are allowed: they write nothing.
- `--watch --message-format json` emits one envelope per run as a concatenated
  JSON stream. `jq` and a streaming deserializer read it; it is not
  line-delimited.
- Toolchain notes and configuration warnings repeat on every run, because a
  per-file error has to be seen again after every save that fails to fix it.
  `-q` silences commentary without silencing errors or diffs.

#### Previewing without writing

```bash
rust-formatter --emit stdout src/lib.rs      # the formatted file, on stdout
rust-formatter --emit files                  # the default: rewrite in place
```

`--emit stdout` prints the formatted text and nothing else -- no path header, the
same framing for both languages -- and writes nothing. It is a preview of the
same fixed point a write run reaches, not of a single rustfmt pass, so
`rust-formatter --emit stdout f.rs` and `rust-formatter f.rs` agree byte for
byte, down to the byte-order mark and the line endings.

A preview is one file. The selection has to resolve to exactly one, so a
directory holding several is refused with exit `2` rather than concatenated into
a nameless stream, and a `.rs` file's `mod` children are not printed alongside
it. It cannot be combined with a listing, which names files rather than printing
one. Naming a file always means that file: `rust-formatter --emit stdout
Cargo.toml` previews the manifest, where `rust-formatter Cargo.toml` formats the
cargo project it belongs to.

#### rustfmt options

```bash
rust-formatter --print-config                              # the settings rustfmt will apply
rust-formatter --unset-config imports_granularity          # drop a default; rustfmt's own applies
```

`--config` can overwrite a default but never remove one, which is what
`--unset-config` is for. Both are checked against the options the resolved
rustfmt actually has, so a typo is refused with exit `2` rather than passing
silently. `--print-config` asks rustfmt for its resolved configuration rather
than echoing back the overrides that were passed, so it reports what will really
be used. Under `--message-format json` the same settings come back as a JSON
object under `config`, with typed values rather than a TOML blob.

An array-valued option is written the way TOML writes one. rustfmt splits its
own `--config` on `,` and rejects an array outright, so these travel through a
merged configuration file written to a temporary directory -- never into the
project:

```bash
rust-formatter --config 'skip_macro_invocations=["assert_matches","matches"]'
```

`ignore` is the exception. rustfmt resolves it against the directory of the
configuration file it was read from, so a list this tool synthesised elsewhere
could never mean what it says; it is applied by the selection layer instead,
where the same gitignore patterns also reach TOML:

```bash
rust-formatter --config 'ignore=["vendor","generated/**"]'   # same as --exclude
```

#### Rust style presets

The rustfmt options an opinionated wrapper should have an opinion about, as
names rather than a dozen `--config` strings. `--rust-style` is repeatable and
layered, and an explicit `--config` still wins over it:

| Preset | Sets |
| --- | --- |
| `comments` | `wrap_comments`, `normalize_comments`, `format_code_in_doc_comments` |
| `literals` | `hex_literal_case = "Upper"`, `condense_wildcard_suffixes`, `overflow_delimited_expr` |
| `strict` | the two above, plus `format_strings`, `reorder_impl_items`, `blank_lines_upper_bound = 1` |

```bash
rust-formatter --rust-style comments
rust-formatter --rust-style strict --config format_strings=false
```

#### Editions

Each file is formatted at the edition of the package that owns it, taken from
`package.edition` -- or from the target's own, for a `[[bin]]` that sets one. A
file with no `Cargo.toml` above it is formatted at **2024**, not at rustfmt's own
2015 default, under which `async`, `dyn` and `try` are not keywords. A project's
`rustfmt.toml` keeps the last word: when it sets `edition`, no inferred
`--edition` is passed, because rustfmt ranks the flag above the file.

`--edition` and `--config edition=` name the same setting and are reconciled
before the run, so they cannot disagree; the same holds for `--style-edition`.

`--style-edition` pins the edition of the Rust Style Guide, which is what keeps
output stable across rustfmt releases -- `--edition` only says which language
the file is:

```bash
rust-formatter --style-edition 2024
```

#### Range formatting

What an editor's format-selection request needs. Ranges are 1-based and
inclusive, repeatable, and Rust only -- a TOML document is formatted whole, and
a directory is refused: a range names lines of one file, so the PATH must be a
`.rs` file, or the buffer must be passed with `--stdin`.
A range gets one rustfmt pass rather than a run to a fixed point, because a
second pass would aim the same line numbers at text the first one already moved.
rustfmt works in whole lines, so a column is accepted and widened:

```bash
rust-formatter --range 12-40 src/lib.rs
rust-formatter --stdin --stdin-filepath src/lib.rs --range 12:1-40:9 < src/lib.rs
```

#### Modules outside the package

A `#[path = "../shared/x.rs"]` module lives outside every directory the walk
covers. When a selected file spells the attribute, one extra rustfmt pass over
the crate roots is spent to find those modules, and each one is put through the
same `--include`/`--exclude` filter as a walked file. A tree that never spells it
pays nothing. This applies to cargo projects, where `cargo metadata` names the
crate roots to start from.

### Configuration

Zero-config by default. When a repository wants a style of its own, it says so in
a table cargo already ignores -- no file is created, and nothing is written into
the project. The full specification is in
[docs/configuration.md](docs/configuration.md).

```toml rf:fixed-point
# Cargo.toml
[workspace.metadata.rust-formatter]
preset = ["cargo"]
toolchain = "nightly-2026-06-01"
exclude = ["vendor/**"]

[workspace.metadata.rust-formatter.config]
max_width = 120
```

A `rust-formatter.toml` (or `.rust-formatter.toml`) works the same way and is
discovered from the path being formatted upwards, stopping at the workspace root,
or at the repository boundary when there is no workspace. `--config-file FILE`
names one directly. `--no-config` ignores every file source -- the
`rust-toolchain.toml` pin, both metadata tables and a discovered file -- while
`RUST_FORMATTER_*` still applies; it cannot be combined with `--config-file`.

Precedence, lowest first:

| | Source |
| --- | --- |
| 1 | built-in defaults |
| 2 | `rust-toolchain.toml` (or `rust-toolchain`), which sets `toolchain` and nothing else |
| 3 | `[workspace.metadata.rust-formatter]` |
| 4 | `[package.metadata.rust-formatter]` |
| 5 | a discovered `rust-formatter.toml` |
| 6 | `--config-file FILE` |
| 7 | `RUST_FORMATTER_*` |
| 8 | flags typed on the command line |

A preset applies immediately below the layer that named it, so naming one never
discards the keys that layer also set.

A setting's key is its flag name without the leading dashes (`--sort-deps` is
`sort-deps`), and its environment variable is that key uppercased under
`RUST_FORMATTER_`:

```bash
RUST_FORMATTER_SORT_DEPS=1
RUST_FORMATTER_EXCLUDE='["vendor/**"]'
RUST_FORMATTER_TOML_MAX_WIDTH=120
```

There are three exceptions. `--rust-only` and `--toml-only` are the one
`languages` key (`"rust"`, `"toml"` or `"both"`); `--style-guide` is the preset
of that name; and the flags that say what one invocation does -- `--check`,
`--stdin`, `--watch`, the git scopes and the rest listed in
[docs/configuration.md](docs/configuration.md#what-is-not-configurable) -- have
no key at all and stay on the command line.

Because a repository can turn a rewrite on, every boolean flag backed by a
setting can turn it off again -- `--sort-deps=false`, `--sort-deps=0` and
`--no-sort-deps` all work, and the last spelling on the line wins.
`--no-ignore`, `--no-default-toml-skips` and `--no-all` take `=false` the same
way but have no `--no-` form of their own. Plain switches such as `--check`,
`--staged`, `--rust-only` and `--style-guide` take no value.

`--preset NAME` is repeatable and layered, and spans both languages:

| Preset | What it sets |
| --- | --- |
| `default` | nothing: the built-in defaults, by name |
| `cargo` (alias `style-guide`) | the Rust Style Guide's `Cargo.toml` chapter |
| `compact` | inline tables never wrap, so no TOML 1.1 construct is written |
| `expand` | any inline table with two or more keys wraps |
| `everything` | every ordering and layout rewrite that keeps array order |
| `narrow-tabs` | tab indent, 60-column budget |
| `aligned-indented` | aligned entries and comments, indented tables |
| `comments`, `literals`, `strict` | the rustfmt option groups, as `--rust-style` |

A configuration file may add `[presets.<name>]` tables of its own.

See what a run resolved to, and where each value came from:

```bash
rust-formatter --print-settings
rust-formatter --print-settings --message-format json
```

Under `--message-format json` that is one `{"settings": …, "sources": …}`
object, not the envelope a run prints: `settings` holds the resolved values and
`sources` names the layer each value that is not a default came from.

`--print-config` is a different question: it asks *rustfmt* for its own resolved
configuration.

### TOML style

Zero-config by default; every knob below is opt-in and none of them writes a
config file. The full specification is in [docs/toml-style.md](docs/toml-style.md).

Every flag written as `flag` below also takes a value, so a setting a
[configuration source](#configuration) turned on can be turned off for one run:
`--sort-deps=false`, `--sort-deps=0` or `--no-sort-deps`. `--style-guide` is a
plain switch instead, the same as `--preset style-guide`.

| Flag | Values | Default | What it does |
| --- | --- | --- | --- |
| `--toml-indent` | `0`-`16`, or `tab` | `4` | Indent of one nesting level in a wrapped container. |
| `--toml-tab-width` | `1`-`16` | `4` | Columns a tab advances to when the width budget is measured. |
| `--toml-max-width` | `1`-`4096` | `100` | Display columns a value may occupy on one line, counting the indent, the key, `" = "`, a separating comma and a same-line comment. |
| `--toml-arrays` | `preserve`, `auto`, `expand` | `preserve` | `preserve` keeps an array the author wrote across lines; `auto` lets width alone decide; `expand` breaks any array with two or more elements. |
| `--toml-inline-tables` | `auto`, `compact`, `expand`, `section` | `auto` | `auto` wraps past the width budget; `compact` never wraps, so the formatter writes no TOML 1.1 construct of its own; `expand` wraps any table with two or more keys; `section` gives a table too wide for its line a `[header]` of its own instead of wrapping it. |
| `--toml-version` | `1.0`, `1.1` | `1.1` | `1.0` never wraps an inline table or gives one a trailing comma, and warns about every 1.1-only spelling it cannot remove without rewriting a value. |
| `--toml-directives` | `on`, `off` | `on` | Whether a `# fmt: off` comment exempts the lines up to the matching `# fmt: on`. See [Turning the formatter off](#turning-the-formatter-off). |
| `--toml-array-spacing` | `compact`, `spaced` | `compact` | `compact` writes `[a, b]`; `spaced` writes `[ a, b ]`. |
| `--toml-inline-table-spacing` | `compact`, `spaced` | `spaced` | `spaced` writes `{ k = v }`; `compact` writes `{k = v}`. |
| `--toml-trailing-comma` | `never`, `multiline` | `never` | `multiline` ends a wrapped array or inline table with a comma. |
| `--toml-blank-line-before-tables` | flag | off | Exactly one blank line before every table header except the first. |
| `--toml-max-blank-lines` | `0`-`32` | `1` | Longest run of blank lines kept. |
| `--toml-align-entries` | flag | off | Pads keys so the `=` of neighbouring entries lines up. |
| `--toml-align-comments` | flag | off | Pads lines so the `#` of neighbouring same-line comments lines up. |
| `--toml-indent-tables` | flag | off | Indents a table header by the number of headers above it. |
| `--toml-indent-entries` | flag | off | Indents the entries under a table header by one level. |
| `--toml-normalize-keys` | flag | off | Drops the quotes from keys the spec allows bare. |
| `--sort-deps` | flag | off | Sorts `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, `[workspace.dependencies]`, `[replace]`, each table under `[patch]` and their `[target.'cfg(...)']` forms. |
| `--sort-package` | flag | off | Puts `[package]` and `[workspace.package]` into a canonical field order, not alphabetical order. |
| `--package-order` | `book`, `style-guide` | `book` | Which canonical order `--sort-package` applies: the Cargo Book sequence, or the Rust Style Guide's `name`, `version`, the rest version-sorted, `description` last. |
| `--sort-dep-fields` | flag | off | Orders the fields inside one dependency entry -- header, dotted or inline -- source first, then what is built from it. |
| `--sort-features` | flag | off | Sorts the keys of `[features]`. |
| `--sort-arrays` | flag | off | Sorts the arrays cargo reads as a set: feature lists, `keywords`, `categories`, `workspace.members`, `workspace.default-members`, `workspace.exclude`. Never `package.include` or `package.exclude`, whose `!` patterns make order meaningful, and never one holding a comment or a non-string. |
| `--sort-targets` | flag | off | Sorts `[[bin]]`, `[[example]]`, `[[test]]` and `[[bench]]` sections by `name`. |
| `--sort-tables` | flag | off | Puts the top-level tables into the Cargo Book's chapter sequence, each moving as a whole block. |
| `--sort-keys` | flag | off | Sorts the keys of every section no more specific flag claims. |
| `--sort-grouped` | flag | off | Makes every sort stay inside blank-line-separated groups instead of reordering across them. |
| `--cargo-conventions` | flag | off | Rewrites `dep = { version = "1" }` to `dep = "1"`. The one option that changes the value tree. |
| `--style-guide` | switch | off | The same as `--preset style-guide`: the `Cargo.toml` chapter of the Rust Style Guide. Any flag typed alongside it wins. |

All of the ordering flags act on the shape of a document, not on its name, so
they apply to any TOML file carrying a table of that shape.

```bash
rust-formatter --style-guide                  # the Rust Style Guide's Cargo.toml chapter
rust-formatter --sort-deps --sort-package
rust-formatter --toml-version 1.0             # readable by a TOML 1.0 parser
rust-formatter --toml-indent 2 --toml-max-width 80
rust-formatter --toml-align-entries --toml-align-comments
```

A multi-line inline table is a TOML 1.1 construct, so the default output of a
wide dependency is rejected by a TOML 1.0 parser such as `python -m tomllib`.
`--toml-inline-tables compact` keeps every inline table on one line, which is
enough for the formatter to add no 1.1 construct of its own.

`--toml-version 1.0` goes further: it also withholds the inline-table trailing
comma, and warns about the 1.1-only spellings a formatter cannot remove without
rewriting a value — `"\e[0m"`, `"\x41"`, `07:32`, and the inline table a
comment inside the braces forces open. The warnings name file, line and column,
appear in `--message-format json` under `warnings`, and never change the exit
code. See [docs/toml-style.md](docs/toml-style.md#toml-10-and-toml-11).

#### Turning the formatter off

A comment takes the formatter out of a stretch of a document. Everything from
the `off` marker through the `on` marker that closes it -- both marker lines
included -- is written back byte for byte, whatever the flags above say.

```toml rf:fixed-point
# fmt: off
matrix = [
  1,   2,   3,
  40,  50,  60,
]
# fmt: on
```

`# taplo: fmt-off` / `# taplo: fmt-on` and `# rust-formatter: fmt-off` /
`# rust-formatter: fmt-on` work the same way. Spacing and case inside the marker
are free, but the comment has to be exactly the marker, so
`# fmt: off — keep the columns` is an ordinary comment. A marker counts wherever
it is written, a trailing `wide = [ 1,  2,  3 ] # fmt: off` included, and the
region starts at the beginning of that line. An `off` with no `on` after it runs
to the end of the file, which is how a whole document is exempted;
`--toml-directives off` turns the mechanism off entirely.

For whole files, `--skip-toml` takes a glob instead. The full rules, including
what a region does to ordering, are in
[docs/toml-style.md](docs/toml-style.md#directives).

---

### Dependency versions

`--full-versions` resolves the requirements in a `Cargo.toml` against the
crates.io sparse index and writes them out in full. It is opt-in, runs only on a
file named `Cargo.toml`, and changes nothing else about how a document is
formatted. The full specification is in [docs/versions.md](docs/versions.md).

```bash
rust-formatter --full-versions
```

```toml rf:skip two-column before/after, not a formatter run
# before                      # after
serde = "1"                   serde = "1.0.229"
clap = "^4.6"                 clap = "^4.6.6"
ignore = "~0.4"               ignore = "~0.4.30"
tokio = "1.*"                 tokio = "1.47.5"
local = { path = "../local" } local = { path = "../local" }   # untouched
```

The operator keeps cargo's meaning: `^1.0` admits any `1.x`, so it completes to
the newest `1.x` and not to the newest `1.0.x`. `~1.0` stays inside `1.0`, `~1`
becomes a caret and `0.0` a tilde so that neither narrows, and a bare `0` is left
as written because no `x.y.z` spells `<1.0.0`. `=4.6`
means "any `4.6.x`" to cargo, so completing it to `=4.6.6` *narrows* the
requirement rather than filling it in.

| Flag | What it does |
| --- | --- |
| `--full-versions` | resolve and complete dependency requirements |
| `--upgrade` | also rewrite requirements that already name `x.y.z`, to the newest release the requirement still admits |
| `--upgrade-incompatible` | allow a bump that breaks the requirement (`^1.7.3` → `^2.0.1`), never below its lower bound; needs `--upgrade` |
| `--upgrade-pinned` | include `=` requirements when upgrading; needs `--upgrade` |
| `--allow-yanked` | consider yanked releases |
| `--ignore-rust-version` | choose versions without regard for the manifest's `rust-version` |
| `--registry-url <URL>` | resolve against another sparse index |
| `--offline` | never touch the network: read cargo's on-disk index and run `cargo metadata` offline |

Nothing is left unexplained. A dependency that is not rewritten is either
counted in the run's summary line or named outright:

```
warning: /work/app/Cargo.toml:18:1: anything left alone: `*` has no major version to complete
warning: /work/app/Cargo.toml:21:1: typo-crate left alone: no such crate on the registry
--full-versions: 4 completed, 11 left alone (1 rust-version, 1 path, 1 git, 1 workspace-inherited, 2 other registry, 3 not completable, 1 unknown crate, 1 patch)
```

`--verbose` lists every one of them; `--message-format json` always carries the
whole account in a `versions` array. Under `--check` or `--verbose`, each
completion is named as a version finding rather than showing up only inside the
file's diff. A manifest that is already fully pinned prints nothing.

A crate the registry does not have, a yanked-only crate, an offline miss or a
vendored `[source.crates-io]` replacement are all **skips**: they are reported
and the run still exits `0`. Only a failure of the registry itself — a transport
error, a `5xx`, a refused redirect, a TLS failure — is fatal, and it stops the
run before any file is written.

Cargo's own settings are honoured: `net.offline`, `net.retry`, `http.timeout`,
`http.proxy` and `http.cainfo`, along with `CARGO_NET_OFFLINE`,
`CARGO_NET_RETRY`, `CARGO_HTTP_TIMEOUT`, `CARGO_HTTP_PROXY`, `CARGO_HTTP_CAINFO`
and `SSL_CERT_FILE`. TLS trusts the OS store, so a corporate root works. Cargo's
on-disk registry index is read before the network, and a per-crate `ETag` cache
turns a repeat run into a `304`.

`--full-versions` and `cargo upgrade` will disagree: cargo-edit deliberately
preserves precision (`"1.0"` upgrades to `"1.1"`) while this expands it. Wrap a
deliberately imprecise requirement in `# fmt: off` / `# fmt: on` to keep it —
a frozen dependency is never rewritten and never costs a request.

---

### Checking in CI (`--check`)

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Already formatted, or a write run succeeded |
| `1` | `--check` found files that need formatting |
| `2` | Tool / environment / parse / I/O error (no rustfmt or git, bad TOML, unreadable path, invalid glob, a registry that failed, …) |

A selection you narrowed yourself — `--since`, `--staged`, `--files-from`, `--include`, `--exclude`, `--ignore-path`, `--skip-toml`, `--rust-only`, `--toml-only`, `--max-depth` — that matches no file exits `0` silently. This is what makes `--staged` usable as a hook: a commit that touches no Rust is a success, not an error. A plain directory with nothing formattable in it still exits `2`.

A bad TOML file does not stop the rest of the tree: every file that cannot be parsed, read, or written is reported on its own line and the process exits `2` after the rest of the tree is formatted. Pass `--fail-fast` to abort on the first one.

- **Rust** and **TOML**: the same unified diff on stdout, with `--- <path>` / `+++ <path>` / `@@` headers, in the same colors. rustfmt's own `--check` output is not unified -- it prints `Diff in <path>:<line>:` and bare `-`/`+` lines -- so it cannot be piped to `patch` or `git apply`. This one names every file by its absolute path in the `---`/`+++` headers, so it applies from `/`: run `git apply --unsafe-paths -p0` or `patch -p1` there, naming the patch file by its absolute path. Inside a repository, plain `git apply` refuses the absolute paths. Each worker's output is written whole, so `-j 8` never interleaves diffs from eight processes.
- `-U`/`--diff-context` sets the lines of context around each hunk, the way `diff -U` does. It defaults to `3`, and `-U 0` prints changed lines alone.
- A file rustfmt cannot parse is reported on its own line as `<path>:<line>:<column>: <message>`, one error per file, exactly as an unparseable TOML file is.
- Summary line goes to stderr and is still `Check failed: …` unless `-q`.
- `--list-different` replaces both diffs with a plain list of paths.
- `--stdin --check` reports on the buffer alone and prints nothing in human output; `--message-format json` carries the diff.
- A stable rustfmt formats identically and is checked through `--emit stdout` rather than the nightly-only `--emit json`; the diff, the exit code and the bytes on disk are the same either way. A warning names the channel, because `--range` is unavailable there.

```bash
rust-formatter --check
```

### Execution

| Flag | Default | What it does |
| --- | --- | --- |
| `-j`, `--jobs N` | CPU count | Worker threads. Both languages and every edition group draw from one queue, so this is the number of files and rustfmt processes in flight, not a budget per half. |
| `--toolchain NAME` | `auto` | `auto` takes `$RUSTFMT`, then rustup's `nightly`, then rustup's active toolchain, then a `rustfmt` on `PATH`; a `rust-toolchain.toml` pin replaces `auto`, and a `cargo +toolchain` outranks the pin. A name is asked of rustup, and when rustup is not installed at all it falls back to `$RUSTFMT` or `PATH` with a warning. |
| `--install-toolchain[=BOOL]` | off | Run the `rustup` command a missing rustfmt would otherwise only be printed. Without it, an interactive terminal is offered the choice; a hook or a CI job never is. |
| `--cache[=BOOL]`, `--no-cache` | on | Skip files whose bytes and effective configuration a previous run already proved were formatted, and reuse the resolved toolchain. |
| `--offline[=BOOL]` | off | Never reach the network: read the local registry index and run `cargo metadata --offline`. There is no `--locked` beside it, because `cargo metadata --no-deps` resolves nothing and so has no lock file to pin. |
| `--fail-fast[=BOOL]` | off | Stop scheduling work after the first file fails, in either language. Work already running finishes. |
| `--watch` | off | Keep running, re-running this same run on every debounced batch of changes. See [Watch mode](#watch-mode). |

The cache lives under `$XDG_CACHE_HOME/rust-formatter` (`%LOCALAPPDATA%\rust-formatter\cache`
on Windows, `~/Library/Caches/rust-formatter` on macOS), one file per target
root, and `RUST_FORMATTER_CACHE_DIR` moves it (a relative path is taken from the
current directory). An entry is the file's bytes
hashed with xxh3 together with a hash of the whole effective configuration --
including the resolved rustfmt's own version -- so changing any of them is a
miss, and an entry can only ever say "this exact content was already a fixed
point". Nothing is cached for `--stdin`, `--emit stdout`, `--range`, a manifest
being resolved by `--full-versions`, or a single `.rs` file whose module tree
rustfmt follows. `RUST_FORMATTER_CACHE=0` switches it off without a flag, and
like every other setting's variable it yields to an explicit `--cache` or
`--no-cache`.

### Custom Options & Pass-through

Override or add rustfmt options, or drop one of rust-formatter's defaults:

```bash
rust-formatter --config max_width=120,tab_spaces=4
rust-formatter --unset-config imports_granularity
rust-formatter --print-config
```

`--edition` and `--style-edition` accept only what rustfmt accepts -- `2015`,
`2018`, `2021`, `2024` -- and are rejected by the argument parser rather than by
rustfmt.

Pass arguments directly to rustfmt (including rustfmt's own `--verbose`):

```bash
rust-formatter -- --verbose
```

`-v` / `--verbose` is rust-formatter's own diagnostics. Use `-- --verbose` for rustfmt verbose output.

---

## Example

### Before
```rust
use std::path::Path;
use crate::bar;
use serde::Deserialize;
use std::fs;
use crate::foo;
```

### After
```rust
use std::{fs, path::Path};

use serde::Deserialize;

use crate::{bar, foo};
```

### TOML

#### Before
```toml rf:before
clap={version="4.6.6",features=["derive"]}
local = { path = "../local" }
key="v"#note
categories = ["api-bindings", "compression", "encoding", "multimedia::audio", "multimedia::encoding"]
```

#### After
```toml rf:after
clap = { version = "4.6.6", features = ["derive"] }
local.path = "../local"
key = "v" # note
categories = [
    "api-bindings",
    "compression",
    "encoding",
    "multimedia::audio",
    "multimedia::encoding"
]
```

The `categories` line wraps because it is 101 columns wide with its key; `clap`
fits at 51 and stays put.

With `--full-versions`, `"4.6"` becomes the latest matching crates.io release
such as `"4.6.6"` -- see [Dependency versions](#dependency-versions).

---

## Development

These have to pass before a change goes in:

```bash
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo run --locked -- --check .     # the formatter checks its own tree
```

The `Justfile` wraps them: `just all` runs the three plus a release build.
`just update-expect` regenerates the goldens under `tests/expected/` -- review
that diff before committing it. `just test-ignored` runs the registry sweeps that
read `~/.cargo/registry`, `just corpus` a differential sweep against a git ref,
and `just fuzz` the fuzz targets.

Every fenced `toml` block in this README and in `docs/` carries an `rf:`
annotation and is executed by `tests/doc_tests.rs`, or says with `rf:skip` why
it cannot be, so an example that stops matching the formatter fails the suite.

## License

Copyright (C) 2026 Vibe Technologies LLC

Licensed under the GNU Affero General Public License, version 3 only
(AGPL-3.0-only). See [LICENSE](LICENSE).
