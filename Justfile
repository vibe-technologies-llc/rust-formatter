# Local verification recipes for rust-formatter. `just all` is what has to pass
# before a change goes in.
#
# `all` needs `just` and cargo-nextest. `coverage` needs cargo-llvm-cov and
# `fuzz` needs cargo-fuzz and a nightly toolchain; both say so if they are
# missing.

corpus_tmp := env("XDG_CACHE_HOME", env("HOME") / ".cache") / "rust-formatter-corpus"

default:
    @just --list

# Everything that has to pass before a commit.
all: test clippy build check-self

# The default suite: unit, integration, property, golden and doc tests.
test:
    cargo nextest run --locked
    cargo test --locked --doc

# The two registry sweeps, which read ~/.cargo/registry and so are #[ignore]d.
test-ignored:
    cargo nextest run --locked --release --run-ignored only --no-capture

# A deeper proptest budget than the 256 cases `test` runs.
test-deep cases="20000":
    RUST_FORMATTER_PROPTEST_CASES={{cases}} cargo nextest run --locked --release --test property_tests

# Regenerate every golden under tests/expected/. Read the diff before committing
# it -- that diff is the whole point of the files.
update-expect:
    UPDATE_EXPECT=1 cargo nextest run --locked

# Prove the Rust suites skip rather than fail without a toolchain.
test-no-toolchain:
    cargo xtask no-toolchain

clippy:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

build:
    cargo build --release --locked

# The formatter over its own tree. Exits 1 if anything needs formatting.
check-self:
    cargo run --locked -- --check .

# Format this repository with itself.
fmt:
    cargo run --locked -- .

# Differential sweep against a git ref over the local crate registry, both
# languages, every preset.
corpus *args:
    cargo xtask corpus {{args}}

# The TOML leg alone, which needs no toolchain and is much faster.
corpus-toml *args:
    cargo xtask corpus --no-rust {{args}}

fuzz seconds="300":
    cargo xtask fuzz --seconds {{seconds}}

coverage:
    cargo llvm-cov nextest --all-features --locked --html
    @echo "report: target/llvm-cov/html/index.html"

coverage-summary:
    cargo llvm-cov nextest --all-features --locked --summary-only

bench:
    cargo bench --locked

clean:
    cargo clean
    rm -rf {{corpus_tmp}} corpus-failures fuzz/target fuzz/artifacts
