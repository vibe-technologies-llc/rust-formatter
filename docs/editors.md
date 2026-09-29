# Editors

How to reach `rust-formatter` from an editor, and what each editor can and
cannot tell it.

There is no plugin and no language server. `--stdin` is the whole interface: it
reads a buffer, formats it and writes the result back, which is what every
editor's "format this file" hook is built to drive. Everything below is a
configuration snippet plus an honest account of what that editor is able to
pass, because the difference between them is the difference between an inferred
edition and a guessed one.

## Contents

- [The contract](#the-contract)
- [VS Code](#vs-code)
- [Neovim](#neovim)
- [Helix](#helix)
- [What each editor can pass](#what-each-editor-can-pass)
- [Without an editor at all](#without-an-editor-at-all)

## The contract

Three things, and they are the same three for every editor:

1. send the buffer on standard input;
2. pass `--stdin`, and `--stdin-filepath <path>` when the editor knows the
   buffer's path -- that is how the language is picked from the extension and
   the edition from the owning `Cargo.toml`;
3. read the formatted buffer from standard output, treat standard error as
   commentary, and expect exit `0`.

The rest of this file is the parts that are not obvious.

**An absolute path is better, but a relative one works.** A relative
`--stdin-filepath` is resolved against the working directory the editor started
the formatter in, then used exactly as an absolute one would be.

**Without `--stdin-filepath` the buffer is treated as Rust**, and as if it lived
in the working directory the editor started the formatter in. The edition is
read from there the same way it would be for a named file: a `rustfmt.toml`
that names one, else the nearest `Cargo.toml` above that directory, else the
project's `edition` setting in `rust-formatter.toml` (or its
`[workspace.metadata.rust-formatter]` table), else `2024`. It is never
rustfmt's own default of 2015, in which `async`, `dyn` and `try` are not
keywords and a modern buffer does not parse. What that directory is depends on
the editor, and a workspace whose packages differ in edition cannot be told
apart from it. Name the path when you can; set `edition`, or pass `--edition`,
when you cannot.

**A `rustfmt.toml` in the project still wins.** If it names an `edition`, no
`--edition` is passed at all, because rustfmt ranks the flag above its own file
and inferring one would silently override what the repository asked for.

**`--range L-L`**, or `L:C-L:C`, is the format-selection request. It is Rust
only -- a TOML document is formatted whole -- and it needs a nightly rustfmt,
because `--file-lines` is nightly-only. Only one of the editors below can
actually reach it.

**Leave `--check` out.** On stdin it exits `1` and prints nothing, which is not
what a format-on-save hook wants.

**`--full-versions`** completes dependency requirements on stdin only when
`--stdin-filepath` names a `Cargo.toml`, which is the one buffer that has any.

A byte-order mark and CRLF line endings survive the round trip, so a formatter
hook cannot silently renormalize a file's framing. The one exception is a
rustfmt `newline_style` of `Unix`, `Windows` or `Native`, which a Rust buffer
follows exactly as rustfmt would.

## VS Code

Rust, through rust-analyzer:

```jsonc
"rust-analyzer.rustfmt.overrideCommand": ["rust-formatter", "--stdin"]
```

rust-analyzer documents that "the file contents will be passed on the standard
input and the formatted result will be read from the standard output", which is
exactly this contract. Two consequences worth knowing before you rely on it:

- **There is no placeholder to substitute.** `$saved_file` belongs to
  `check.overrideCommand`; the rustfmt override passes its arguments through
  verbatim. So the buffer's path cannot be handed to `--stdin-filepath`, and the
  edition is read from the formatter's working directory rather than from the
  file's own package (see [the contract](#the-contract)). Where that directory
  is not the package the file belongs to, set the edition once in the
  repository instead:

  ```toml rf:fixed-point
  [workspace.metadata.rust-formatter]
  edition = "2024"
  ```

  or append `"--edition", "2024"` to the override command.
- **Range formatting is not available through an override.** rust-analyzer only
  builds the range arguments for its own rustfmt, so
  `rust-analyzer.rustfmt.rangeFormatting.enable` has no effect on an override
  command. Whole-buffer formatting is what this gets you.

A non-zero exit produces no edit, so a buffer that fails to parse is left alone
rather than mangled -- which is also why commentary on standard error is safe.

This hook is Rust only. VS Code has no equivalent for TOML: nothing in the TOML
extensions accepts an external formatter command. Either run
[`rust-formatter --watch`](../README.md#watch-mode) alongside the editor, which
needs no editor configuration at all, or bridge it with a generic run-a-command
formatter extension -- with the caveat that those cannot pass the buffer's path
either, so a `Cargo.toml` buffer is indistinguishable from any other TOML and
`--full-versions` is out of reach.

## Neovim

Through [`conform.nvim`](https://github.com/stevearc/conform.nvim), which
substitutes `$FILENAME` with the buffer's absolute path:

```lua
require("conform").setup({
  formatters = {
    rust_formatter = {
      command = "rust-formatter",
      args = { "--stdin", "--stdin-filepath", "$FILENAME" },
      stdin = true,
      range_args = function(_, ctx)
        return {
          "--stdin",
          "--stdin-filepath",
          "$FILENAME",
          "--range",
          ("%d-%d"):format(ctx.range.start[1], ctx.range["end"][1]),
        }
      end,
    },
  },
  formatters_by_ft = {
    rust = { "rust_formatter" },
    toml = { "rust_formatter" },
  },
  format_on_save = { lsp_format = "never", timeout_ms = 2000 },
})
```

One formatter entry serves both filetypes, because the language is read from the
path. `stdin = true` is conform's default and is spelled out here only because
it is the whole contract.

`lsp_format = "never"` matters: without it rust-analyzer's own formatter runs as
well.

`range_args` is the only place in this file where `--range` is reachable, so
this is the one editor that can format a selection. It needs a nightly rustfmt,
and it is Rust only -- give TOML a second entry without `range_args` if you want
to be strict about that.

## Helix

Through `languages.toml`, in the project or in `~/.config/helix/`. Needs Helix
25.07 or newer, which is where `%{buffer_name}` in formatter arguments arrived:

```toml rf:fixed-point
[[language]]
name = "rust"
auto-format = true
formatter = { command = "rust-formatter", args = ["--stdin", "--stdin-filepath", "%{buffer_name}"] }

[[language]]
name = "toml"
auto-format = true
formatter = { command = "rust-formatter", args = ["--stdin", "--stdin-filepath", "%{buffer_name}"] }
```

Helix states the same contract -- "the formatter must be able to take the
original file as input from stdin and write the formatted file to stdout" -- and
documents `%{buffer_name}` as the way to pass the current buffer's name.

Two things to know:

- **`%{buffer_name}` is a relative path**, resolved against the directory Helix
  was started in. Start Helix at the project root and the owning `Cargo.toml`
  is found and the edition is inferred; start it elsewhere and it is not.
- **A scratch buffer has no file**, so the expansion is a name with no extension
  and the buffer is treated as Rust. For a TOML scratch buffer whose language
  you set by hand, that is wrong, and there is nothing to be done about it from
  here.

On an older Helix, pass a fixed name purely to select the language --
`"--stdin-filepath", "buffer.rs"` -- and accept that the edition can no longer
come from the buffer's own project: set `edition` in the repository's
`rust-formatter.toml`, or add `"--edition", "2024"` to `args`.

## What each editor can pass

| | Buffer path | Edition inferred | TOML | `Cargo.toml` and `--full-versions` | `--range` |
| --- | --- | --- | --- | --- | --- |
| VS Code (rust-analyzer) | no | from the working directory, not the file -- set `edition` or `--edition` to be sure | no hook exists | no | no |
| Neovim (conform.nvim) | yes, absolute | yes | yes | yes | yes, via `range_args` |
| Helix 25.07+ | yes, relative | yes, from Helix's own directory | yes | yes | no |

## Without an editor at all

`rust-formatter --watch` reformats files as they change, which needs no editor
configuration and covers both languages and every editor at once. It is the
answer for anything not listed above, and for TOML in VS Code. See
[Watch mode](../README.md#watch-mode).

Vim's built-in `formatprg` needs no plugin either:

```vim
set formatprg=rust-formatter\ --stdin\ --stdin-filepath\ %
```
