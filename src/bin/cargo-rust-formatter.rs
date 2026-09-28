//! `cargo rust-formatter`. Cargo finds this by name on `PATH` and passes
//! `rust-formatter` as the first argument, which `cargo_main` drops.

#![forbid(unsafe_code)]

fn main() {
    rust_formatter::cli::cargo_main();
}
