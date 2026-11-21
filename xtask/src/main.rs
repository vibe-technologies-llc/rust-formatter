mod cargo;
mod corpus;
mod error;
mod fuzz;
mod no_toolchain;
mod tree;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    bin_name = "cargo xtask",
    about = "Development tasks for rust-formatter"
)]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    #[command(
        about = "Differential and idempotence sweep over the local crate registry",
        long_about = "Builds a reference binary from a git ref and the working-tree binary, runs \
                      both over identical copies of a sample of the registry, and diffs the \
                      results. Then runs the working-tree binary twice over a third copy to \
                      prove idempotence, and with --list-different to prove the converged tree \
                      is clean. A style whose flags the reference rejects drops to idempotence \
                      alone.\n\nExit: 0 pass, 1 differences found, 2 the sweep could not run, 77 \
                      corpus unavailable."
    )]
    Corpus(corpus::Options),
    #[command(about = "Seed the corpus from tests/fixtures and fuzz format_toml")]
    Fuzz(fuzz::Options),
    #[command(about = "Prove every test binary skips rather than fails without a rustfmt")]
    NoToolchain,
}

fn main() -> ExitCode {
    let result = match Cli::parse().task {
        Task::Corpus(options) => corpus::run(&options),
        Task::Fuzz(options) => fuzz::run(&options),
        Task::NoToolchain => no_toolchain::run(),
    };

    result.unwrap_or_else(|err| {
        eprintln!("xtask: {err}");
        ExitCode::from(err.exit_code())
    })
}
