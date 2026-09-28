#!/bin/sh
# Prove the Rust-path tests skip rather than fail when no rustfmt can be
# resolved.
#
# Every suite that drives Rust formatting has to ask `needs_nightly!()` or
# `needs_stable!()` first. One that forgets does not fail on a developer's
# machine -- it fails on a machine without the toolchain, which is exactly where
# nobody is looking. This runs each test binary with a PATH whose rustup and
# rustfmt both fail, so a missing gate shows up here instead.
#
# Exit: 0 every suite skipped, 1 a suite failed.

set -eu

repo=$(git rev-parse --show-toplevel)
cd "$repo"

cargo test --locked --no-run >/dev/null

shim=$(mktemp -d)
cleanup() { rm -rf "$shim"; }
trap cleanup EXIT
for tool in rustup rustfmt; do
    printf '#!/bin/sh\nexit 127\n' > "$shim/$tool"
    chmod +x "$shim/$tool"
done

# Only the test binaries. The same output also names the bin target, which takes
# no libtest arguments and would report a usage error rather than a missing gate;
# cargo puts test binaries under `deps/` and the bin one level above it.
cargo test --locked --no-run --message-format=json 2>/dev/null \
    | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' \
    | grep '/deps/' \
    | LC_ALL=C sort -u > "$shim/binaries"

count=0
fail=0
while IFS= read -r bin; do
    [ -x "$bin" ] || continue
    count=$((count + 1))
    name=$(basename "$bin")
    if env -u RUSTFMT PATH="$shim:/usr/bin:/bin" "$bin" --test-threads 8 >"$shim/out" 2>&1; then
        printf '    %s skipped cleanly\n' "$name"
    else
        printf 'FAIL: %s does not skip without a toolchain\n' "$name" >&2
        sed -n '/^failures:$/,$p' "$shim/out" | head -n 20 >&2
        fail=1
    fi
done < "$shim/binaries"

if [ "$count" -eq 0 ]; then
    printf 'no test binaries found\n' >&2
    exit 1
fi
if [ "$fail" -ne 0 ]; then
    printf '\nGATE INCOMPLETE (%d binaries)\n' "$count" >&2
    exit 1
fi
printf '\nevery one of %d test binaries skips without a toolchain\n' "$count"
