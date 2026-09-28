# Local verification recipes for rust-formatter. `just all` is what has to pass
# before a change goes in.
#
# `just` is the only extra tool `all` needs. `coverage` needs cargo-llvm-cov and
# `fuzz` needs cargo-fuzz and a nightly toolchain; both say so if they are
# missing.

# Outside the repository, and on a real disk. Both matter: a work tree inside a
# cargo project is detected as that project, so the sweep would format nothing
# and pass having proved nothing, and /tmp is a tmpfs on some machines with no
# room for a release build of the reference binary. `scripts/corpus.sh` refuses
# to run if the first is wrong.
corpus_tmp := env("XDG_CACHE_HOME", env("HOME") / ".cache") / "rust-formatter-corpus"

default:
    @just --list

# Everything that has to pass before a commit.
all: test clippy build check-self

# The default suite: unit, integration, property, golden and doc tests.
test:
    cargo test --locked

# The two registry sweeps, which read ~/.cargo/registry and so are #[ignore]d.
test-ignored:
    cargo test --locked --release -- --ignored --nocapture

# A deeper proptest budget than the 256 cases `test` runs.
test-deep cases="20000":
    RUST_FORMATTER_PROPTEST_CASES={{cases}} cargo test --locked --release --test property_tests

# Regenerate every golden under tests/expected/. Read the diff before committing
# it -- that diff is the whole point of the files.
update-expect:
    UPDATE_EXPECT=1 cargo test --locked

# Prove the Rust suites skip rather than fail without a toolchain.
test-no-toolchain:
    sh scripts/no-toolchain.sh

clippy:
    cargo clippy --all-targets --all-features --locked -- -D warnings

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
    mkdir -p {{corpus_tmp}}
    TMPDIR={{corpus_tmp}} sh scripts/corpus.sh {{args}}

# The TOML leg alone, which needs no toolchain and is much faster.
corpus-toml *args:
    mkdir -p {{corpus_tmp}}
    TMPDIR={{corpus_tmp}} RS=0 sh scripts/corpus.sh {{args}}

fuzz seconds="300":
    SECONDS_TO_RUN={{seconds}} sh scripts/fuzz.sh

coverage:
    cargo llvm-cov --all-features --locked --html
    @echo "report: target/llvm-cov/html/index.html"

coverage-summary:
    cargo llvm-cov --all-features --locked --summary-only

bench:
    cargo bench --locked

clean:
    cargo clean
    rm -rf {{corpus_tmp}} corpus-failures fuzz/target fuzz/artifacts
