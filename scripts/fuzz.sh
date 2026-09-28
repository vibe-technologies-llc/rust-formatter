#!/bin/sh
# Fuzz `format_toml` against the same oracles the test suites use.
#
#   scripts/fuzz.sh                # 5 minutes
#   SECONDS_TO_RUN=3600 scripts/fuzz.sh
#   JOBS=8 scripts/fuzz.sh
#
# Seeds the corpus from tests/fixtures/ first. The corpus is not committed --
# every seed is derived from a fixture, so it is cheaper to rebuild than to
# review, and libFuzzer's own additions are machine-specific.
#
# Needs a nightly toolchain and cargo-fuzz:
#   cargo install cargo-fuzz

set -eu

SECONDS_TO_RUN="${SECONDS_TO_RUN:-300}"
JOBS="${JOBS:-1}"

repo=$(git rev-parse --show-toplevel)
corpus="$repo/fuzz/corpus/format_toml"
mkdir -p "$corpus"

# The first byte of an input picks the preset, so each fixture is seeded once
# per preset rather than only under the default style.
seeded=0
for fixture in "$repo"/tests/fixtures/*.toml.in; do
    [ -f "$fixture" ] || continue
    index=0
    while [ "$index" -lt 7 ]; do
        name=$(basename "$fixture" .toml.in)
        {
            printf "$(printf '\\%03o' "$index")"
            cat "$fixture"
        } > "$corpus/$name-$index"
        index=$((index + 1))
        seeded=$((seeded + 1))
    done
done
printf '==> seeded %d inputs into %s\n' "$seeded" "$corpus"

printf '==> fuzzing for %ss\n' "$SECONDS_TO_RUN"
cd "$repo"
exec cargo +nightly fuzz run format_toml --jobs "$JOBS" -- \
    -max_total_time="$SECONDS_TO_RUN" \
    -print_final_stats=1
