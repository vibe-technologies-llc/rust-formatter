#!/bin/sh
# Differential and idempotence sweep of rust-formatter over the local crate registry.
#
#   scripts/corpus.sh
#   N=8000 scripts/corpus.sh                 # .toml sample size
#   RS_N=60 scripts/corpus.sh                # crate directories for the Rust leg
#   RS=0 scripts/corpus.sh                   # skip the Rust leg entirely
#   REF=HEAD~1 scripts/corpus.sh             # git ref to compare against
#   KEEP=1 scripts/corpus.sh                 # keep the work tree even on success
#   PRESETS='default cargo' scripts/corpus.sh
#   STYLE='--sort-deps' scripts/corpus.sh    # raw flags instead of the presets
#   RUST_FORMATTER_CORPUS_DIR=... scripts/corpus.sh
#
# Builds a reference binary from a git ref in a throwaway worktree, runs it and
# the working-tree binary over identical copies of the corpus, and diffs the
# results. Then runs the working-tree binary twice over a third copy to prove
# idempotence at corpus scale, and once more with --list-different to prove the
# converged tree is clean.
#
# Every style runs the whole trio, and the default style list is the same seven
# presets tests/support/styles.rs uses, so the shell sweep and the in-process
# property and corpus suites cover the same points of the option surface.
#
# The differential leg survives a style: the reference binary is asked whether it
# accepts the flags first, and only the styles it rejects -- flags that postdate
# REF -- drop to idempotence alone.
#
# The Rust leg samples whole crate directories rather than loose files, so both
# transports are exercised: the parent directory is a loose walk, and each crate
# directory on its own is a cargo project with an edition to resolve.
#
# The reference is always built from a git ref, never taken from
# ~/.cargo/bin/rust-formatter: an installed binary lags HEAD and reports its own
# staleness as a difference.
#
# Exit: 0 pass, 1 differences found, 77 corpus unavailable.

set -eu

N="${N:-3000}"
RS_N="${RS_N:-25}"
RS="${RS:-1}"
REF="${REF:-HEAD}"
CORPUS="${RUST_FORMATTER_CORPUS_DIR:-${CARGO_HOME:-$HOME/.cargo}/registry/src}"
KEEP="${KEEP:-0}"
STYLE="${STYLE:-}"
PRESETS="${PRESETS:-default compact expand everything cargo narrow-tabs aligned-indented}"
RS_PRESETS="${RS_PRESETS:-default comments literals strict}"

if [ ! -d "$CORPUS" ]; then
    printf 'no corpus at %s (set RUST_FORMATTER_CORPUS_DIR)\n' "$CORPUS" >&2
    exit 77
fi

repo=$(git rev-parse --show-toplevel)
work=$(mktemp -d "${TMPDIR:-/tmp}/rf-corpus.XXXXXX")
fail=0

cleanup() {
    status=$?
    git -C "$repo" worktree remove --force "$work/ref" >/dev/null 2>&1 || true
    if [ "$KEEP" = 1 ] || [ "$status" -ne 0 ] || [ "$fail" -ne 0 ]; then
        printf '\nartifacts kept in %s\n' "$work" >&2
    else
        rm -rf "$work"
    fi
}
trap cleanup EXIT

say() { printf '==> %s\n' "$1"; }
note() { printf '    %s\n' "$1"; }
bad() { printf 'FAIL: %s\n' "$1" >&2; fail=1; }

say "building reference binary from $REF"
git -C "$repo" worktree add --detach "$work/ref" "$REF" >/dev/null
cargo build --release --quiet \
    --manifest-path "$work/ref/Cargo.toml" \
    --target-dir "$work/ref-target"
ref_bin="$work/ref-target/release/rust-formatter"

say "building working-tree binary"
cargo build --release --quiet --manifest-path "$repo/Cargo.toml"
new_bin="$repo/target/release/rust-formatter"

# Whether the reference binary understands these flags at all. A style made of
# flags that postdate REF is not a regression, so it drops the differential leg
# rather than failing the sweep.
# shellcheck disable=SC2086
ref_accepts() {
    "$ref_bin" $1 --print-settings >/dev/null 2>&1
}

# `$1 label` `$2 style flags` `$3 tree` `$4 binary`. Leaves the exit code in
# `code` and the path-normalised stderr in `$work/$1.err.norm`, so the two trees
# compare despite sitting at different paths.
# shellcheck disable=SC2086
sweep_run() {
    code=0
    "$4" $2 "$3" >/dev/null 2>"$work/$1.err" || code=$?
    sed "s#$3#CORPUS#g" "$work/$1.err" | LC_ALL=C sort > "$work/$1.err.norm"
}

# The whole trio for one style over one prepared corpus directory `$1`, whose
# a/ b/ c/ copies already exist. `$2` is the style flags, `$3` a label.
one_style() {
    root=$1
    spec=$2
    label=$3

    if ref_accepts "$spec"; then
        sweep_run "ref" "$spec" "$root/a" "$ref_bin"
        ref_code=$code
        sweep_run "new" "$spec" "$root/b" "$new_bin"
        new_code=$code

        if [ "$ref_code" != "$new_code" ]; then
            bad "$label: exit codes differ (ref=$ref_code new=$new_code)"
        fi
        if diff -rq "$root/a" "$root/b" > "$work/output.diff" 2>&1; then
            note "$label: output identical to $REF"
        else
            bad "$label: $REF and the working tree produce different output"
            head -n 40 "$work/output.diff" >&2
        fi
        if diff -u "$work/ref.err.norm" "$work/new.err.norm" > "$work/stderr.diff"; then
            note "$label: stderr identical to $REF"
        else
            bad "$label: stderr differs from $REF"
            head -n 40 "$work/stderr.diff" >&2
        fi
    else
        note "$label: $REF does not accept these flags, differential leg skipped"
    fi

    # shellcheck disable=SC2086
    "$new_bin" $spec "$root/c" >/dev/null 2>&1 || true
    rm -rf "$root/c.pass1"
    cp -a "$root/c" "$root/c.pass1"
    # shellcheck disable=SC2086
    "$new_bin" $spec "$root/c" >/dev/null 2>&1 || true

    if diff -rq "$root/c.pass1" "$root/c" > "$work/idem.diff" 2>&1; then
        note "$label: stable after the second pass"
    else
        bad "$label: formatting is not idempotent"
        head -n 40 "$work/idem.diff" >&2
    fi

    # Names, not diffs: the list is the whole answer and needs no parsing.
    # shellcheck disable=SC2086
    "$new_bin" $spec --list-different "$root/c" >"$work/check.out" 2>/dev/null || true
    if [ -s "$work/check.out" ]; then
        bad "$label: --check still flags files after two write passes"
        head -n 20 "$work/check.out" >&2
    else
        note "$label: --check is clean"
    fi
}

# ------------------------------------------------------------------ TOML leg

say "sampling up to $N .toml files from $CORPUS"
find "$CORPUS" -type f -name '*.toml' | LC_ALL=C sort > "$work/all.list"
total=$(wc -l < "$work/all.list" | tr -d ' ')

# Stride rather than head: a prefix of a sorted list is one alphabetical
# neighbourhood and covers only a handful of crate families.
awk -v want="$N" -v total="$total" '
    BEGIN { step = (total > want && want > 0) ? int(total / want) : 1 }
    (NR - 1) % step == 0 && kept < want { print; kept++ }
' "$work/all.list" > "$work/list"

mkdir -p "$work/toml/pristine"
i=0
# Flat NNNNNN.toml names: detect_target promotes any directory under a
# Cargo.toml to a cargo project, and the walker skips Cargo.lock / clippy.toml /
# rustfmt.toml by name. Renaming avoids both so coverage stays total.
while IFS= read -r file; do
    i=$((i + 1))
    cp -- "$file" "$(printf '%s/toml/pristine/%06d.toml' "$work" "$i")"
done < "$work/list"
note "$i of $total files copied"

if [ "$i" -eq 0 ]; then
    printf 'no files sampled\n' >&2
    exit 77
fi
chmod -R u+w "$work/toml/pristine"

# A work tree inside a git repository that has a Cargo.toml is detected as that
# cargo project, and a corpus copy is not one of its members -- so every leg
# formats nothing and reports "identical, stable, clean" having proved nothing.
# Refuse rather than pass vacuously.
say "checking the work tree is formattable"
probe="$work/probe"
rm -rf "$probe"
mkdir -p "$probe"
printf 'rf_probe={path="x"}\n' > "$probe/000001.toml"
"$new_bin" "$probe" >/dev/null 2>&1 || true
if ! grep -q 'rf_probe.path = "x"' "$probe/000001.toml"; then
    printf 'the work tree at %s is not formatted as a loose directory.\n' "$work" >&2
    printf 'TMPDIR is probably inside a cargo project; point it somewhere else.\n' >&2
    exit 77
fi
note "a loose directory, as the sweep needs"

if [ -n "$STYLE" ]; then
    specs="$STYLE"
    spec_kind=raw
else
    specs="$PRESETS"
    spec_kind=preset
fi

for spec in $specs; do
    if [ "$spec_kind" = preset ]; then
        flags="--preset $spec"
        label="toml/$spec"
    else
        flags="$STYLE"
        label="toml/style"
    fi
    say "$label"
    for copy in a b c; do
        rm -rf "$work/toml/$copy"
        cp -a "$work/toml/pristine" "$work/toml/$copy"
    done
    one_style "$work/toml" "$flags" "$label"
    if [ "$spec_kind" = raw ]; then
        break
    fi
done

# ------------------------------------------------------------------ Rust leg

if [ "$RS" != 1 ]; then
    say "Rust leg skipped (RS=$RS)"
elif ! command -v rustfmt >/dev/null 2>&1 && ! command -v rustup >/dev/null 2>&1; then
    say "Rust leg skipped (no rustfmt and no rustup)"
else
    say "sampling up to $RS_N crate directories from $CORPUS"
    find "$CORPUS" -mindepth 2 -maxdepth 2 -type d | LC_ALL=C sort > "$work/crates.all"
    crate_total=$(wc -l < "$work/crates.all" | tr -d ' ')
    awk -v want="$RS_N" -v total="$crate_total" '
        BEGIN { step = (total > want && want > 0) ? int(total / want) : 1 }
        (NR - 1) % step == 0 && kept < want { print; kept++ }
    ' "$work/crates.all" > "$work/crates.list"

    mkdir -p "$work/rs/pristine"
    j=0
    while IFS= read -r dir; do
        [ -f "$dir/Cargo.toml" ] || continue
        j=$((j + 1))
        cp -a -- "$dir" "$(printf '%s/rs/pristine/%03d' "$work" "$j")"
    done < "$work/crates.list"
    note "$j of $crate_total crate directories copied"

    if [ "$j" -eq 0 ]; then
        say "Rust leg skipped (no crate directories sampled)"
    else
        # Registry sources are checked out read-only, and a write run has to be
        # able to replace a file.
        chmod -R u+w "$work/rs/pristine"
        # The walker skips any directory holding a .cargo-checksum.json, which
        # every registry checkout has. Without this the Rust leg would format
        # nothing and pass in silence.
        find "$work/rs/pristine" -name .cargo-checksum.json -delete

        for spec in $RS_PRESETS; do
            flags="--preset $spec"
            say "rust/$spec (loose walk)"
            for copy in a b c; do
                rm -rf "$work/rs/$copy"
                cp -a "$work/rs/pristine" "$work/rs/$copy"
            done
            one_style "$work/rs" "$flags" "rust/$spec"
        done

        # Each crate directory on its own is a cargo project, which is the other
        # transport: the edition comes from its manifest and --all is emulated
        # from cargo metadata. The loose walk above never reaches that code.
        say "rust/default (per-crate cargo projects)"
        mkdir -p "$work/rsp"
        for copy in a b c; do
            rm -rf "$work/rsp/$copy"
            cp -a "$work/rs/pristine" "$work/rsp/$copy"
        done
        for copy in a b; do
            bin=$ref_bin
            [ "$copy" = b ] && bin=$new_bin
            for crate in "$work/rsp/$copy"/*; do
                "$bin" "$crate" >/dev/null 2>>"$work/rsp-$copy.err" || true
            done
        done
        if diff -rq "$work/rsp/a" "$work/rsp/b" > "$work/rsp.diff" 2>&1; then
            note "rust/project: output identical to $REF"
        else
            bad "rust/project: $REF and the working tree produce different output"
            head -n 40 "$work/rsp.diff" >&2
        fi
        for crate in "$work/rsp/c"/*; do
            "$new_bin" "$crate" >/dev/null 2>&1 || true
        done
        cp -a "$work/rsp/c" "$work/rsp/c.pass1"
        for crate in "$work/rsp/c"/*; do
            "$new_bin" "$crate" >/dev/null 2>&1 || true
        done
        if diff -rq "$work/rsp/c.pass1" "$work/rsp/c" > "$work/rsp-idem.diff" 2>&1; then
            note "rust/project: stable after the second pass"
        else
            bad "rust/project: formatting is not idempotent"
            head -n 40 "$work/rsp-idem.diff" >&2
        fi
    fi
fi

if [ "$fail" -ne 0 ]; then
    printf '\nCORPUS SWEEP FAILED\n' >&2
    exit 1
fi
printf '\nCORPUS SWEEP PASSED (%d toml files, ref=%s)\n' "$i" "$REF"
