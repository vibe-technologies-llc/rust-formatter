#![forbid(unsafe_code)]

use std::{
    io::Read,
    path::{Path, PathBuf},
};

pub mod cache;
pub mod cargo_config;
pub mod cli;
pub mod config;
pub mod detector;
pub mod error;
pub mod git;
pub mod hook;
pub mod output;
pub mod pool;
pub mod registry;
pub mod registry_http;
pub mod report;
pub mod runner;
pub mod rustfmt_config;
pub mod selection;
pub mod semver;
pub mod settings;
mod toml_align;
mod toml_directive;
pub mod toml_fmt;
pub mod toml_lint;
mod toml_scan;
mod toml_sort;
pub mod toml_style;
mod toml_width;
pub mod toolchain;
pub mod toolchain_file;
mod version_sort;
pub mod versions;
pub mod watch;

pub use config::{RustStyle, RustfmtConfig};
pub use detector::{TargetKind, detect_target};
pub use error::{Error, Result};
pub use git::{GitPlan, GitScope, GitSelection};
pub use output::{ColorChoice, Emit, ListMode, MessageFormat, Streams};
pub use registry::{IndexEntry, Registry, RegistryCli, RegistryOptions, RegistrySource};
pub use report::{
    DEFAULT_DIFF_CONTEXT, Report, ReportFile, ReportMode, Verdict, paint_diff, paint_error,
    paint_summary, paint_verbose, summary_line, unified_diff, verbose_preamble,
};
pub use runner::{
    FileOutcome, FileStatus, FormatResult, FormatterOptions, LineRange, Preview,
    RustfmtCapabilities, run_format, run_format_plan, run_format_to,
};
pub use selection::{
    FileSource, Filter, Kind, Languages, Plan, SelectionOptions, Selector,
    resolve as resolve_selection,
};
pub use semver::{PartialVersion, Requirement, Unpinnable};
pub use toml_fmt::{ManifestContext, TomlFormatOutput, format_toml, format_toml_with_versions};
pub use toml_lint::{TomlIssue, toml_1_0_issues};
pub use toml_style::{
    ArrayStyle, IndentSpec, InlineTableStyle, PackageOrder, Spacing, TomlIndent, TomlStyle,
    TomlVersion, TrailingComma,
};
pub use toolchain::{InstallPolicy, Resolved as ResolvedToolchain, Via as ToolchainVia};
pub use toolchain_file::Pin as ToolchainPin;
pub use versions::{
    DepRequest, LookupPolicy, RegistryLookup, Resolution, SkipReason, UpgradePolicy, VersionLookup,
    VersionRecord,
};

pub fn format(options: &FormatterOptions) -> Result<FormatResult> {
    let streams = Streams::discard();
    let selector = options.selector()?;
    let plan = plan(options, &selector)?;
    runner::run_format_plan(plan, options, &selector, &streams)
}

/// Resolve what `options` selects without formatting anything.
pub fn plan(options: &FormatterOptions, selector: &Selector) -> Result<Plan> {
    let mut plan = selection::resolve(&options.targets, &options.source, options.all, selector)?;
    runner::bind_workspaces(&mut plan, options);
    Ok(plan)
}

/// Formats the target, writing the product to `streams` stdout and every
/// diagnostic to its stderr, and returns the process exit code. This is the
/// whole binary: `main` only parses the CLI.
pub fn run(options: &FormatterOptions, streams: &Streams<'_>) -> i32 {
    run_reporting(options, streams).0
}

/// [`run`], and the paths it rewrote.
///
/// `--watch` needs the second half: an event for a file this run just wrote is
/// not a reason to run again, and a formatter that never reached a fixed point
/// would otherwise chase its own writes forever.
pub fn run_reporting(options: &FormatterOptions, streams: &Streams<'_>) -> (i32, Vec<PathBuf>) {
    let mut written = Vec::new();
    let code = match execute(options, streams, &mut written) {
        Ok(code) => code,
        Err(err) => report_fatal(err, options, streams),
    };
    (code, written)
}

/// A run that could not be completed at all, reported and turned into an exit
/// code. `--watch` shares it, because a watcher that fails to start has to say
/// so in whichever format the caller asked for.
pub fn report_fatal(err: Error, options: &FormatterOptions, streams: &Streams<'_>) -> i32 {
    if options.message_format == MessageFormat::Json {
        return report_failure(err, run_mode(options), streams);
    }
    let _ = streams.paint_note(|out| paint_error(&err, out));
    2
}

pub fn report_failure(err: Error, mode: ReportMode, streams: &Streams<'_>) -> i32 {
    let errors = [err];
    let _ = report(
        streams,
        &Report {
            errors: &errors,
            exit_code: 2,
            ..Report::new(mode)
        },
    );
    2
}

fn execute(
    options: &FormatterOptions,
    streams: &Streams<'_>,
    written: &mut Vec<PathBuf>,
) -> Result<i32> {
    let json = options.message_format == MessageFormat::Json;

    if options.print_config {
        let probe = options.targets.first().map(PathBuf::as_path);
        let config = runner::print_config(options, probe)?;
        if json {
            return report(
                streams,
                &Report {
                    config: Some(&config),
                    ..Report::new(ReportMode::PrintConfig)
                },
            );
        }
        emitted(streams.product(&config))?;
        return Ok(0);
    }

    if options.stdin {
        return run_stdin(options, streams);
    }

    let selector = options.selector()?;
    let plan = plan(options, &selector)?;

    if options.list == ListMode::Files {
        let files = runner::collect_files(&plan, options, &selector, streams)?;
        if json {
            return report(
                streams,
                &Report {
                    files: files
                        .iter()
                        .map(|(path, language)| ReportFile {
                            path,
                            language: *language,
                            verdict: None,
                            diff: None,
                        })
                        .collect(),
                    selected: files.len(),
                    ..Report::new(ReportMode::ListFiles)
                },
            );
        }
        for (path, _) in &files {
            emitted(streams.product_line(&path.display().to_string()))?;
        }
        return Ok(0);
    }

    if !options.quiet && !json {
        for warning in &plan.warnings {
            emitted(streams.note_line(warning))?;
        }
    }

    if options.verbose && !options.quiet && !json {
        let preamble = verbose_preamble(&plan.targets, options);
        emitted(streams.paint_note(|out| paint_verbose(&preamble, out)))?;
    }

    let explicit = plan.explicit;
    let git = plan.git.clone();
    let mut warnings = plan.warnings.clone();
    let mut result = runner::run_format_plan(plan, options, &selector, streams)?;
    written.extend(result.changed_paths().map(Path::to_path_buf));

    if options.restage
        && let Some(git) = &git
    {
        match git::restage(git, result.changed_paths()) {
            Ok(notes) => result.warnings.extend(notes),
            Err(err) => result.file_errors.push(err),
        }
    }
    warnings.extend(result.warnings.iter().cloned());

    report_run(&result, &warnings, explicit, options, streams, false)
}

/// Everything a finished run has to say: the JSON envelope, or the warnings,
/// per-file errors, listing, preview, diffs and summary.
///
/// [`execute`] and [`watch::run`] both end here, so a watched run reports
/// exactly as a single one does -- with one difference, which is the argument.
/// `quiet_when_unchanged` is that difference: a single run always states its
/// verdict, because it is the whole output of a command someone just typed,
/// while a watched run that changed nothing has nothing to say. The batch that
/// woke it was, nearly always, the previous run's own write.
pub(crate) fn report_run(
    result: &FormatResult,
    warnings: &[String],
    explicit: bool,
    options: &FormatterOptions,
    streams: &Streams<'_>,
    quiet_when_unchanged: bool,
) -> Result<i32> {
    if options.message_format == MessageFormat::Json {
        let mut envelope = Report::from_result(run_mode(options), result, warnings);
        if let Some(preview) = &result.preview {
            envelope.files = vec![ReportFile {
                path: &preview.path,
                language: preview.language,
                verdict: None,
                diff: None,
            }];
            envelope.content = Some(&preview.content);
        }
        return report(streams, &envelope);
    }

    if !options.quiet {
        for warning in &result.warnings {
            emitted(streams.note_line(warning))?;
        }
    }

    // Nothing was selected, so there is no target to name in a summary. A
    // selection the user narrowed themselves says nothing at all, because a
    // hook run over a commit that touched no Rust must succeed quietly; a plain
    // path that filtered down to nothing says so instead.
    let vacuous = result.files == 0 && (explicit || result.targets.is_empty());
    if vacuous
        && !explicit
        && !options.quiet
        && !quiet_when_unchanged
        && result.file_errors.is_empty()
    {
        emitted(streams.note_line("Nothing to format"))?;
    }

    for file_error in &result.file_errors {
        emitted(streams.paint_note(|out| paint_error(file_error, out)))?;
    }

    if options.list == ListMode::Different {
        for path in result.changed_paths() {
            emitted(streams.product_line(&path.display().to_string()))?;
        }
        return Ok(result.process_exit_code());
    }

    if let Some(preview) = &result.preview {
        emitted(streams.product_bytes(&preview.content))?;
    }

    // `-q` silences commentary, not the product: rustfmt's own `--quiet` still
    // prints its diff, so suppressing the TOML one would split the two apart.
    if options.check {
        for outcome in &result.outcomes {
            if let Some(diff) = &outcome.diff {
                emitted(streams.product(&paint_diff(diff)))?;
            }
        }
    }

    let worth_saying = !quiet_when_unchanged || result.changed() > 0 || result.exit_code != 0;
    if !options.quiet && !vacuous && worth_saying && result.file_errors.is_empty() {
        let line = summary_line(result, options.check);
        let changed = result.changed();
        let previewing = result.preview.is_some();
        emitted(streams.paint_note(|out| {
            paint_summary(
                &line,
                options.check,
                previewing,
                result.exit_code,
                changed,
                out,
            )
        }))?;
    }

    Ok(result.process_exit_code())
}

fn run_mode(options: &FormatterOptions) -> ReportMode {
    if options.print_config {
        return ReportMode::PrintConfig;
    }
    if options.stdin {
        return ReportMode::Stdin;
    }
    match options.list {
        ListMode::Files => ReportMode::ListFiles,
        ListMode::Different => ReportMode::ListDifferent,
        ListMode::None if options.emit == Emit::Stdout => ReportMode::Preview,
        ListMode::None if options.check => ReportMode::Check,
        ListMode::None => ReportMode::Write,
    }
}

fn report(streams: &Streams<'_>, envelope: &Report<'_>) -> Result<i32> {
    emitted(streams.product(&envelope.render()))?;
    emitted(streams.product("\n"))?;
    Ok(envelope.exit_code)
}

fn run_stdin(options: &FormatterOptions, streams: &Streams<'_>) -> Result<i32> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|err| Error::io("<stdin>", err))?;

    let name = options
        .stdin_filepath
        .as_deref()
        .unwrap_or(Path::new("<stdin>"));
    let language = if detector::is_toml_path(name) {
        Kind::Toml
    } else {
        Kind::Rust
    };

    let mut resolved = options.clone();
    // The name is reported as it was typed, but everything resolved *from* it --
    // the owning manifest, the project's `rustfmt.toml` -- needs an absolute
    // path to climb. Helix's `%{buffer_name}` is relative.
    resolved.stdin_filepath = options.stdin_filepath.as_deref().map(detector::absolutize);
    if language == Kind::Rust {
        resolved.edition = runner::stdin_edition(resolved.stdin_filepath.as_deref(), options);
    }

    let mut warnings = Vec::new();

    let mut source = runner::decode_source(&bytes, name)?;
    if language == Kind::Rust {
        source = source.with_newline_style(runner::stdin_newline_style(&resolved));
    }
    let output = runner::format_stdin(&source.text, language, &resolved)?;
    warnings.extend(output.warnings);
    let versions = output.versions;
    let formatted = output.text;
    if language == Kind::Toml && resolved.toml_style.toml_version == TomlVersion::V1_0 {
        warnings.extend(toml_1_0_issues(&formatted).into_iter().map(|issue| {
            format!(
                "warning: {}:{}:{}: {}",
                name.display(),
                issue.line,
                issue.column,
                issue.message
            )
        }));
    }
    let product = runner::encode_source(&formatted, &source);
    let original = source.text;

    // rustfmt exits 0 for a `--check` on stdin even when it printed a diff, so
    // the verdict has to come from comparing the bytes themselves.
    let changed = product != bytes;

    if options.message_format == MessageFormat::Json {
        let diff = (options.check && changed)
            .then(|| unified_diff(name, &original, &formatted, options.diff_context));
        return report(
            streams,
            &Report {
                files: vec![ReportFile {
                    path: name,
                    language,
                    verdict: Some(if options.check && changed {
                        Verdict::NeedsFormatting
                    } else {
                        Verdict::Formatted
                    }),
                    diff: diff.as_deref(),
                }],
                content: (!options.check).then_some(product.as_slice()),
                warnings: &warnings,
                versions: &versions,
                selected: 1,
                changed: usize::from(changed),
                exit_code: i32::from(options.check && changed),
                ..Report::new(ReportMode::Stdin)
            },
        );
    }

    if !options.quiet {
        for warning in &warnings {
            emitted(streams.note_line(warning))?;
        }
    }

    if options.check {
        return Ok(i32::from(changed));
    }

    emitted(streams.product_bytes(&product))?;
    Ok(0)
}

/// `Streams` already swallows a closed pipe; anything left is a real failure to
/// deliver output, which is a tool error rather than a formatting verdict.
fn emitted(result: std::io::Result<()>) -> Result<()> {
    result.map_err(|err| Error::io("<output>", err))
}
