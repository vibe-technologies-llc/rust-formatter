# Dependency versions

`--full-versions` resolves the dependency requirements in a `Cargo.toml` against
a registry index and writes them out in full: `serde = "1"` becomes
`serde = "1.0.229"`. It is opt-in, it only ever runs on a file literally named
`Cargo.toml`, and it never changes anything else about how a document is
formatted.

This page is the specification. The flag summary lives in the
[README](../README.md#dependency-versions).

## Guarantees

For any manifest, under any combination of the flags below:

1. A requirement is only ever rewritten to one the registry actually offers.
2. A rewrite never changes which major version cargo would resolve, unless
   `--upgrade-incompatible` was passed.
3. A rewrite never raises the manifest's `rust-version`, unless
   `--ignore-rust-version` was passed.
4. Every dependency the run considered is accounted for: it was completed,
   already complete, or left alone with a stated reason. Nothing is silent.
5. A dependency that does not come from the registry is never touched.
6. A failure of the registry itself stops the run **before any file is written**.
   A dependency the registry does not have is a skip, and the rest of the run
   proceeds.

## What gets rewritten

A requirement is eligible when it is a **single comparator** whose operator
anchors a version rather than bounding one, and it does not name a pre-release.

| Requirement | Meaning to cargo | Completed to | Note |
| --- | --- | --- | --- |
| `1` | `>=1.0.0, <2.0.0` | `1.7.3` | highest release in the range |
| `1.2` | `>=1.2.0, <2.0.0` | `1.7.3` | a bare requirement is a caret |
| `^1.2` | `>=1.2.0, <2.0.0` | `^1.7.3` | the caret is kept |
| `~1.2` | `>=1.2.0, <1.3.0` | `~1.2.9` | the tilde bounds the minor |
| `~1` | `>=1.0.0, <2.0.0` | `^1.7.3` | a major-only tilde bounds the major, as a caret does |
| `=1.2` | `>=1.2.0, <1.3.0` | `=1.2.9` | **narrows** — see below |
| `0.4` | `>=0.4.0, <0.5.0` | `0.4.33` | a `0.x` caret bounds the minor |
| `0.0`, `^0.0` | `>=0.0.0, <0.1.0` | `~0.0.7` | `0.0.7` would mean `=0.0.7`, so a tilde keeps the minor |
| `1.*`, `1.x` | `>=1.0.0, <2.0.0` | `1.7.3` | the wildcard is replaced |
| `1.2.*`, `1.2.x` | `>=1.2.0, <1.3.0` | `~1.2.9` | a patch wildcard is a tilde, not a caret |
| `1.2.3` | `>=1.2.3, <2.0.0` | *(unchanged)* | already complete; see `--upgrade` |

`^1.0` resolving to `^1.7.3` rather than `^1.0.230` is deliberate: cargo's caret
admits any `1.x`, so picking the newest `1.0.x` would name an older release than
the one cargo will actually resolve.

The requirement is written as `major.minor.patch` with the operator that keeps
cargo's range: the original one, except where the table shows a different one.
Apart from `=`, a rewrite only raises the lower bound and never moves the upper
one. A pre-release or `+build` suffix on the chosen release is dropped,
because cargo ignores build metadata when matching: `toml_edit` publishes its
releases with a suffix, as in `0.25.15+spec-1.1.0`, and the requirement written
for that release is `0.25.15`.

### `=` narrows

`=4.6` means "any `4.6.x`" to cargo, not "exactly 4.6.0". Completing it to
`=4.6.6` therefore **tightens** the requirement rather than filling it in. This
is intentional and is what a pinned requirement is usually for, but it is a
change of meaning and not just of spelling.

### What is not rewritten

| Requirement | Why |
| --- | --- |
| `*` | there is no major version to complete |
| `0`, `^0`, `~0`, `0.*`, `0.x` | `<1.0.0` admits every `0.x`; no single `x.y.z` comparator does |
| `>=1, <2` | a multi-comparator range already names its bounds |
| `>=1.0`, `<2` | a `<`/`>` comparator already names its bound |
| `1.0.0-rc.1` | the requirement names a pre-release |

Each of these is reported rather than passed over, because "the flag did
nothing" and "the flag decided not to" are different answers.

## What is skipped, and why

Skips fall into two classes. A **notable** skip means the run could not do what
it was asked, and always prints a `warning:` line. A **routine** skip is ordinary
and is counted in the summary, listed under `--verbose`, and always present in
`--message-format json`.

| Reason | Class |
| --- | --- |
| `path`, `git`, `registry`, `registry-index` dependency | routine |
| `workspace = true` (the version is inherited) | routine |
| a `[patch.*]` or `[replace]` entry | routine |
| inside a `# fmt: off` region | routine |
| the requirement already names `x.y.z` | routine, and not counted |
| an `=` requirement under `--upgrade` without `--upgrade-pinned` | routine |
| the requirement cannot be completed (`*`, a bare `0`, a range, a bound) | **notable** |
| no such crate on the registry | **notable** |
| not a legal crate name | **notable** |
| no release satisfies the requirement | **notable** |
| every matching release is yanked | **notable** |
| every matching release is a pre-release | **notable** |
| every matching release needs a newer compiler | **notable** |
| offline and not in the local index | **notable** |
| crates.io is replaced by another source | **notable** |

A requirement that was already complete is the ordinary case, so it is listed
under `--verbose` and present in the JSON but not counted as something the run
left alone. When there is nothing to complete and nothing was left alone, no
summary is printed at all.

Otherwise the run ends with one summary line naming the counts:

```
--full-versions: 4 completed, 11 left alone (1 rust-version, 1 path, 1 git,
1 workspace-inherited, 2 other registry, 3 not completable, 1 unknown crate, 1 patch)
```

Under `--check` or `--verbose`, each completion is also named as a *version*
finding rather than showing up only inside the file's unified diff:

```
version: Cargo.toml:8:1: serde "1" completes to "1.0.229"
```

### `[patch]` and `[replace]` are out of scope

Cargo requires a `[patch]` entry to point at a *different* source from the one it
patches, so the `version` in a patch entry constrains which version of that other
source is acceptable. Completing it against crates.io would narrow a constraint
on a source this tool never looked at. `[replace]` is deprecated by cargo. Both
are visited so that they can be reported, and neither is ever rewritten.

## Which sections are read

`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`,
`[workspace.dependencies]` and every `[target.'cfg(…)'.…]` variant of them, in
both the inline (`serde = "1"`, `serde = { version = "1" }`) and the table
(`[dependencies.serde]`) form. A `package = "real-name"` rename is resolved, so
the lookup uses the crate's real name and the record reports it. A table named
`dependencies` anywhere else, such as under `[package.metadata]`, is not a cargo
dependency section and is never read.

## The minimum supported Rust version

A pin must not silently raise the compiler a project needs. Candidates whose
index entry declares a `rust_version` above the manifest's own are filtered out,
and if that leaves nothing the dependency is skipped with a `rust-version`
reason.

The manifest's value is taken from `[package] rust-version`. When that is
`rust-version.workspace = true`, or the package declares no `rust-version` at
all, it comes from `[workspace.package] rust-version` in the same document, and
failing that from the workspace root's manifest; a manifest with no `[package]`
table reads the workspace value the same way. A manifest with no
`rust-version` anywhere is unbounded.

`--ignore-rust-version` turns the filter off, matching cargo's flag of the same
name.

## Upgrading

`--full-versions` never touches a requirement that already names `x.y.z`, so
bumping to a newer release is a separate opt-in. The vocabulary is cargo-edit's.

| Flag | Effect |
| --- | --- |
| `--upgrade` | also rewrite complete requirements, to the newest release the requirement still admits |
| `--upgrade-incompatible` | allow a bump that breaks the requirement, e.g. `^1.7.3` → `^2.0.1`, but never to a release below the requirement's own lower bound |
| `--upgrade-pinned` | include `=` requirements, which are otherwise left alone |

`--upgrade-incompatible` and `--upgrade-pinned` both require `--upgrade`.

### Interaction with `cargo upgrade`

`cargo upgrade` deliberately *preserves* precision: its own tests upgrade `"1.0"`
to `"1.1"`, staying two-component. `--full-versions` expands `"4.6"` to
`"4.6.6"`. The two will fight over any repository that runs both.

`cargo add`, by contrast, writes full three-component versions with no caret,
which is exactly what this tool writes. If a requirement is deliberately
imprecise, wrap it in a `# fmt: off` / `# fmt: on` pair: a frozen dependency is
never rewritten and never even costs a request.

```toml rf:fixed-point
[dependencies]
# fmt: off
# kept two-component on purpose, `cargo upgrade` owns this one
serde = "1.0"
# fmt: on
```

## Yanked releases

By default a yanked release is never chosen: candidates the index marks as
yanked are dropped before the newest is picked, and a requirement whose every
match is yanked is skipped with a `yanked` reason. `--allow-yanked` keeps them
in the running, so the choice is made among yanked and unyanked releases alike.
It requires `--full-versions`.

## Where versions come from

The crates.io **sparse index** at `https://index.crates.io`. The crates.io
data-access policy lists the available methods in order and puts the sparse index
first: it needs no user agent, imposes no rate limit, is served from a CDN,
answers `If-None-Match` with a `304`, and is not paginated. (The API this once
used is the policy's last choice, allows one request per second, and *is*
paginated — a 30-dependency manifest would have taken 30 seconds and could still
have read a truncated version list.)

Entries are JSON Lines in **publish order, not semver order**, so the newest
release is not the last line and the maximum has to be taken.

### Resolution order for one crate

1. `--offline`, `CARGO_NET_OFFLINE` or `net.offline`: nothing is fetched. This
   tool's own cache is read first, then cargo's on-disk index under
   `$CARGO_HOME/registry/index/…/.cache`. A crate in neither is skipped.
2. This tool's own cache, if it holds the crate. Its stored `ETag` is sent as
   `If-None-Match`, so an unchanged crate costs a `304` and no body.
3. Cargo's on-disk index, read but never written. Its stored `ETag` seeds the
   conditional request when this tool has no copy of its own.
4. A `GET` against the index.

The cache lives outside cargo's directories, under
`$XDG_CACHE_HOME/rust-formatter` (`~/Library/Caches/rust-formatter` on macOS,
`%LOCALAPPDATA%\rust-formatter\cache` on Windows), in a subdirectory named after
the index host: `index.crates.io` for crates.io.
`RUST_FORMATTER_REGISTRY_CACHE_DIR` replaces the base directory, and the host
subdirectory is still used under it. A cache read or write that fails is never
an error: it costs a fetch.

### Which index

In precedence order:

1. `--registry-url <URL>`.
2. `RUST_FORMATTER_CRATES_IO_URL`. Plain HTTP is accepted only for a loopback
   address.
3. Cargo's `[source.crates-io] replace-with` chain. A chain that ends at a
   `registry = "sparse+https://…"` is followed and used. A chain that ends at a
   `directory`, `local-registry`, git registry, or non-sparse registry means the
   versions cargo resolves are not the ones crates.io holds, so **nothing is
   pinned**. The run says so once, and each dependency it would have looked up
   is also skipped with its own `crates.io is replaced` warning.
4. `https://index.crates.io`.

## Network behaviour

Everything below follows cargo's own settings, with the environment overriding
the merged `.cargo/config.toml` layers. As with cargo, those layers are the
`.cargo` directory of the formatted path's directory and of every directory
above it, then `$CARGO_HOME`; a relative path is resolved against the working
directory first. Where one directory holds both `config` and `config.toml`,
only the extensionless `config` is read, which is the file cargo uses.

| Setting | Config key | Environment | Default |
| --- | --- | --- | --- |
| Offline | `net.offline` | `CARGO_NET_OFFLINE` | off (`--offline` also sets it) |
| Retries | `net.retry` | `CARGO_NET_RETRY` | `3` |
| Timeout | `http.timeout` | `CARGO_HTTP_TIMEOUT` | `30` seconds |
| Proxy | `http.proxy` | `CARGO_HTTP_PROXY`, then `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` | none |
| CA bundle | `http.cainfo` | `CARGO_HTTP_CAINFO`, then `SSL_CERT_FILE` | the OS trust store |

- **TLS roots** are the platform's own, which is what libcurl gives cargo, so a
  corporate root installed on the machine is trusted. A PEM bundle named by
  `http.cainfo`, `CARGO_HTTP_CAINFO` or `SSL_CERT_FILE` replaces them. A
  handshake failure says so and names both variables.
- **Proxies** may be `http`, `https`, `socks5` or `socks5h`.
- **Retries** apply to a connection failure, a timeout, `408`, `429` and any
  `5xx`, with exponential backoff and full jitter. A `Retry-After` header in
  delta-seconds is honoured, capped at 60s. The HTTP-date form is not parsed:
  it is treated as a wait of the full 60s.
- **Redirects** are followed for at most two hops, and each hop must be `https`
  and on the same host as the one before it. The sparse protocol does not
  redirect; this is an accommodation for a mirror in front of one.
- **Concurrency** is the run's `-j` value. Requirements across every manifest in
  the run are resolved in one batch before formatting starts, and each crate is
  requested exactly once however many requirements name it.

## Exit codes

| Code | When |
| --- | --- |
| `0` | every dependency was completed or accounted for |
| `1` | `--check` found a manifest that needs rewriting |
| `2` | the registry itself failed: a transport error, a refused redirect, a TLS failure, an index body that did not parse, or any status other than `200`, `304`, `404` and `410` -- a `401` or `403` included, and a `408`, `429` or `5xx` once its retries are spent |

A `404` or `410`, an illegal crate name, an offline miss and a replaced source
are all skips and never change the exit code.

## Machine-readable output

`--message-format json` carries a `versions` array alongside `files`, `errors`
and `warnings`:

```json
{
  "versions": [
    {
      "path": "Cargo.toml",
      "crate": "serde",
      "section": "dependencies",
      "requirement": "1",
      "line": 8,
      "column": 1,
      "outcome": "completed",
      "completed": "1.0.229"
    },
    {
      "path": "Cargo.toml",
      "crate": "local",
      "section": "dependencies",
      "requirement": "",
      "line": 9,
      "column": 1,
      "outcome": "skipped",
      "reason": "a path dependency does not come from a registry"
    }
  ]
}
```

`outcome` is `completed`, `unchanged` or `skipped`. `completed` carries the new
requirement; `reason` carries the skip. `line` and `column` are absent when the
layout moved the entry out from under the parse the position was read from.
