use std::{fs, num::NonZeroUsize, process::ExitCode};

use clap::Args;

use crate::{
    cargo,
    corpus::TOML_PRESETS,
    error::{Error, Result},
    tree,
};

const TARGET: &str = "format_toml";
const FIXTURES: &str = "tests/fixtures";
const FIXTURE_SUFFIX: &str = ".toml.in";

#[derive(Args)]
pub struct Options {
    #[arg(
        long,
        default_value_t = 300,
        help = "How long libFuzzer runs, in seconds"
    )]
    seconds: u64,
    #[arg(long, default_value_t = NonZeroUsize::MIN, help = "How many fuzzing jobs run at once")]
    jobs: NonZeroUsize,
}

pub fn run(options: &Options) -> Result<ExitCode> {
    let root = cargo::repo_root();
    let corpus = root.join("fuzz/corpus").join(TARGET);
    fs::create_dir_all(&corpus).map_err(Error::io(&corpus))?;

    let mut seeded = 0;
    for fixture in tree::files_with_suffix(&root.join(FIXTURES), FIXTURE_SUFFIX, 1)? {
        let name = fixture.file_name().map_or_default(|name| {
            name.to_string_lossy()
                .trim_end_matches(FIXTURE_SUFFIX)
                .to_owned()
        });
        let body = fs::read(&fixture).map_err(Error::io(&fixture))?;
        for (selector, _) in (0u8..).zip(TOML_PRESETS) {
            let seed = corpus.join(format!("{name}-{selector}"));
            let input: Vec<u8> = std::iter::once(selector)
                .chain(body.iter().copied())
                .collect();
            fs::write(&seed, input).map_err(Error::io(&seed))?;
            seeded += 1;
        }
    }
    println!("==> seeded {seeded} inputs into {}", corpus.display());

    println!("==> fuzzing for {}s", options.seconds);
    let status = cargo::status(
        cargo::cargo()
            .current_dir(root)
            .args(["+nightly", "fuzz", "run", TARGET, "--jobs"])
            .arg(options.jobs.to_string())
            .arg("--")
            .arg(format!("-max_total_time={}", options.seconds))
            .arg("-print_final_stats=1"),
    )?;
    Ok(cargo::exit_code(status))
}
