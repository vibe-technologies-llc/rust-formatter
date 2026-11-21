use std::{
    env,
    fmt::Display,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

use clap::Args;
use similar::TextDiff;

use crate::{
    cargo,
    error::{Error, Result},
    tree,
};

pub const TOML_PRESETS: [&str; 7] = [
    "default",
    "compact",
    "expand",
    "everything",
    "cargo",
    "narrow-tabs",
    "aligned-indented",
];
const RUST_PRESETS: [&str; 4] = ["default", "comments", "literals", "strict"];

const COPIES: [&str; 3] = ["a", "b", "c"];
const FIRST_PASS: &str = "c.pass1";
const CHECKSUM: &str = ".cargo-checksum.json";
const REGISTRY_SOURCES: &str = "registry/src";
const WORK_DIR_NAME: &str = "rust-formatter-corpus";
const WORK_PREFIX: &str = "rf-corpus.";
const CRATE_DEPTH: usize = 2;
const PROBE_NAME: &str = "000001.toml";
const PROBE_SOURCE: &str = "rf_probe={path=\"x\"}\n";
const PROBE_FORMATTED: &str = "rf_probe.path = \"x\"";
const PATH_PLACEHOLDER: &str = "CORPUS";
const SHOWN_DIFFERENCES: usize = 40;
const SHOWN_LISTED: usize = 20;

#[derive(Args)]
pub struct Options {
    #[arg(long, default_value_t = 3000, help = "How many .toml files to sample")]
    toml_files: usize,
    #[arg(
        long,
        default_value_t = 25,
        help = "How many crate directories the Rust leg samples"
    )]
    crates: usize,
    #[arg(long, help = "Skip the Rust leg, which needs a rustfmt")]
    no_rust: bool,
    #[arg(
        long = "ref",
        value_name = "REF",
        default_value = "HEAD",
        help = "The git ref the reference binary is built from"
    )]
    reference: String,
    #[arg(long, help = "Keep the work tree even when the sweep passes")]
    keep: bool,
    #[arg(
        long,
        value_delimiter = ',',
        default_values = TOML_PRESETS,
        help = "The presets the TOML leg sweeps"
    )]
    presets: Vec<String>,
    #[arg(
        long,
        value_delimiter = ',',
        default_values = RUST_PRESETS,
        help = "The presets the Rust leg sweeps"
    )]
    rust_presets: Vec<String>,
    #[arg(
        long,
        allow_hyphen_values = true,
        conflicts_with = "presets",
        help = "Raw formatter flags the TOML leg sweeps instead of the presets"
    )]
    style: Option<String>,
    #[arg(
        long,
        env = "RUST_FORMATTER_CORPUS_DIR",
        help = "The tree to sample [default: the cargo registry sources]"
    )]
    corpus: Option<PathBuf>,
    #[arg(
        long,
        help = "Where the work tree is created [default: ~/.cache/rust-formatter-corpus]"
    )]
    work_dir: Option<PathBuf>,
}

impl Options {
    fn corpus(&self) -> Option<PathBuf> {
        self.corpus
            .clone()
            .or_else(|| cargo_home().map(|home| home.join(REGISTRY_SOURCES)))
    }

    fn work_base(&self) -> Option<PathBuf> {
        self.work_dir
            .clone()
            .or_else(|| cache_home().map(|cache| cache.join(WORK_DIR_NAME)))
    }

    fn toml_styles(&self) -> Vec<Style> {
        match &self.style {
            Some(flags) => vec![Style {
                label: "toml/style".to_owned(),
                flags: flags.split_whitespace().map(str::to_owned).collect(),
            }],
            None => Style::presets("toml", &self.presets),
        }
    }
}

struct Style {
    label: String,
    flags: Vec<String>,
}

impl Style {
    fn presets(leg: &str, presets: &[String]) -> Vec<Self> {
        presets
            .iter()
            .map(|preset| Self {
                label: format!("{leg}/{preset}"),
                flags: vec!["--preset".to_owned(), preset.clone()],
            })
            .collect()
    }
}

enum Verdict {
    Passed,
    Failed,
}

struct Run {
    code: Option<i32>,
    stderr: String,
}

struct Sweep {
    work: PathBuf,
    reference: String,
    reference_binary: PathBuf,
    binary: PathBuf,
    failed: bool,
}

pub fn run(options: &Options) -> Result<ExitCode> {
    let corpus = options.corpus().ok_or(Error::HomeUnknown)?;
    if !corpus.is_dir() {
        return Err(Error::CorpusMissing { path: corpus });
    }

    let base = options.work_base().ok_or(Error::HomeUnknown)?;
    fs::create_dir_all(&base).map_err(Error::io(&base))?;
    let work = tempfile::Builder::new()
        .prefix(WORK_PREFIX)
        .tempdir_in(&base)
        .map_err(Error::io(&base))?
        .keep();
    let worktree = work.join("ref");

    let result = sweep(options, &corpus, &work, &worktree);
    remove_worktree(&worktree);

    if options.keep || !matches!(result, Ok(Verdict::Passed)) {
        eprintln!("\nartifacts kept in {}", work.display());
    } else {
        fs::remove_dir_all(&work).map_err(Error::io(&work))?;
    }
    result.map(|verdict| match verdict {
        Verdict::Passed => ExitCode::SUCCESS,
        Verdict::Failed => ExitCode::FAILURE,
    })
}

fn sweep(options: &Options, corpus: &Path, work: &Path, worktree: &Path) -> Result<Verdict> {
    say(format!(
        "building reference binary from {}",
        options.reference
    ));
    cargo::succeed(
        Command::new("git")
            .arg("-C")
            .arg(cargo::repo_root())
            .args(["worktree", "add", "--detach"])
            .arg(worktree)
            .arg(&options.reference)
            .stdout(Stdio::null()),
    )?;
    let reference_binary =
        cargo::release_binary(&worktree.join("Cargo.toml"), Some(&work.join("ref-target")))?;

    say("building working-tree binary");
    let binary = cargo::release_binary(&cargo::repo_root().join("Cargo.toml"), None)?;

    let mut sweep = Sweep {
        work: work.to_owned(),
        reference: options.reference.clone(),
        reference_binary,
        binary,
        failed: false,
    };
    let sampled = sweep.toml_leg(options, corpus)?;
    sweep.rust_leg(options, corpus)?;

    if sweep.failed {
        eprintln!("\nCORPUS SWEEP FAILED");
        Ok(Verdict::Failed)
    } else {
        println!(
            "\nCORPUS SWEEP PASSED ({sampled} toml files, ref={})",
            sweep.reference
        );
        Ok(Verdict::Passed)
    }
}

impl Sweep {
    fn toml_leg(&mut self, options: &Options, corpus: &Path) -> Result<usize> {
        say(format!(
            "sampling up to {} .toml files from {}",
            options.toml_files,
            corpus.display()
        ));
        let all = tree::files_with_suffix(corpus, ".toml", usize::MAX)?;
        let sample = tree::stride(&all, options.toml_files);

        let toml = self.work.join("toml");
        let pristine = toml.join("pristine");
        fs::create_dir_all(&pristine).map_err(Error::io(&pristine))?;
        for (index, file) in sample.iter().enumerate() {
            tree::copy_file(file, &pristine.join(format!("{:06}.toml", index + 1)))?;
        }
        note(format!("{} of {} files copied", sample.len(), all.len()));
        if sample.is_empty() {
            return Err(Error::NothingSampled {
                corpus: corpus.to_owned(),
            });
        }

        self.probe()?;

        for style in options.toml_styles() {
            say(&style.label);
            refresh_copies(&pristine, &toml)?;
            self.one_style(&toml, &style)?;
        }
        Ok(sample.len())
    }

    fn rust_leg(&mut self, options: &Options, corpus: &Path) -> Result<()> {
        if options.no_rust {
            say("Rust leg skipped (--no-rust)");
            return Ok(());
        }
        if !on_path("rustfmt") && !on_path("rustup") {
            say("Rust leg skipped (no rustfmt and no rustup)");
            return Ok(());
        }

        say(format!(
            "sampling up to {} crate directories from {}",
            options.crates,
            corpus.display()
        ));
        let all = tree::directories_at_depth(corpus, CRATE_DEPTH)?;
        let crates: Vec<PathBuf> = tree::stride(&all, options.crates)
            .into_iter()
            .filter(|dir| dir.join("Cargo.toml").is_file())
            .collect();

        let rs = self.work.join("rs");
        let pristine = rs.join("pristine");
        for (index, dir) in crates.iter().enumerate() {
            tree::copy_tree(
                dir,
                &pristine.join(format!("{:03}", index + 1)),
                &[CHECKSUM],
            )?;
        }
        note(format!(
            "{} of {} crate directories copied",
            crates.len(),
            all.len()
        ));
        if crates.is_empty() {
            say("Rust leg skipped (no crate directories sampled)");
            return Ok(());
        }

        for style in Style::presets("rust", &options.rust_presets) {
            say(format!("{} (loose walk)", style.label));
            refresh_copies(&pristine, &rs)?;
            self.one_style(&rs, &style)?;
        }
        self.project_leg(&pristine)
    }

    fn project_leg(&mut self, pristine: &Path) -> Result<()> {
        say("rust/default (per-crate cargo projects)");
        let rsp = self.work.join("rsp");
        refresh_copies(pristine, &rsp)?;
        let [a, b, c] = COPIES.map(|copy| rsp.join(copy));

        format_each(&self.reference_binary, &a)?;
        format_each(&self.binary, &b)?;
        let differences = tree::differences(&a, &b)?;
        if differences.is_empty() {
            note(format!(
                "rust/project: output identical to {}",
                self.reference
            ));
        } else {
            self.bad(format!(
                "rust/project: {} and the working tree produce different output",
                self.reference
            ));
            show(&differences, SHOWN_DIFFERENCES);
        }

        let first_pass = rsp.join(FIRST_PASS);
        format_each(&self.binary, &c)?;
        tree::replace_with_copy(&c, &first_pass)?;
        format_each(&self.binary, &c)?;
        let unstable = tree::differences(&first_pass, &c)?;
        if unstable.is_empty() {
            note("rust/project: stable after the second pass");
        } else {
            self.bad("rust/project: formatting is not idempotent");
            show(&unstable, SHOWN_DIFFERENCES);
        }
        Ok(())
    }

    fn probe(&self) -> Result<()> {
        say("checking the work tree is formattable");
        let probe = self.work.join("probe");
        fs::create_dir_all(&probe).map_err(Error::io(&probe))?;
        let file = probe.join(PROBE_NAME);
        fs::write(&file, PROBE_SOURCE).map_err(Error::io(&file))?;

        format_quietly(&self.binary, &[], &probe)?;

        let formatted = fs::read_to_string(&file).map_err(Error::io(&file))?;
        if !formatted.contains(PROBE_FORMATTED) {
            return Err(Error::NotLoose {
                work: self.work.clone(),
            });
        }
        note("a loose directory, as the sweep needs");
        Ok(())
    }

    fn one_style(&mut self, root: &Path, style: &Style) -> Result<()> {
        let label = &style.label;
        let [a, b, c] = COPIES.map(|copy| root.join(copy));

        if self.reference_accepts(&style.flags)? {
            let reference = format(&self.reference_binary, &style.flags, &a)?;
            let current = format(&self.binary, &style.flags, &b)?;

            if reference.code != current.code {
                self.bad(format!(
                    "{label}: exit codes differ (ref={} new={})",
                    describe_code(reference.code),
                    describe_code(current.code)
                ));
            }

            let differences = tree::differences(&a, &b)?;
            if differences.is_empty() {
                note(format!("{label}: output identical to {}", self.reference));
            } else {
                self.bad(format!(
                    "{label}: {} and the working tree produce different output",
                    self.reference
                ));
                show(&differences, SHOWN_DIFFERENCES);
            }

            if reference.stderr == current.stderr {
                note(format!("{label}: stderr identical to {}", self.reference));
            } else {
                self.bad(format!("{label}: stderr differs from {}", self.reference));
                let diff = TextDiff::from_lines(&reference.stderr, &current.stderr)
                    .unified_diff()
                    .header("ref", "new")
                    .to_string();
                show(&diff.lines().collect::<Vec<_>>(), SHOWN_DIFFERENCES);
            }
        } else {
            note(format!(
                "{label}: {} does not accept these flags, differential leg skipped",
                self.reference
            ));
        }

        self.converges(root, &c, style)
    }

    fn converges(&mut self, root: &Path, tree: &Path, style: &Style) -> Result<()> {
        let label = &style.label;
        let first_pass = root.join(FIRST_PASS);

        format_quietly(&self.binary, &style.flags, tree)?;
        tree::replace_with_copy(tree, &first_pass)?;
        format_quietly(&self.binary, &style.flags, tree)?;

        let unstable = tree::differences(&first_pass, tree)?;
        if unstable.is_empty() {
            note(format!("{label}: stable after the second pass"));
        } else {
            self.bad(format!("{label}: formatting is not idempotent"));
            show(&unstable, SHOWN_DIFFERENCES);
        }

        let listed = cargo::output(
            Command::new(&self.binary)
                .args(&style.flags)
                .arg("--list-different")
                .arg(tree)
                .stderr(Stdio::null()),
        )?;
        let listed = String::from_utf8_lossy(&listed.stdout);
        if listed.trim().is_empty() {
            note(format!("{label}: --check is clean"));
        } else {
            self.bad(format!(
                "{label}: --check still flags files after two write passes"
            ));
            show(&listed.lines().collect::<Vec<_>>(), SHOWN_LISTED);
        }
        Ok(())
    }

    fn reference_accepts(&self, flags: &[String]) -> Result<bool> {
        let status = cargo::status(
            Command::new(&self.reference_binary)
                .args(flags)
                .arg("--print-settings")
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )?;
        Ok(status.success())
    }

    fn bad(&mut self, message: impl Display) {
        eprintln!("FAIL: {message}");
        self.failed = true;
    }
}

fn refresh_copies(pristine: &Path, root: &Path) -> Result<()> {
    for copy in COPIES {
        tree::replace_with_copy(pristine, &root.join(copy))?;
    }
    Ok(())
}

fn format(binary: &Path, flags: &[String], tree: &Path) -> Result<Run> {
    let output = cargo::output(
        Command::new(binary)
            .args(flags)
            .arg(tree)
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    )?;
    let stderr =
        String::from_utf8_lossy(&output.stderr).replace(&*tree.to_string_lossy(), PATH_PLACEHOLDER);
    let mut lines: Vec<&str> = stderr.lines().collect();
    lines.sort_unstable();
    Ok(Run {
        code: output.status.code(),
        stderr: lines.join("\n"),
    })
}

fn format_quietly(binary: &Path, flags: &[String], tree: &Path) -> Result<()> {
    cargo::status(
        Command::new(binary)
            .args(flags)
            .arg(tree)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )?;
    Ok(())
}

fn format_each(binary: &Path, root: &Path) -> Result<()> {
    for crate_dir in tree::directories_at_depth(root, 1)? {
        format_quietly(binary, &[], &crate_dir)?;
    }
    Ok(())
}

fn remove_worktree(worktree: &Path) {
    let removal = Command::new("git")
        .arg("-C")
        .arg(cargo::repo_root())
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Err(err) = removal {
        eprintln!(
            "could not remove the worktree at {}: {err}",
            worktree.display()
        );
    }
}

fn on_path(program: &str) -> bool {
    let name = format!("{program}{}", env::consts::EXE_SUFFIX);
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(&name).is_file()))
}

fn cargo_home() -> Option<PathBuf> {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(".cargo")))
}

fn cache_home() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(".cache")))
}

fn describe_code(code: Option<i32>) -> String {
    code.map_or_else(|| "signal".to_owned(), |code| code.to_string())
}

fn say(message: impl Display) {
    println!("==> {message}");
}

fn note(message: impl Display) {
    println!("    {message}");
}

fn show(lines: &[impl Display], limit: usize) {
    for line in lines.iter().take(limit) {
        eprintln!("{line}");
    }
}
