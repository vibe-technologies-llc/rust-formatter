use std::{
    fmt::Write as _,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anstyle::{AnsiColor, Color, Style};
use serde::Serialize;
use similar::TextDiff;

use crate::{
    detector::TargetKind,
    error::Error,
    runner::{FileStatus, FormatResult, FormatterOptions},
    selection::Kind,
    versions::{Resolution, VersionRecord},
};

const SUCCESS: Style = Style::new()
    .bold()
    .fg_color(Some(Color::Ansi(AnsiColor::Green)));
const FAILURE: Style = Style::new()
    .bold()
    .fg_color(Some(Color::Ansi(AnsiColor::Red)));
const KEY: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan)));
const ADDED: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
const REMOVED: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)));
const HUNK: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan)));

const VERBOSE_FILE_LIMIT: usize = 20;

pub fn verbose_preamble(targets: &[TargetKind], options: &FormatterOptions) -> String {
    let mut out = String::with_capacity(128);

    for target in targets {
        push_target_details(&mut out, target);
    }

    let _ = writeln!(out, "toolchain: {}", options.toolchain);
    let _ = writeln!(out, "check: {}", options.check);
    out
}

fn push_target_details(out: &mut String, target: &TargetKind) {
    match target {
        TargetKind::CargoProject { manifest_path, .. } => {
            out.push_str("target: cargo project\n");
            let _ = writeln!(out, "manifest: {}", manifest_path.display());
        }
        TargetKind::SingleFile(path) => {
            out.push_str("target: file\n");
            let _ = writeln!(out, "path: {}", path.display());
        }
        TargetKind::FileList {
            rust_files,
            toml_files,
        } => {
            out.push_str("target: file list\n");
            let _ = writeln!(out, "files: {}", rust_files.len());
            if !toml_files.is_empty() {
                let _ = writeln!(out, "toml files: {}", toml_files.len());
            }
            push_file_rows(out, rust_files);
        }
        TargetKind::LooseDirectory {
            root_dir,
            files,
            toml_files,
        } => {
            out.push_str("target: loose directory\n");
            let _ = writeln!(out, "root: {}", root_dir.display());
            let _ = writeln!(out, "files: {}", files.len());
            if !toml_files.is_empty() {
                let _ = writeln!(out, "toml files: {}", toml_files.len());
            }
            push_file_rows(out, files);
        }
    }
}

fn push_file_rows(out: &mut String, files: &[std::path::PathBuf]) {
    for file in files.iter().take(VERBOSE_FILE_LIMIT) {
        let _ = writeln!(out, "  {}", file.display());
    }
    if let Some(hidden) = files
        .len()
        .checked_sub(VERBOSE_FILE_LIMIT)
        .filter(|n| *n > 0)
    {
        let _ = writeln!(out, "  ... and {hidden} more");
    }
}

fn status_prefix(check: bool, previewing: bool, exit_code: i32, changed: usize) -> &'static str {
    match (check, exit_code) {
        (false, 0) if previewing => "Previewed:",
        (false, 0) if changed == 0 => "Already formatted:",
        (false, 0) => "Formatted",
        (false, _) => "Format failed:",
        (true, 0) => "Already formatted:",
        (true, _) => "Check failed:",
    }
}

pub fn summary_line(result: &FormatResult, check: bool) -> String {
    let mut out = String::with_capacity(64);
    let changed = result.changed();

    out.push_str(status_prefix(
        check,
        result.preview.is_some(),
        result.exit_code,
        changed,
    ));
    out.push(' ');

    push_result_label(&mut out, result, check);

    if check && result.exit_code != 0 {
        out.push(' ');
        out.push_str(needs_formatting_suffix(result));
    }

    out.push('\n');
    out
}

pub fn status_style(exit_code: i32) -> Style {
    if exit_code != 0 { FAILURE } else { SUCCESS }
}

pub fn paint_summary(
    line: &str,
    check: bool,
    previewing: bool,
    exit_code: i32,
    changed: usize,
    out: &mut impl Write,
) -> io::Result<()> {
    let prefix = status_prefix(check, previewing, exit_code, changed);
    let style = status_style(exit_code);

    match line.strip_prefix(prefix) {
        Some(rest) => write!(out, "{style}{prefix}{style:#}{rest}"),
        None => out.write_all(line.as_bytes()),
    }
}

pub fn paint_verbose(text: &str, out: &mut impl Write) -> io::Result<()> {
    for line in text.split_inclusive('\n') {
        match line.split_once(':') {
            Some((key, rest)) if !key.starts_with([' ', '.']) => {
                write!(out, "{KEY}{key}:{KEY:#}{rest}")?;
            }
            _ => out.write_all(line.as_bytes())?,
        }
    }
    Ok(())
}

/// What a run was asked to do, which is what tells a consumer how to read the
/// rest of the envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportMode {
    Write,
    Check,
    ListFiles,
    ListDifferent,
    PrintConfig,
    Stdin,
    Preview,
}

impl ReportMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Check => "check",
            Self::ListFiles => "list-files",
            Self::ListDifferent => "list-different",
            Self::PrintConfig => "print-config",
            Self::Stdin => "stdin",
            Self::Preview => "preview",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Formatted,
    NeedsFormatting,
    Errored,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Formatted => "formatted",
            Self::NeedsFormatting => "needs-formatting",
            Self::Errored => "error",
        }
    }
}

impl From<FileStatus> for Verdict {
    fn from(status: FileStatus) -> Self {
        match status {
            FileStatus::Formatted => Self::Formatted,
            FileStatus::NeedsFormatting => Self::NeedsFormatting,
        }
    }
}

pub struct ReportFile<'a> {
    pub path: &'a Path,
    pub language: Kind,
    /// `None` for a listing, which names files without judging them.
    pub verdict: Option<Verdict>,
    pub diff: Option<&'a str>,
}

/// One envelope for every mode. The alternative was a JSON branch per mode,
/// which is how four of them came to ignore `--message-format json` entirely.
pub struct Report<'a> {
    pub mode: ReportMode,
    pub files: Vec<ReportFile<'a>>,
    /// rustfmt's `--print-config` dump, still in TOML.
    pub config: Option<&'a str>,
    /// Formatted source, for the modes whose product is a buffer.
    pub content: Option<&'a [u8]>,
    pub errors: &'a [Error],
    pub warnings: &'a [String],
    /// What `--full-versions` did to each dependency it considered, so a machine
    /// consumer can tell a version finding from a style one.
    pub versions: &'a [(PathBuf, VersionRecord)],
    pub selected: usize,
    pub changed: usize,
    pub exit_code: i32,
}

const ENVELOPE_VERSION: u32 = 3;

pub const DEFAULT_DIFF_CONTEXT: usize = 3;

impl<'a> Report<'a> {
    pub fn new(mode: ReportMode) -> Self {
        Self {
            mode,
            files: Vec::new(),
            config: None,
            content: None,
            errors: &[],
            warnings: &[],
            versions: &[],
            selected: 0,
            changed: 0,
            exit_code: 0,
        }
    }

    /// A run's verdicts, plus one `error` row per file-scoped failure so that
    /// `files` is the complete per-file account rather than the successful part
    /// of one.
    pub fn from_result(mode: ReportMode, result: &'a FormatResult, warnings: &'a [String]) -> Self {
        let mut files: Vec<ReportFile<'a>> = result
            .outcomes
            .iter()
            .map(|outcome| ReportFile {
                path: &outcome.path,
                language: outcome.language,
                verdict: Some(outcome.status.into()),
                diff: outcome.diff.as_deref(),
            })
            .collect();

        files.extend(result.file_errors.iter().filter_map(|error| {
            let path = error.diagnostic().path?;
            Some(ReportFile {
                path,
                language: language_of(path),
                verdict: Some(Verdict::Errored),
                diff: None,
            })
        }));

        Self {
            files,
            errors: &result.file_errors,
            warnings,
            versions: &result.versions,
            selected: result.files,
            changed: result.changed(),
            exit_code: result.process_exit_code(),
            ..Self::new(mode)
        }
    }

    pub fn render(&self) -> String {
        let files: Vec<JsonFile<'_>> = self
            .files
            .iter()
            .map(|file| JsonFile {
                path: file.path.display().to_string(),
                language: match file.language {
                    Kind::Rust => "rust",
                    Kind::Toml => "toml",
                },
                status: file.verdict.map(Verdict::as_str),
                diff: file.diff,
            })
            .collect();

        let report = JsonReport {
            version: ENVELOPE_VERSION,
            mode: self.mode.as_str(),
            config: self.config.map(config_value),
            content: self
                .content
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
            files,
            errors: self.errors.iter().map(json_diagnostic).collect(),
            warnings: self.warnings,
            versions: self.versions.iter().map(json_version).collect(),
            summary: JsonSummary {
                selected: self.selected,
                changed: self.changed,
                errors: self.errors.len(),
            },
            exit_code: self.exit_code,
        };

        serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
    }
}

fn language_of(path: &Path) -> Kind {
    if crate::detector::is_toml_path(path) {
        Kind::Toml
    } else {
        Kind::Rust
    }
}

#[derive(Serialize)]
struct JsonFile<'a> {
    path: String,
    language: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<&'a str>,
}

#[derive(Serialize)]
struct JsonDiagnostic {
    code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<usize>,
    message: String,
}

fn json_diagnostic(error: &Error) -> JsonDiagnostic {
    let diagnostic = error.diagnostic();
    JsonDiagnostic {
        code: diagnostic.code,
        path: diagnostic.path.map(|path| path.display().to_string()),
        line: diagnostic.line,
        column: diagnostic.column,
        message: diagnostic.message,
    }
}

#[derive(Serialize)]
struct JsonVersion {
    path: String,
    #[serde(rename = "crate")]
    crate_name: String,
    section: String,
    requirement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<usize>,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

fn json_version((path, record): &(PathBuf, VersionRecord)) -> JsonVersion {
    let (outcome, completed, reason) = match &record.outcome {
        Resolution::Pinned(to) => ("completed", Some(to.clone()), None),
        Resolution::Unchanged => ("unchanged", None, None),
        Resolution::Skipped(reason) => ("skipped", None, Some(reason.to_string())),
    };
    JsonVersion {
        path: path.display().to_string(),
        crate_name: record.crate_name.clone(),
        section: record.section.clone(),
        requirement: record.requirement.clone(),
        line: record.line,
        column: record.column,
        outcome,
        completed,
        reason,
    }
}

#[derive(Serialize)]
struct JsonSummary {
    selected: usize,
    changed: usize,
    errors: usize,
}

#[derive(Serialize)]
struct JsonReport<'a> {
    version: u32,
    mode: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    files: Vec<JsonFile<'a>>,
    errors: Vec<JsonDiagnostic>,
    warnings: &'a [String],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    versions: Vec<JsonVersion>,
    summary: JsonSummary,
    exit_code: i32,
}

/// rustfmt reports its configuration as TOML. A machine consumer asking for
/// JSON should not have to carry a TOML parser to read one field of it.
fn config_value(dump: &str) -> serde_json::Value {
    match dump.parse::<toml_edit::DocumentMut>() {
        Ok(doc) => serde_json::Value::Object(
            doc.iter()
                .map(|(key, item)| (key.to_string(), item_value(item)))
                .collect(),
        ),
        Err(_) => serde_json::Value::String(dump.to_string()),
    }
}

fn item_value(item: &toml_edit::Item) -> serde_json::Value {
    match item {
        toml_edit::Item::Value(value) => scalar_value(value),
        toml_edit::Item::Table(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(key, item)| (key.to_string(), item_value(item)))
                .collect(),
        ),
        toml_edit::Item::ArrayOfTables(tables) => serde_json::Value::Array(
            tables
                .iter()
                .map(|table| {
                    serde_json::Value::Object(
                        table
                            .iter()
                            .map(|(key, item)| (key.to_string(), item_value(item)))
                            .collect(),
                    )
                })
                .collect(),
        ),
        toml_edit::Item::None => serde_json::Value::Null,
    }
}

fn scalar_value(value: &toml_edit::Value) -> serde_json::Value {
    match value {
        toml_edit::Value::String(text) => serde_json::Value::String(text.value().clone()),
        toml_edit::Value::Integer(number) => serde_json::Value::from(*number.value()),
        toml_edit::Value::Float(number) => serde_json::Number::from_f64(*number.value())
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        toml_edit::Value::Boolean(flag) => serde_json::Value::Bool(*flag.value()),
        toml_edit::Value::Datetime(stamp) => serde_json::Value::String(stamp.value().to_string()),
        toml_edit::Value::Array(entries) => {
            serde_json::Value::Array(entries.iter().map(scalar_value).collect())
        }
        toml_edit::Value::InlineTable(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(key, value)| (key.to_string(), scalar_value(value)))
                .collect(),
        ),
    }
}

/// A plain unified diff. Colour is applied later, by `paint_diff`, because the
/// same string is also embedded in `--message-format json`, where an escape
/// would be JSON-encoded and so survive any stream-level stripping.
pub fn unified_diff(path: &Path, before: &str, after: &str, context: usize) -> String {
    let display = path.display().to_string();
    TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(context)
        .header(&display, &display)
        .to_string()
}

pub fn paint_diff(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 64);
    for line in body.split_inclusive('\n') {
        let style = if line.starts_with("+++") || line.starts_with("---") {
            KEY
        } else if line.starts_with('@') {
            HUNK
        } else if line.starts_with('+') {
            ADDED
        } else if line.starts_with('-') {
            REMOVED
        } else {
            Style::new()
        };

        if style == Style::new() {
            out.push_str(line);
            continue;
        }
        let (text, newline) = match line.strip_suffix('\n') {
            Some(text) => (text, "\n"),
            None => (line, ""),
        };
        let _ = write!(out, "{style}{text}{style:#}{newline}");
    }
    out
}

pub fn paint_error(err: &impl std::fmt::Display, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "{FAILURE}Error:{FAILURE:#} {err}")
}

/// Once a run has acted on files, the honest thing to name is how many it
/// acted on. Only a run that changed nothing falls back to naming the target.
fn push_result_label(out: &mut String, result: &FormatResult, check: bool) {
    let changed = result.changed();
    let acted = if check {
        result.exit_code != 0
    } else {
        changed > 0
    };

    if acted {
        let _ = write!(out, "{}", count(changed, "file"));
        return;
    }

    match result.targets.as_slice() {
        [target] => push_target_label(out, target),
        targets => {
            let _ = write!(
                out,
                "{} targets, {}",
                targets.len(),
                count(result.files, "file")
            );
        }
    }
}

fn push_target_label(out: &mut String, target: &TargetKind) {
    match target {
        TargetKind::CargoProject { root_dir, .. } => {
            let _ = write!(out, "Cargo project {}", root_dir.display());
        }
        TargetKind::SingleFile(path) => {
            let _ = write!(out, "{}", path.display());
        }
        TargetKind::LooseDirectory {
            root_dir,
            files,
            toml_files,
        } => {
            let total = files.len() + toml_files.len();
            let _ = write!(out, "{} in {}", count(total, "file"), root_dir.display());
        }
        TargetKind::FileList {
            rust_files,
            toml_files,
        } => {
            let _ = write!(
                out,
                "{}",
                count(rust_files.len() + toml_files.len(), "file")
            );
        }
    }
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn needs_formatting_suffix(result: &FormatResult) -> &'static str {
    if result.changed() == 1 {
        "needs formatting"
    } else {
        "need formatting"
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::runner::{FileOutcome, Preview};

    fn options(check: bool) -> FormatterOptions {
        FormatterOptions {
            check,
            ..FormatterOptions::default()
        }
    }

    fn result(exit_code: i32, target: TargetKind) -> FormatResult {
        changed_result(exit_code, target, 0)
    }

    fn changed_result(exit_code: i32, target: TargetKind, changed: usize) -> FormatResult {
        let files = match &target {
            TargetKind::LooseDirectory {
                files, toml_files, ..
            } => files.len() + toml_files.len(),
            _ => 1,
        };
        FormatResult {
            exit_code,
            targets: vec![target],
            files,
            outcomes: (0..changed)
                .map(|i| FileOutcome {
                    path: PathBuf::from(format!("/tmp/changed/{i}.rs")),
                    language: Kind::Rust,
                    status: FileStatus::NeedsFormatting,
                    diff: None,
                })
                .collect(),
            file_errors: Vec::new(),
            warnings: Vec::new(),
            versions: Vec::new(),
            preview: None,
        }
    }

    fn cargo_target() -> TargetKind {
        TargetKind::CargoProject {
            manifest_path: PathBuf::from("/tmp/foo/Cargo.toml"),
            root_dir: PathBuf::from("/tmp/foo"),
            workspace_root: PathBuf::from("/tmp/foo"),
        }
    }

    fn file_target() -> TargetKind {
        TargetKind::SingleFile(PathBuf::from("/tmp/foo.rs"))
    }

    fn loose_target(n: usize) -> TargetKind {
        TargetKind::LooseDirectory {
            root_dir: PathBuf::from("/tmp/loose"),
            files: (0..n)
                .map(|i| PathBuf::from(format!("/tmp/loose/{i}.rs")))
                .collect(),
            toml_files: Vec::new(),
        }
    }

    #[test]
    fn a_write_run_that_changed_nothing_says_so() {
        let result = result(0, cargo_target());
        assert_eq!(
            summary_line(&result, false),
            "Already formatted: Cargo project /tmp/foo\n"
        );
    }

    #[test]
    fn a_preview_does_not_claim_the_file_was_already_formatted() {
        let mut result = result(0, file_target());
        result.preview = Some(Preview {
            path: PathBuf::from("/tmp/foo.rs"),
            language: Kind::Rust,
            content: b"fn main() {}\n".to_vec(),
        });
        let line = summary_line(&result, false);
        assert_eq!(line, "Previewed: /tmp/foo.rs\n");

        let mut buf = Vec::new();
        paint_summary(&line, false, true, 0, 0, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains('\x1b'), "{out}");
    }

    #[test]
    fn a_write_run_counts_only_the_files_it_wrote() {
        let result = changed_result(0, loose_target(9), 2);
        assert_eq!(summary_line(&result, false), "Formatted 2 files\n");
    }

    #[test]
    fn a_single_written_file_is_singular() {
        let result = changed_result(0, loose_target(9), 1);
        assert_eq!(summary_line(&result, false), "Formatted 1 file\n");
    }

    #[test]
    fn summary_formats_single_file() {
        let result = changed_result(0, file_target(), 1);
        assert_eq!(summary_line(&result, false), "Formatted 1 file\n");
    }

    #[test]
    fn summary_check_ok_cargo_project() {
        let result = result(0, cargo_target());
        assert_eq!(
            summary_line(&result, true),
            "Already formatted: Cargo project /tmp/foo\n"
        );
    }

    #[test]
    fn summary_check_ok_single_file() {
        let result = result(0, file_target());
        assert_eq!(
            summary_line(&result, true),
            "Already formatted: /tmp/foo.rs\n"
        );
    }

    #[test]
    fn summary_check_ok_loose_dir() {
        let result = result(0, loose_target(2));
        assert_eq!(
            summary_line(&result, true),
            "Already formatted: 2 files in /tmp/loose\n"
        );
    }

    #[test]
    fn summary_format_failed_single_file() {
        let result = result(1, file_target());
        assert_eq!(summary_line(&result, false), "Format failed: /tmp/foo.rs\n");
    }

    #[test]
    fn summary_format_failed_cargo_project() {
        let result = result(1, cargo_target());
        assert_eq!(
            summary_line(&result, false),
            "Format failed: Cargo project /tmp/foo\n"
        );
    }

    #[test]
    fn summary_check_failed_counts_the_mismatches() {
        let result = changed_result(1, cargo_target(), 3);
        assert_eq!(
            summary_line(&result, true),
            "Check failed: 3 files need formatting\n"
        );
    }

    #[test]
    fn summary_check_failed_one_file_is_singular() {
        let result = changed_result(1, loose_target(2), 1);
        assert_eq!(
            summary_line(&result, true),
            "Check failed: 1 file needs formatting\n"
        );
    }

    /// The label names the directory that was actually formatted. A workspace
    /// run substitutes the workspace root here, so reading `root_dir` is what
    /// keeps a member path from standing in for the whole workspace.
    #[test]
    fn a_cargo_label_names_the_formatted_root() {
        let target = TargetKind::CargoProject {
            manifest_path: PathBuf::from("/ws/crates/member/Cargo.toml"),
            root_dir: PathBuf::from("/ws"),
            workspace_root: PathBuf::from("/ws"),
        };
        assert_eq!(
            summary_line(&result(0, target), true),
            "Already formatted: Cargo project /ws\n"
        );
    }

    #[test]
    fn verbose_preamble_contains_core_fields() {
        let text = verbose_preamble(&[cargo_target()], &options(true));
        assert!(text.contains("target: cargo project\n"));
        assert!(text.contains("manifest: /tmp/foo/Cargo.toml\n"));
        assert!(text.contains("toolchain: auto\n"));
        assert!(text.contains("check: true\n"));
        assert!(!text.contains("/tmp/src"));
    }

    #[test]
    fn verbose_preamble_truncates_file_list() {
        let text = verbose_preamble(&[loose_target(21)], &options(false));
        assert!(text.contains("target: loose directory\n"));
        assert!(text.contains("files: 21\n"));
        assert!(text.contains("  /tmp/loose/0.rs\n"));
        assert!(text.contains("  /tmp/loose/19.rs\n"));
        assert!(!text.contains("  /tmp/loose/20.rs\n"));
        assert!(text.contains("  ... and 1 more\n"));
        assert!(text.contains("toolchain: auto\n"));
        assert!(text.contains("check: false\n"));
    }

    #[test]
    fn status_style_success_is_green() {
        assert_eq!(
            status_style(0).get_fg_color(),
            Some(Color::Ansi(AnsiColor::Green))
        );
        assert_eq!(
            status_style(1).get_fg_color(),
            Some(Color::Ansi(AnsiColor::Red))
        );
    }

    #[test]
    fn paint_summary_styles_formatted_prefix() {
        let mut buf = Vec::new();
        paint_summary("Formatted /tmp/foo.rs\n", false, false, 0, 1, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Formatted"));
        assert!(out.contains("/tmp/foo.rs"));
        assert!(out.contains('\x1b'));
        assert!(out.ends_with("/tmp/foo.rs\n"));
    }

    #[test]
    fn paint_summary_styles_format_failed_prefix() {
        let mut buf = Vec::new();
        paint_summary("Format failed: /tmp/foo.rs\n", false, false, 1, 0, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Format failed:"));
        assert!(out.contains("/tmp/foo.rs"));
        assert!(out.contains('\x1b'));
    }

    #[test]
    fn paint_verbose_styles_keys_not_file_rows() {
        let mut buf = Vec::new();
        paint_verbose("target: file\n  /tmp/foo.rs\n", &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("target:"));
        assert!(out.contains('\x1b'));
        assert!(out.contains("  /tmp/foo.rs\n"));
        let file_row = out.split('\n').nth(1).unwrap();
        assert!(!file_row.contains('\x1b'));
    }

    #[test]
    fn paint_error_styles_label() {
        let mut buf = Vec::new();
        paint_error(&"boom", &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Error:"));
        assert!(out.contains("boom"));
        assert!(out.contains('\x1b'));
        assert!(out.ends_with("boom\n"));
    }
}
