use std::{
    any::Any,
    io,
    path::{Path, PathBuf},
};

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// The machine-readable projection of an error: what `--message-format json`
/// emits instead of a rendered sentence. `code` is a stable discriminant, so a
/// consumer can branch on the kind without matching on prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic<'a> {
    pub code: &'static str,
    pub path: Option<&'a Path>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
}

/// Appended to a watcher failure the platform blamed on its own limit, which is
/// the only one a reader can act on directly.
const WATCH_LIMIT: &str = "\n\
    This is the kernel's limit on watches, not a problem with the tree. Raise\n\
    it with:\n    \
    sudo sysctl fs.inotify.max_user_watches=524288\n\
    or watch less of the tree by naming a directory.";

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error(
        "A toolchain was named, but rustup was not found in PATH and no rustfmt\n\
         could be found to honour the pin with.\n\
         Install rustup from https://rustup.rs, put a rustfmt on PATH, point\n\
         $RUSTFMT at one, or use --toolchain auto."
    )]
    RustupNotFound,

    #[error(
        "No rustfmt could be found.\n\
         rust-formatter looks at $RUSTFMT, then rustup's nightly and active\n\
         toolchains, then PATH.\n\
         Install one with:\n    \
         rustup toolchain install nightly --component rustfmt\n\
         or put a rustfmt on PATH."
    )]
    RustfmtUnresolved,

    #[error(
        "rustfmt component is not available for toolchain '{toolchain}'.\n\
         Details: {details}\n\
         Please install it with:\n    \
         rustup toolchain install {toolchain} --component rustfmt"
    )]
    RustfmtNotFound { toolchain: String, details: String },

    #[error(
        "Toolchain '{toolchain}' could not be asked what rustfmt options it has\n\
         (resolved rustfmt: {rustfmt}).\n\
         Details: {details}\n\
         Install a working rustfmt with:\n    \
         rustup toolchain install {toolchain} --component rustfmt"
    )]
    UnstableRustfmtRequired {
        toolchain: String,
        rustfmt: String,
        details: String,
    },

    #[error(
        "{what} needs a nightly rustfmt, and '{toolchain}' is not one\n\
         (resolved rustfmt: {version}).\n\
         Pass --toolchain nightly, or drop the option."
    )]
    NightlyRustfmtRequired {
        what: String,
        toolchain: String,
        version: String,
    },

    #[error("Target path does not exist: {}", .0.display())]
    PathNotFound(PathBuf),

    #[error("Unsupported target (expected a directory, Cargo project, .rs file, or .toml file): {}", .0.display())]
    UnsupportedTarget(PathBuf),

    #[error("No formattable (.rs or .toml) files found in path: {}", .0.display())]
    NoFormattableFilesFound(PathBuf),

    #[error(
        "The filesystem watcher could not be started{}.\n\
         Details: {details}{}",
        .path.as_ref().map(|path| format!(" for {}", path.display())).unwrap_or_default(),
        if *.limit { WATCH_LIMIT } else { "" }
    )]
    Watch {
        /// The path the platform blamed, when it named one.
        path: Option<PathBuf>,
        /// Whether the failure was the platform's limit on watches, which is
        /// the one case with a command that fixes it.
        limit: bool,
        details: String,
    },

    /// `details` is the rustc-style rendering a reader wants; `message`, `line`
    /// and `column` are the same failure as data, so the JSON report does not
    /// have to be regular-expressed back apart.
    #[error("Failed to parse TOML file {}: {details}", .path.display())]
    TomlParse {
        path: PathBuf,
        line: Option<usize>,
        column: Option<usize>,
        message: String,
        details: String,
    },

    #[error("{}:{line}:{column}: {message}", .path.display())]
    RustfmtDiagnostic {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },

    #[error(
        "{version} has no option `{option}`.\n\
         Run `rust-formatter --print-config` to see the options it does have."
    )]
    UnknownRustfmtOption { option: String, version: String },

    #[error(
        "{} sets `ignore`, which rustfmt resolves against the directory that \
         file is in, so it cannot be carried into the temporary configuration \
         that `--config {option}` needs.\n\
         Pass the same patterns to --exclude, which applies to TOML as well.",
        .path.display()
    )]
    ConflictingIgnore { path: PathBuf, option: String },

    #[error("{}: {message}", .path.display())]
    Config { path: PathBuf, message: String },

    #[error("unknown preset `{name}`.\nKnown presets: {known}")]
    UnknownPreset { name: String, known: String },

    #[error("registry lookup failed for '{crate_name}': {details}")]
    RegistryLookup { crate_name: String, details: String },

    #[error("{command} exited with code {code}:\n{details}")]
    ToolFailed {
        command: String,
        code: i32,
        details: String,
    },

    #[error("Failed to execute command '{command}': {source}")]
    CommandExecutionFailed {
        command: String,
        #[source]
        source: io::Error,
    },

    #[error("Invalid glob pattern: {0}")]
    InvalidGlob(String),

    #[error(
        "git was not found in PATH.\n\
         --since and --staged select files by asking git what changed."
    )]
    GitNotFound,

    #[error("Not inside a git repository: {}", .0.display())]
    NotAGitRepository(PathBuf),

    #[error("Unknown git revision: {0}")]
    UnknownGitRef(String),

    #[error("git command failed ({command}): {details}")]
    GitCommandFailed { command: String, details: String },

    #[error(
        "--files-from - reads a list of paths from standard input, but standard input is a terminal.\n\
         Pipe a list in, or pass a file path instead."
    )]
    StdinIsATerminal,

    #[error("A formatter worker thread panicked: {message}")]
    ThreadPanicked { message: String },

    #[error(
        "--emit stdout writes one formatted file to standard output, but the \
         selection resolves to {count} files.\n\
         Name a single file, or drop --emit stdout to rewrite them in place."
    )]
    PreviewNotSingleFile { count: usize },

    #[error("I/O error at {}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(transparent)]
    Usage(#[from] Usage),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Usage {
    #[error(
        "--stdin formats standard input and takes no PATH; name the input with --stdin-filepath"
    )]
    StdinWithPath,

    #[error("--emit stdout needs --stdin or exactly one PATH")]
    PreviewNeedsOnePath,

    #[error("--watch rewrites files as they change, so it cannot be combined with --emit stdout")]
    WatchWithPreview,

    #[error("--restage rewrites files, so it cannot be combined with --emit stdout")]
    RestageWithPreview,

    #[error("--emit stdout prints a formatted file, so it cannot be combined with a listing")]
    PreviewWithListing,

    #[error("--full-versions rewrites Cargo.toml, which --rust-only excludes")]
    FullVersionsWithRustOnly,

    #[error("--registry-url names an index to fetch from, which --offline forbids")]
    RegistryUrlWhileOffline,

    #[error("--recurse-submodules narrows --since or --staged, which is what asks git for a scope")]
    RecurseSubmodulesWithoutGitScope,

    #[error("--toml-version 1.0 cannot be combined with --toml-inline-tables expand")]
    Toml10WithExpandedInlineTables,

    #[error("--sort-grouped cannot be combined with --toml-max-blank-lines 0")]
    SortGroupedWithoutBlankLines,

    #[error("--range needs --stdin or exactly one PATH")]
    RangeNeedsOnePath,

    #[error("--range formats Rust only; TOML is formatted as a whole document")]
    RangeOnToml,

    #[error("--range needs a .rs file or --stdin")]
    RangeNeedsRustFile,

    #[error("{flag} {value} conflicts with --config {key}={configured}")]
    EditionConflict {
        flag: &'static str,
        value: String,
        key: &'static str,
        configured: String,
    },

    #[error("invalid value `{value}` for {key}: expected one of {}", .expected.join(", "))]
    InvalidEdition {
        key: &'static str,
        value: String,
        expected: &'static [&'static str],
    },

    #[error("expected a list of patterns, found `{raw}`")]
    PatternListExpected { raw: String },

    #[error("expected KEY=VALUE in --config, found `{entry}`")]
    ConfigEntryWithoutValue { entry: String },

    #[error("--config entry `{entry}` has no key")]
    ConfigEntryWithoutKey { entry: String },
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// `source` has to be the text that was handed to the parser, because the
    /// span is a byte offset into that exact buffer -- the decoded, newline-
    /// normalized form, not the bytes as they sit on disk.
    pub fn toml_parse(path: impl Into<PathBuf>, source: &str, err: &toml_edit::TomlError) -> Self {
        let position = err.span().and_then(|span| line_column(source, span.start));
        Self::TomlParse {
            path: path.into(),
            line: position.map(|(line, _)| line),
            column: position.map(|(_, column)| column),
            message: err.message().to_string(),
            details: err.to_string(),
        }
    }

    pub fn thread_panicked(payload: &(dyn Any + Send)) -> Self {
        let message = payload
            .downcast_ref::<&'static str>()
            .map(|text| (*text).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "no message".to_string());
        Self::ThreadPanicked { message }
    }

    /// The whole enum projected onto one shape. A new variant that forgets to
    /// name itself here would report as the wrong kind, so every arm is written
    /// out rather than defaulted.
    pub fn diagnostic(&self) -> Diagnostic<'_> {
        fn plain(code: &'static str, message: String) -> Diagnostic<'static> {
            Diagnostic {
                code,
                path: None,
                line: None,
                column: None,
                message,
            }
        }
        fn at<'a>(code: &'static str, path: &'a Path, message: String) -> Diagnostic<'a> {
            Diagnostic {
                code,
                path: Some(path),
                line: None,
                column: None,
                message,
            }
        }
        let plain = |code| plain(code, self.to_string());
        let at = |code, path| at(code, path, self.to_string());

        match self {
            Self::RustupNotFound => plain("rustup-not-found"),
            Self::RustfmtUnresolved => plain("rustfmt-unresolved"),
            Self::RustfmtNotFound { .. } => plain("rustfmt-not-found"),
            Self::UnstableRustfmtRequired { .. } => plain("unstable-rustfmt-required"),
            Self::NightlyRustfmtRequired { .. } => plain("nightly-rustfmt-required"),
            Self::PathNotFound(path) => at("path-not-found", path),
            Self::UnsupportedTarget(path) => at("unsupported-target", path),
            Self::NoFormattableFilesFound(path) => at("no-formattable-files-found", path),
            Self::Watch { path, .. } => match path {
                Some(path) => at("watch-failed", path),
                None => plain("watch-failed"),
            },
            Self::TomlParse {
                path,
                line,
                column,
                message,
                ..
            } => Diagnostic {
                code: "toml-parse",
                path: Some(path),
                line: *line,
                column: *column,
                message: message.clone(),
            },
            Self::RustfmtDiagnostic {
                path,
                line,
                column,
                message,
            } => Diagnostic {
                code: "rustfmt-diagnostic",
                path: Some(path),
                line: Some(*line),
                column: Some(*column),
                message: message.clone(),
            },
            Self::UnknownRustfmtOption { .. } => plain("unknown-rustfmt-option"),
            Self::ConflictingIgnore { path, .. } => at("conflicting-ignore", path),
            Self::Config { path, message } => Diagnostic {
                code: "config",
                path: Some(path),
                line: None,
                column: None,
                message: message.clone(),
            },
            Self::UnknownPreset { .. } => plain("unknown-preset"),
            Self::RegistryLookup { .. } => plain("registry-lookup"),
            Self::ToolFailed { .. } => plain("tool-failed"),
            Self::CommandExecutionFailed { .. } => plain("command-execution-failed"),
            Self::InvalidGlob(_) => plain("invalid-glob"),
            Self::GitNotFound => plain("git-not-found"),
            Self::NotAGitRepository(path) => at("not-a-git-repository", path),
            Self::UnknownGitRef(_) => plain("unknown-git-ref"),
            Self::GitCommandFailed { .. } => plain("git-command-failed"),
            Self::StdinIsATerminal => plain("stdin-is-a-terminal"),
            Self::ThreadPanicked { .. } => plain("thread-panicked"),
            Self::PreviewNotSingleFile { .. } => plain("preview-not-single-file"),
            Self::Io { path, source } => Diagnostic {
                code: "io",
                path: Some(path),
                line: None,
                column: None,
                message: source.to_string(),
            },
            Self::Usage(_) => plain("usage"),
        }
    }
}

pub(crate) fn line_column(source: &str, offset: usize) -> Option<(usize, usize)> {
    let head = source.get(..offset)?;
    let line = head.matches('\n').count() + 1;
    let column = head.rsplit('\n').next().unwrap_or_default().chars().count() + 1;
    Some((line, column))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_failure(source: &str) -> Error {
        let err = source
            .parse::<toml_edit::DocumentMut>()
            .expect_err("this source does not parse");
        Error::toml_parse("/tmp/a.toml", source, &err)
    }

    #[test]
    fn a_toml_failure_carries_its_position_as_data() {
        let error = parse_failure("a = 1\nb = = 2\n");
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.code, "toml-parse");
        assert_eq!(diagnostic.path, Some(Path::new("/tmp/a.toml")));
        assert_eq!(diagnostic.line, Some(2));
        assert_eq!(diagnostic.column, Some(5));
        assert!(!diagnostic.message.contains("/tmp/a.toml"));
    }

    /// The rendered form is what a reader sees, so it keeps the snippet the
    /// structured fields deliberately drop.
    #[test]
    fn a_toml_failure_still_renders_for_a_reader() {
        let rendered = parse_failure("a = 1\nb = = 2\n").to_string();
        assert!(rendered.starts_with("Failed to parse TOML file /tmp/a.toml:"));
        assert!(rendered.contains("line 2, column 5"), "{rendered}");
    }

    #[test]
    fn a_position_past_the_end_is_reported_as_absent() {
        assert_eq!(line_column("ab", 9), None);
        assert_eq!(line_column("ab\ncd", 3), Some((2, 1)));
        assert_eq!(line_column("", 0), Some((1, 1)));
    }

    #[test]
    fn a_panic_payload_is_kept_whatever_shape_it_arrives_in() {
        let from_str = Error::thread_panicked(&"literal boom");
        assert!(from_str.to_string().ends_with("literal boom"));

        let from_string = Error::thread_panicked(&"formatted boom".to_string());
        assert!(from_string.to_string().ends_with("formatted boom"));

        let opaque = Error::thread_panicked(&7_u8);
        assert!(opaque.to_string().ends_with("no message"));
        assert_eq!(opaque.diagnostic().code, "thread-panicked");
    }

    #[test]
    fn a_rustfmt_diagnostic_projects_all_four_fields() {
        let error = Error::RustfmtDiagnostic {
            path: PathBuf::from("/tmp/a.rs"),
            line: 12,
            column: 5,
            message: "error: expected expression".to_string(),
        };
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.code, "rustfmt-diagnostic");
        assert_eq!(diagnostic.path, Some(Path::new("/tmp/a.rs")));
        assert_eq!(diagnostic.line, Some(12));
        assert_eq!(diagnostic.column, Some(5));
        assert_eq!(diagnostic.message, "error: expected expression");
    }

    #[test]
    fn an_error_with_no_location_still_carries_its_message() {
        let error = Error::ToolFailed {
            command: "cargo metadata".to_string(),
            code: 101,
            details: "no such manifest".to_string(),
        };
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.code, "tool-failed");
        assert_eq!(diagnostic.path, None);
        assert!(diagnostic.message.contains("exited with code 101"));
    }
}
