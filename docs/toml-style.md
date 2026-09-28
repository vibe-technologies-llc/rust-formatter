# TOML style

What `rust-formatter` does to a `.toml` file, and which parts of it you can
change. Every rule below is the behaviour of the current implementation; where a
flag changes it, the flag is named.

The formatter parses the document with `toml_edit` and rewrites only the
whitespace and comments around each key, value and table. It never re-serialises
a value, so the layout rules here cannot change what a document means. The
exceptions are opt-in and named in [Guarantee 2](#guarantees): the flags that
reorder arrays, and the ones that rewrite a value.

## Contents

- [Guarantees](#guarantees)
- [The width budget](#the-width-budget)
- [Inline tables](#inline-tables)
- [Dotted keys](#dotted-keys)
- [Arrays](#arrays)
- [Trailing commas](#trailing-commas)
- [Indentation](#indentation)
- [Comments](#comments)
- [Directives](#directives)
- [Blank lines](#blank-lines)
- [Alignment](#alignment)
- [Keys](#keys)
- [Ordering](#ordering)
- [The Rust Style Guide](#the-rust-style-guide)
- [TOML 1.0 and TOML 1.1](#toml-10-and-toml-11)
- [Option reference](#option-reference)

## Guarantees

For any document that parses, under any combination of options:

1. Formatting succeeds and the output parses.
2. The value tree is unchanged. `[a] b = 1`, `a.b = 1` and `a = { b = 1 }` are
   interchangeable spellings and the formatter may move between them, but no
   value is added, dropped or altered. The exceptions are all opt-in:
   - `--sort-arrays` and `--sort-targets` reorder arrays — the values cargo
     reads as a set, and the `[[bin]]`-style target lists — so the tree is
     unchanged only up to the order of those arrays;
   - `--cargo-conventions` rewrites `dep = { version = "1" }` to `dep = "1"` —
     the two mean the same thing to cargo, but a table becomes a string;
   - [`--full-versions`](versions.md) rewrites version requirement strings.
3. Formatting is a fixed point: a second pass changes nothing.
4. No comment is lost. Comments move only where a rule below says they move.
5. Value spellings survive verbatim. `'win\path'`, `1.50`, `0x1F`, `07:32:00`
   and `"""…"""` all come out as they went in. The one exception is a version
   requirement rewritten by [`--full-versions`](versions.md), which is written
   as a basic string.
6. Everything between a [`# fmt: off`](#directives) marker and the `# fmt: on`
   that closes it comes out byte for byte, whatever the other options say. The
   one option that does reach it is
   [`--toml-directives off`](#directives), which is what stops a marker being
   read as one at all.

All six are checked over committed fixtures, over generated documents and over
the local crate registry, under each of the seven TOML presets and under a
style drawn at random from the whole option surface. See
`tests/corpus_tests.rs`, `tests/property_tests.rs`, `tests/encoding_tests.rs`
and `scripts/corpus.sh`.

Every fenced TOML example on this page is executed by `tests/doc_tests.rs`,
which is what the `rf:` marker on each fence selects.

## The width budget

`--toml-max-width` (default `100`) is the number of columns a value may occupy
while staying on one line. A container stays on one line when

```
column + width(value) + reserved <= max_width
```

where `column` is where the value starts and covers everything to its left:

- the indentation of the line: one level for each wrapped container the value
  sits inside, plus whatever `--toml-indent-entries` and `--toml-indent-tables`
  give the table body it belongs to
- the key as it is written, including quotes
- every ancestor segment of a dotted key path, and the dots
- the ` = ` that follows

`reserved` is what still has to fit *after* the value on the same line:

- the comma separating it from the next entry of a wrapped array or expanded
  inline table, and the comma `--toml-trailing-comma multiline` adds to the last
  one
- a comment sharing the value's line, `" #"` included

and `width(value)` is the value's own one-line rendering, with `pad` being 1 for
a container whose spacing pads the inside of its brackets and 0 otherwise (see
[Arrays](#arrays) and [Inline tables](#inline-tables)):

| value | width |
| --- | --- |
| scalar | its rendered text |
| `[]` | 2 |
| `[a, b, c]` | `2 + 2 × pad + Σ width + 2 × (n − 1)` |
| `{}` | 2 |
| `{ k = v, … }` | `2 + 2 × pad + Σ (width(k) + 3 + width(v)) + 2 × (n − 1)` |

### Columns

Width is counted in the columns a terminal draws, not in bytes and not in
`char`s. A CJK ideograph or an emoji costs two, a combining mark costs nothing,
and a tab advances to the next multiple of `--toml-tab-width` (default `4`) —
which is also what one level of `--toml-indent tab` costs. This is the same
notion of a column that rustfmt applies to the Rust half of a project, so the
two halves agree about what `100` means.

A value whose own text spans lines — a multi-line string — has no column count.
It is treated as over budget, so its container wraps; it never *forces* a break
in a container that has already committed to one line.

Nothing else enters the decision. In particular a container that has committed
to one line forces every value inside it onto that line too, however wide.

Two things are laid out first and measured never, so they can leave a line
longer than the budget: a single value too wide to break at all, and the padding
`--toml-align-entries` / `--toml-align-comments` add (see
[Alignment](#alignment)).

## Inline tables

`--toml-inline-tables` picks between four rules:

| mode | rule |
| --- | --- |
| `auto` (default) | one line while it fits the [width budget](#the-width-budget) |
| `compact` | always one line |
| `expand` | one line only with a single key |
| `section` | one line while it fits, and a `[header]` section of its own when it does not |

`--toml-version 1.0` overrides this to `compact`; see
[TOML 1.0 and TOML 1.1](#toml-10-and-toml-11).

A one-line inline table is written `{ a = 1, b = 2 }`: one space inside each
brace, no space before a comma, one after. `--toml-inline-table-spacing compact`
drops the spaces inside the braces, `{a = 1, b = 2}`. An empty one is `{}`
either way.

```toml rf:fixed-point-parts
# auto, default width
clap = { version = "4.6.6", features = ["derive"] }

# auto, once the line no longer fits
clap = {
    version = "4.6.6",
    features = ["derive", "cargo", "env", "unicode", "wrap_help", "suggestions"]
}
```

**A comment inside the braces is the only thing that can force a break.** A
comment cannot share a line with what follows it, so a table holding one is
written across lines whatever the mode says — `compact` included:

```toml rf:fixed-point
x = {
    a = 1, # why
    b = 2
}
```

A comment *outside* the braces is a comment on the line, not on the table, and
does not break it:

```toml rf:fixed-point
x = { a = 1, b = 2 } # keep
```

An empty table holding nothing but a comment stays open for the same reason.

### Promotion to a section

`section` measures like `auto`, but a table that does not fit is written as its
own header section instead of being wrapped over lines. This is what the
[Rust Style Guide](#the-rust-style-guide) asks for:

```toml rf:before-after
# before
[dependencies]
short = "1"
extremely_long_crate_name_goes_here = { version = "4.5.6", path = "extremely_long_path_name_goes_right_here" }

# after --toml-inline-tables section
[dependencies]
short = "1"
[dependencies.extremely_long_crate_name_goes_here]
version = "4.5.6"
path = "extremely_long_path_name_goes_right_here"
```

A comment block above the entry moves onto the new header. The transform runs
only one way — a short `[dependencies.foo]` section is never pulled back inline
— which is what makes it a fixed point.

Promotion needs somewhere to write the header, so it falls back to `auto`'s
multi-line form when there is none:

- inside an array or inside another inline table, where a header cannot be
  written at all;
- at the document root, where the new header would have to be moved past every
  other table in the file;
- for a table of a single key, which the [dotted key](#dotted-keys) rule already
  covers;
- for a table holding a comment between a key and its value, a position a table
  body cannot render — promoting would lose it, so the table wraps instead.

`--toml-version 1.0` pins every inline table to one line, `section` included, so
under 1.0 nothing is promoted and a table that does not fit stays on its line.

## Dotted keys

A single-key inline table written as the value of a key in a table body
collapses to a dotted key. This happens before the width budget is consulted and
is not width-limited.

```toml rf:skip arrow table, not a formatter run
project = { path = "crates/foo" }   ->   project.path = "crates/foo"
foo = { workspace = true }          ->   foo.workspace = true
a = { b = { c = 1 } }               ->   a.b.c = 1
```

The collapse is skipped when:

- the table has more than one key;
- the table is an array element, or sits inside a table that kept its braces —
  a dotted key cannot go there, so the braces stay:

  ```toml rf:fixed-point
  items = [{ a = 1 }]
  x = { a = 1, b = { c = 2 } }
  ```

- collapsing would drop a comment. A dotted line keeps only the comment that
  shares the value's line, so a comment on the key, above it, or below the value
  holds the table open:

  ```toml rf:fixed-point
  x = {
      # note
      a = 1
  }
  ```

A comment that *does* share the value's line survives the collapse:

```toml rf:skip arrow table, not a formatter run
project = { path = "x" } # keep   ->   project.path = "x" # keep
```

A dotted key the author wrote is left as it is, wherever it appears — inside
braces included, where `{ a.b = 1 }` is not rewritten to `{ a = { b = 1 } }`.
The formatter never expands a dotted key back into a table. Its own spacing is
normalized, though: see [Keys](#keys).

## Arrays

`--toml-arrays` picks between three rules:

| mode | rule |
| --- | --- |
| `preserve` (default) | an array the author wrote across lines stays across lines; otherwise the width budget decides |
| `auto` | the width budget decides, so an array that fits is pulled back onto one line |
| `expand` | any array with two or more elements is written across lines |

Under `preserve`, an array counts as written across lines if any newline appears
between its brackets. A nested array that qualifies breaks its parent too.

`preserve` and `expand` reach every array except one inside a container pinned
to a single line by `--toml-inline-tables compact` or `--toml-version 1.0`: a
break there would write a multi-line inline table, which is the TOML 1.1
construct those settings exist to avoid. Nothing else overrides them.

```toml rf:fixed-point-parts
# preserve keeps this shape even though it fits
features = [
    "bmp",
    "webp"
]

# --toml-arrays auto
features = ["bmp", "webp"]
```

A one-line array is written `[a, b, c]`, with no space inside the brackets;
`--toml-array-spacing spaced` writes `[ a, b, c ]` instead. An empty one is
written `[]` either way, whatever whitespace or newlines it held — unless it
holds a comment, which keeps it open:

```toml rf:fixed-point
targets = [
    # keep
    # me
]
```

A wrapped array puts one element per line, indented one level from the line the
opening bracket is on, with the closing bracket back at that level.

## Trailing commas

`--toml-trailing-comma` is `never` by default: no array or inline table ends with
a comma, in either shape.

`multiline` adds one to wrapped arrays and expanded inline tables. One-line forms
never take one.

```toml rf:fixed-point --toml-trailing-comma multiline
a = [
    1,
    2,
]
```

A comment that sat on the last element moves to after the comma, where it can no
longer swallow it:

```toml rf:fixed-point --toml-trailing-comma multiline
a = [
    1,
    2, # two
]
```

## Indentation

`--toml-indent` sets what one nesting level of a wrapped container costs:
`0`–`16` spaces, or `tab`. The default is four spaces. `--toml-tab-width`
(default `4`) says how many columns a tab is worth when the width budget is
measured; it changes no output on its own.

Table bodies and table headers are flush by default, which is TOML's own layout.
Two flags opt out:

| flag | effect |
| --- | --- |
| `--toml-indent-entries` | the entries under a table header take one level |
| `--toml-indent-tables` | a table header takes one level per header above it |

```toml rf:fixed-point --toml-indent-tables --toml-indent-entries
# --toml-indent-tables --toml-indent-entries
[a]
    x = 1

    [a.b]
        y = 2
```

A header that is never written — an implicit `[a]` standing above a lone
`[a.b.c]` — indents nothing beneath it. Both flags feed the width budget, so an
indented value wraps that much sooner.

## Comments

Every comment is rewritten to a canonical form: the `#`, any further `#` that
immediately followed it, one space, and the trimmed body.

```toml rf:skip arrow table, not a formatter run
#no space      ->   # no space
##Banner       ->   ## Banner
#   padded     ->   # padded
#              ->   #
```

Repeated leading hashes survive because they are used as heading markers. A
comment with an empty body stays bare.

A comment that shares a line with a value is separated from it by exactly one
space, unless [`--toml-align-comments`](#alignment) pads it into a column. A
comment on its own line keeps its own line, indented to the level of
whatever it introduces. This includes the comments that sit just before a
closing bracket or brace, which are indented one level deeper than the closing
line:

```toml rf:fixed-point
a = [
    1,
    2
    # tail
]
```

Comments at the end of the document are kept; trailing whitespace alone is
dropped.

## Directives

A comment can take the formatter out of a stretch of the document. Everything
from the `off` marker through the `on` marker that closes it — both marker lines
included — is written back byte for byte, under every combination of the options
on this page.

```toml rf:fixed-point
[package]
name = "demo"

# fmt: off
matrix = [
  1,   2,   3,
  40,  50,  60,
]
# fmt: on

version = "1"
```

Three spellings are recognised, and each `off` is closed by its own `on` or by
any other:

| Off | On |
| --- | --- |
| `# fmt: off` | `# fmt: on` |
| `# taplo: fmt-off` | `# taplo: fmt-on` |
| `# rust-formatter: fmt-off` | `# rust-formatter: fmt-on` |

Spacing and case inside the marker are free — `#FMT:Off` is the same marker —
but the comment has to be *exactly* the marker, so
`# fmt: off — keep the columns` is an ordinary comment. A comment body longer
than 40 characters is prose whatever it spells, which is what keeps every
comment in a document off the marker-normalizing path. A `#` inside a string is
not a comment at all, so a marker written in one is text.

A marker counts wherever its comment is written, not only on a line of its own,
and the region it opens starts at the beginning of that line:

```toml rf:fixed-point
wide = [ 1,  2,  3 ] # fmt: off
```

That is deliberate. The formatter moves a comment written between `=` and its
value onto a line of its own, so a rule that asked for a line of its own would
read a different document on the second pass than on the first, and the output
would stop being a fixed point.

An `off` with no `on` after it runs to the end of the file, which is how a whole
document is exempted. An `on` with no `off` before it, and an `off` inside a
region already open, are both ignored.

`--toml-directives off` turns the whole mechanism off, so the markers become
ordinary comments again.

### What a region reaches

A directive is honoured at the granularity of a whole entry. A marker inside a
value freezes the entry that value belongs to, since a container's layout is
decided as one:

```toml rf:fixed-point
a = [
  1,
  # fmt: off
  2,   3,
  # fmt: on
  4,
]
```

Everything above — the whole `a = [ … ]` entry — is written back as it stands.

Outside the marked lines the formatter carries on, with three consequences worth
knowing:

- A frozen entry is a wall that [ordering](#ordering) never crosses. Entries
  before it and entries after it are still sorted, but none of them moves past
  it, because the comment block that carries the marker travels with the entry it
  introduces.
- `--toml-inline-tables section` promotes nothing out of a table that a region
  reaches into. A promoted section is written after every body line of the table
  it came from, which for an unclosed region is between the markers.
- The rest of a run is still the formatter's: blank lines above the `off` marker
  are still capped by `--toml-max-blank-lines`, the comment block below the `on`
  marker is still rewritten, and the entry that follows is still indented.

## Blank lines

A run of blank lines collapses to `--toml-max-blank-lines`, which is `1` by
default and may be `0`–`32`. A blank line is kept wherever it was: between
values, after a table header, before a comment block, and between two comment
blocks. A line holding only spaces or tabs is a blank line.

The interior of a wrapped array or inline table takes no blank lines at all,
whatever the maximum is.

Blank lines are never *inserted* unless you ask:
`--toml-blank-line-before-tables` puts exactly one blank line before every table
header, including each `[[array]]` element, except the header that opens the
document. The blank goes above the header's comment block, not between the block
and the header.

```toml rf:fixed-point
[a]
x = 1

# note
[b]
y = 2
```

## Alignment

Both alignment flags are off by default, and both are applied to the finished
text once every layout decision is settled. That is why they never change what
wraps — and why the padding they add can leave a line past the
[width budget](#the-width-budget), the same way a value too wide to break can.

`--toml-align-entries` pads keys so that the `=` of neighbouring entries lines
up. A group is a run of consecutive entry lines at the same indentation; a blank
line, a table header, a value written across lines, or anything that is not an
entry ends it.

```toml rf:fixed-point --toml-align-entries
name    = "demo"
version = "0.1.0"
```

`--toml-align-comments` pads lines so that the `#` of neighbouring same-line
comments lines up. A group is a run of consecutive lines that each carry one; a
line without a comment, a comment on a line of its own, and the interior of a
multi-line string all end it.

```toml rf:fixed-point --toml-align-comments
bmp = []       # lossless
webp = ["dep"] # lossy
```

Neither flag looks inside a string, so a `#` or an `=` there is left alone.

## Keys

Whitespace inside a key path and inside a table header's brackets is always
normalized, whatever the options say, the same way the spacing around an `=` is.
Only *quoting* is preserved by default.

```toml rf:skip arrow table, not a formatter run
a  .  b  .  c = 1   ->   a.b.c = 1
[  a  .  b  ]       ->   [a.b]
[[  c  ]]           ->   [[c]]
```

Nothing else about the key changes: `"quoted" = 1` keeps its quotes and
`'literal' = 2` keeps its own.

`--toml-normalize-keys` removes quotes wherever the TOML spec allows a bare key,
and does nothing else. A key that must stay quoted keeps the quoting style the
author chose:

```toml rf:skip arrow table, not a formatter run
"quoted" = 1        ->   quoted = 1
'literal' = 2       ->   literal = 2
"1234" = 3          ->   1234 = 3
'lit key' = 4       ->   'lit key' = 4     (a space is not allowed bare)
"" = 5              ->   "" = 5
```

It reaches table headers, array-of-tables headers, dotted paths and inline-table
entries alike. Comments above and beside the key are unaffected.

## Ordering

Key order is preserved by default, and so is the order of table headers. Every
flag below is off unless it is asked for, and every one of them acts on the
shape of the document rather than on the file name, so they apply to any TOML
file with a table of that shape. (`--full-versions` is the exception: it only
runs on a file named `Cargo.toml`, because it resolves names against a registry
-- see [docs/versions.md](versions.md).)

| Flag | What it orders |
| --- | --- |
| `--sort-deps` | keys of every dependency table |
| `--sort-package` | fields of `[package]` and `[workspace.package]` |
| `--sort-dep-fields` | fields inside one dependency entry |
| `--sort-features` | keys of `[features]` |
| `--sort-arrays` | the array values cargo reads as a set |
| `--sort-targets` | `[[bin]]`, `[[example]]`, `[[test]]`, `[[bench]]` |
| `--sort-tables` | the top-level table sequence |
| `--sort-keys` | keys of every other section |
| `--sort-grouped` | modifier: every sort above stays inside blank-line groups |

### Version sort

Every sort here uses the version sort the Rust Style Guide defines, not raw byte
order. Four rules separate them:

- a run of digits compares by numeric value, so `x8` sorts before `x16` before
  `x32`; runs of equal value but different length, such as `00` and `0`, only
  matter when nothing else separates the keys, and then the one with more
  leading zeros sorts first (`v00` before `v0`);
- a space sorts before every other character, and `_` before everything but a
  space, digits included;
- an uppercase or non-letter character sorts before a lowercase one;
- otherwise characters compare by code point.

```
_x  Bee  Zed  aaa
```

Keys the rule cannot separate keep the order they were written in: every sort is
stable.

### Dependencies

`--sort-deps` sorts every dependency table: `[dependencies]`,
`[dev-dependencies]`, `[build-dependencies]`, `[workspace.dependencies]`, any
`[target.'cfg(…)'.…]` form of them, `[replace]`, and each table under `[patch]`
— `[patch.crates-io]` holds dependencies, `[patch]` itself holds registries.
Entries written as `[dependencies.serde]` headers are reordered along with the
plain ones, and keep the slots the group already occupied in the document.

`--sort-dep-fields` orders the fields *inside* one entry, source first and then
what is built from it:

> `package`, `version`, `registry`, `registry-index`, `path`, `git`, `branch`,
> `tag`, `rev`, `features`, `optional`, `default-features`, `workspace`

It reaches all three spellings of an entry — the `[dependencies.serde]` header,
the `serde.version = "1"` dotted form and the `serde = { version = "1" }` inline
table. Fields not on the list keep their relative order after the ones that are.

### `[package]`

`--sort-package` puts `[package]` and `[workspace.package]` into a canonical
field order — **not** alphabetical order. The two published orders disagree, so
`--package-order` picks between them.

`book` (the default) is cargo's own manifest-reference sequence, with `edition`
and `rust-version` hoisted ahead of `authors` because that is the order the
ecosystem actually writes:

> `name`, `version`, `edition`, `rust-version`, `authors`, `description`,
> `documentation`, `readme`, `homepage`, `repository`, `license`,
> `license-file`, `keywords`, `categories`, `workspace`, `build`, `links`,
> `exclude`, `include`, `publish`, `metadata`, `default-run`, `autolib`,
> `autobins`, `autoexamples`, `autotests`, `autobenches`, `resolver`

Fields not on that list keep their relative order after the ones that are.

`style-guide` is the normative rule from the Rust Style Guide: `name`, then
`version`, then every other field version-sorted, then `description` **last**.

A `[[package]]` array of tables — the shape a `Cargo.lock` uses — is left alone
by either order.

### Features, arrays and targets

`--sort-features` version-sorts the keys of `[features]`. `--sort-arrays` sorts
the *values*, and every other array cargo reads as a set rather than a sequence:

| Section | Keys |
| --- | --- |
| `[features]` | every value |
| a dependency entry | `features` |
| `[package]` | `keywords`, `categories`, `exclude`, `include` |
| `[workspace]` | `members`, `default-members`, `exclude` |

`authors` is deliberately absent: the Cargo Book gives its order meaning.

An array is only sorted when every element is a string and no comment sits
inside it. Reordering past an interior comment would move the comment away from
what it describes, so a commented array is left alone entirely.

`--sort-targets` orders `[[bin]]`, `[[example]]`, `[[test]]` and `[[bench]]` at
the document root by each section's `name`, with a missing `name` treated as
empty. Each section keeps the blank lines and comments it was written with.

### The document sequence

`--sort-tables` orders the top-level tables:

> `package`, `lib`, `bin`, `example`, `test`, `bench`, `features`,
> `dependencies`, `dev-dependencies`, `build-dependencies`, `target`, `badges`,
> `lints`, `workspace`, `profile`, `patch`, `replace`

This is the Cargo Book's chapter sequence, with `[features]` pulled ahead of the
dependency tables. Tables not on the list keep their relative order after the
ones that are, and each table moves as a whole block — its sub-tables, comments
and internal blank lines travel with it.

`--sort-keys` version-sorts the keys of every section a more specific flag has
not claimed. `[package]` has its own order and the document sequence is
`--sort-tables`' job, so neither falls through to it.

### Grouping

`--sort-grouped` makes every sort above stay inside blank-line-separated runs
instead of reordering across them, which is how a hand-partitioned dependency
list survives sorting:

```toml rf:skip two-column before/after, not a formatter run
# before                      # after --sort-deps --sort-grouped
[dependencies]                [dependencies]
zzz = "1"                     mmm = "2"
mmm = "2"                     zzz = "1"

bbb = "3"                     aaa = "4"
aaa = "4"                     bbb = "3"
```

The blank lines themselves do not move, so the groups keep their sizes.
`--sort-grouped` cannot be combined with `--toml-max-blank-lines 0`, which would
delete the blank lines it reads.

### What moves with what

Sorting moves a comment block with the entry it introduces. Vertical spacing is
positional and stays where it was: the blank line under a `[header]` belongs to
the header rather than to whichever entry happened to sit there, and so do the
blank lines between header blocks.

```toml rf:skip two-column before/after, not a formatter run
# before                      # after --sort-deps
[dependencies]                [dependencies]

zzz = "1"                     # about aaa
# about aaa                   aaa = "2"
aaa = "2"                     zzz = "1"
```

## The Rust Style Guide

`doc.rust-lang.org/style-guide/cargo.html` is a normative chapter of the Rust
Style Guide covering `Cargo.toml`. `--style-guide` is that chapter as a preset:

```
--toml-indent 4                  --toml-max-width 100
--toml-arrays auto               --toml-inline-tables section
--toml-trailing-comma multiline  --toml-max-blank-lines 0
--toml-blank-line-before-tables  --toml-normalize-keys
--sort-package --package-order style-guide
--sort-deps --sort-features --sort-arrays
--sort-targets --sort-tables --sort-keys
```

`--toml-blank-line-before-tables` together with `--toml-max-blank-lines 0` is
the guide's "a blank line before each section header, none between key-value
pairs": the cap runs first and the header blank is re-added afterwards.

A flag typed on the command line always wins, so `--style-guide
--toml-max-width 60` is 60 columns, and `--style-guide --no-sort-keys` (or
`--sort-keys=false`) keeps every rule of the preset but that one.

Two things the preset deliberately leaves out:

- `--sort-dep-fields`. The guide asks for version-sorted keys within each
  section, which is `--sort-keys`; a semantic field order is a different rule.
- `--cargo-conventions`. It turns a table into a string, which changes the
  value tree beyond any reordering ([Guarantee 2](#guarantees)), so it stays
  opt-in. It turns `dep = { version = "1" }` into `dep = "1"` inside a
  dependency table, merging any trailing comment onto the shorter line, and
  leaves an entry with more than one field alone.

## TOML 1.0 and TOML 1.1

A TOML 1.0 parser — `python -m tomllib`, an older cargo, some third-party
tooling — rejects five things this formatter's dependency accepts:

| construct | example |
| --- | --- |
| an inline table written across lines | a newline between `{` and `}` |
| a trailing comma in an inline table | `a = { b = 1, }` |
| the `\e` escape | `"\e[0m"` |
| the `\xHH` escape | `"\x41"` |
| a time with its seconds omitted | `07:32` |

The first two are layout, so the formatter decides them. The last three are
value spellings, and [Guarantee 5](#guarantees) says those survive verbatim —
the formatter will not rewrite one, and neither will it silently pass one off as
1.0.

`--toml-inline-tables compact` keeps every inline table on one line, so the
formatter **adds no 1.1 construct of its own**. A container on one line also
forces its nested arrays inline, so a wide array cannot smuggle a newline in.
It says nothing about what the input already carried.

`--toml-version 1.0` is the checked guarantee. It

- pins every inline table to one line, overriding `--toml-inline-tables`;
- withholds the inline-table trailing comma that `--toml-trailing-comma
  multiline` would otherwise add — arrays keep theirs, since a multi-line array
  with a trailing comma is TOML 1.0;
- reports every 1.1-only construct still in the output, as a warning naming the
  file, line and column. Positions are read against the *formatted* text, which
  is the text the guarantee is about.

A warning never changes the exit code: it is a finding about the document, not a
formatting verdict. In `--message-format json` the findings join the top-level
`warnings` array.

`--toml-version 1.0` cannot be combined with `--toml-inline-tables expand`,
which asks for the one construct it forbids.

One case no option can fix is a comment inside the braces — on an entry, or on
an element of an array nested in one. It cannot be written on one line at all,
so the formatter breaks the table rather than delete the comment, and reports
the break.

A multi-line string inside a one-line inline table is deliberately *not* a
finding. TOML 1.0 allows a newline between the braces when it is part of a
value.

The defaults are 1.1: any inline table wider than the budget wraps.

## Option reference

Every option below can also be set by a [configuration
source](configuration.md) -- a `[workspace.metadata.rust-formatter]` table, a
`rust-formatter.toml`, or a `RUST_FORMATTER_*` variable -- under the flag's own
name without the dashes. A flag listed as `flag` also takes a value, so one a
configuration source turned on can be turned off for a single run:
`--sort-deps=false` or `--no-sort-deps`.

`--style-guide` is the exception on both counts: it takes no value and is not a
configuration key. It names the preset, which a configuration source spells
`preset = ["style-guide"]` (or `["cargo"]`).

| Flag | Values | Default |
| --- | --- | --- |
| `--toml-indent WIDTH` | `0`–`16`, or `tab` | `4` |
| `--toml-tab-width N` | `1`–`16` | `4` |
| `--toml-max-width N` | `1`–`4096` | `100` |
| `--toml-arrays STYLE` | `preserve`, `auto`, `expand` | `preserve` |
| `--toml-inline-tables STYLE` | `auto`, `compact`, `expand`, `section` | `auto` |
| `--toml-version VERSION` | `1.0`, `1.1` | `1.1` |
| `--toml-directives WHEN` | `on`, `off` | `on` |
| `--toml-array-spacing STYLE` | `compact`, `spaced` | `compact` |
| `--toml-inline-table-spacing STYLE` | `compact`, `spaced` | `spaced` |
| `--toml-trailing-comma WHEN` | `never`, `multiline` | `never` |
| `--toml-blank-line-before-tables` | flag | off |
| `--toml-max-blank-lines N` | `0`–`32` | `1` |
| `--toml-align-entries` | flag | off |
| `--toml-align-comments` | flag | off |
| `--toml-indent-tables` | flag | off |
| `--toml-indent-entries` | flag | off |
| `--toml-normalize-keys` | flag | off |
| `--sort-deps` | flag | off |
| `--sort-package` | flag | off |
| `--package-order ORDER` | `book`, `style-guide` | `book` |
| `--sort-dep-fields` | flag | off |
| `--sort-features` | flag | off |
| `--sort-arrays` | flag | off |
| `--sort-targets` | flag | off |
| `--sort-tables` | flag | off |
| `--sort-keys` | flag | off |
| `--sort-grouped` | flag | off |
| `--cargo-conventions` | flag | off |
| `--style-guide` | no value; the `style-guide` preset | off |
| `--preset NAME` | `default`, `cargo` / `style-guide`, `compact`, `expand`, `everything`, `narrow-tabs`, `aligned-indented`, `comments`, `literals`, `strict` | none |
