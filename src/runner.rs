use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read, Write},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    thread,
};

use ahash::AHashMap;
use serde::Deserialize;
use tempfile::NamedTempFile;

use crate::{
    cache::{self, Cache, Fingerprint},
    config::RustfmtConfig,
    detector::{
        Boundary, TargetKind, WorkspaceScope, bounded_ancestors, collect, find_cargo_manifest,
        is_rust_path, is_toml_path, simplify_path, workspace_member_dirs, workspace_root,
    },
    error::{Error, Result},
    output::{ColorChoice, Emit, ListMode, MessageFormat, Streams},
    pool,
    registry::{RegistryCli, RegistryOptions},
    selection::{self, FileSource, Kind, Languages, Plan, SelectionOptions, Selector},
    semver::PartialVersion,
    toml_directive::Regions,
    toml_fmt::{
        ManifestContext, TomlFormatOutput, format_toml, format_toml_with_versions,
        owned_dep_requests,
    },
    toml_lint::{TomlIssue, toml_1_0_issues},
    toml_style::{TomlStyle, TomlVersion},
    toolchain::{self, InstallPolicy, Resolved},
    versions::{
        DepRequest, LookupPolicy, RegistryLookup, Resolution, SkipReason, UpgradePolicy,
        VersionLookup, VersionRecord,
    },
};

/// Most files one rustfmt invocation is given. The real limit is the command
/// line, which `RUSTFMT_ARGV_BUDGET` bounds; this bounds how much work is lost
/// when one invocation fails.
const RUSTFMT_CHUNK_SIZE: usize = 500;
/// How many bytes of argv one rustfmt invocation may carry. Windows caps a
/// whole command line at 32767 characters, which 500 absolute paths overrun on
/// their own; elsewhere the limit is `ARG_MAX` and this is only a courtesy.
const RUSTFMT_ARGV_BUDGET: usize = if cfg!(windows) { 30_000 } else { 128 * 1024 };
/// Room inside that budget for everything but the file names: the binary, the
/// `--config` argument with every option in it, `--config-path`, `--edition`.
const RUSTFMT_ARGV_RESERVE: usize = 8192;
/// The edition a file with no owning manifest is formatted at. rustfmt's own
/// default is 2015, under which `async`, `dyn` and `try` are not even keywords,
/// so a loose script fails to parse rather than being formatted.
pub const DEFAULT_EDITION: &str = "2024";
/// rustfmt does not always reach a fixed point in one pass: with
/// `group_imports` and `imports_granularity` both set, a comment between two
/// imports takes two (rustfmt#6195). Formatting to a fixed point is what keeps
/// a write run's own output clean under `--check`.
const MAX_RUSTFMT_PASSES: usize = 3;
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Debug, Clone)]
pub struct FormatterOptions {
    pub targets: Vec<PathBuf>,
    pub source: FileSource,
    pub selection: SelectionOptions,
    pub check: bool,
    pub toolchain: String,
    pub edition: Option<String>,
    /// An edition a configuration source named rather than the caller. It ranks
    /// below the project's own `rustfmt.toml` and below each package's
    /// `edition`, so it only replaces the built-in default.
    pub edition_fallback: Option<String>,
    pub style_edition: Option<String>,
    /// Line ranges to restrict formatting to, for an editor's format-selection
    /// request. Empty means the whole file.
    pub ranges: Vec<LineRange>,
    pub all: bool,
    /// `--staged --restage`: re-add every formatted path to the index it came
    /// from, so a pre-commit hook commits what was formatted.
    pub restage: bool,
    pub verbose: bool,
    pub quiet: bool,
    pub config: RustfmtConfig,
    /// The keys `--unset-config` named, kept so they can be checked against the
    /// options rustfmt actually has.
    pub unset_configs: Vec<String>,
    /// The subset of those this run never set, which is a no-op worth naming
    /// rather than a silent one.
    pub unset_misses: Vec<String>,
    /// The configuration sources this run read, in the order they applied, for
    /// `-v` to name.
    pub config_sources: Vec<String>,
    /// Option names a configuration source supplied rather than the caller. A
    /// key this rustfmt does not have is a typo when it was typed and a
    /// portability problem when a repository's own file carries it, so these
    /// are dropped with a warning instead of failing the run.
    pub lenient_keys: std::collections::BTreeSet<String>,
    pub toml_style: TomlStyle,
    pub extra_args: Vec<String>,
    pub full_versions: bool,
    pub upgrade: UpgradePolicy,
    pub allow_yanked: bool,
    pub ignore_rust_version: bool,
    /// An index base URL that replaces crates.io for this run.
    pub registry_url: Option<String>,
    /// Never reach the network: neither the registry nor `cargo metadata`.
    ///
    /// There is no `--locked` beside it: `cargo metadata --no-deps` resolves
    /// nothing, so there is no lock file for it to pin or update, and passing
    /// the flag through would be shipping a no-op.
    pub offline: bool,
    pub jobs: Option<NonZeroUsize>,
    /// Skip a file whose bytes and effective configuration a previous run
    /// already proved to be a fixed point. Off by default for a library
    /// caller, which should not silently acquire on-disk state; the binary
    /// turns it on.
    pub cache: bool,
    pub fail_fast: bool,
    /// `--install-toolchain`: whether a missing rustfmt component may be
    /// installed. `None` leaves the choice to the session -- a prompt when
    /// there is someone to answer it, the printed command otherwise.
    pub install_toolchain: Option<bool>,
    pub color: ColorChoice,
    /// Lines of context around each hunk of a `--check` diff.
    pub diff_context: usize,
    pub emit: Emit,
    pub list: ListMode,
    pub message_format: MessageFormat,
    pub print_config: bool,
    pub stdin: bool,
    pub stdin_filepath: Option<PathBuf>,
}

impl Default for FormatterOptions {
    fn default() -> Self {
        Self {
            targets: vec![PathBuf::from(".")],
            source: FileSource::Paths,
            selection: SelectionOptions::default(),
            check: false,
            toolchain: toolchain::AUTO.to_string(),
            edition: None,
            edition_fallback: None,
            style_edition: None,
            ranges: Vec::new(),
            all: true,
            restage: false,
            verbose: false,
            quiet: false,
            config: RustfmtConfig::default(),
            unset_configs: Vec::new(),
            unset_misses: Vec::new(),
            config_sources: Vec::new(),
            lenient_keys: std::collections::BTreeSet::new(),
            toml_style: TomlStyle::default(),
            extra_args: Vec::new(),
            full_versions: false,
            upgrade: UpgradePolicy::default(),
            allow_yanked: false,
            ignore_rust_version: false,
            registry_url: None,
            offline: false,
            jobs: None,
            cache: false,
            fail_fast: false,
            install_toolchain: Some(false),
            color: ColorChoice::Never,
            diff_context: crate::report::DEFAULT_DIFF_CONTEXT,
            emit: Emit::Files,
            list: ListMode::None,
            message_format: MessageFormat::Human,
            print_config: false,
            stdin: false,
            stdin_filepath: None,
        }
    }
}

impl FormatterOptions {
    pub fn for_path(path: impl Into<PathBuf>) -> Self {
        Self {
            targets: vec![path.into()],
            ..Self::default()
        }
    }

    pub fn worker_threads(&self) -> usize {
        self.jobs.map_or_else(parallelism, NonZeroUsize::get)
    }

    pub fn selector(&self) -> Result<Selector> {
        Ok(Selector::new(&self.selection)?.with_threads(self.worker_threads()))
    }

    /// `--list-different` and `--check` both need to know which files differ;
    /// only the second of them wants the contents of the difference, in either
    /// output format -- a list asked for names.
    pub(crate) fn wants_diff(&self) -> bool {
        self.check && !self.list.is_listing()
    }

    fn lookup_policy(&self) -> LookupPolicy {
        LookupPolicy {
            upgrade: self.upgrade,
            allow_yanked: self.allow_yanked,
            ignore_rust_version: self.ignore_rust_version,
        }
    }

    /// The registry client this run needs, or `None` when it was not asked for.
    /// A client that cannot even be built is a tool failure, not a skip.
    fn version_lookup(&self) -> Result<Option<RegistryLookup>> {
        if !self.full_versions {
            return Ok(None);
        }
        let start = self
            .targets
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."));
        let options = RegistryOptions::resolve(
            &start,
            &crate::cargo_config::process_env,
            &RegistryCli {
                registry_url: self.registry_url.clone(),
                offline: self.offline,
                concurrency: self.worker_threads(),
            },
        );
        RegistryLookup::new(options, self.lookup_policy(), self.worker_threads())
            .map(Some)
            .map_err(|err| Error::RegistryLookup {
                crate_name: "<registry>".to_string(),
                details: err.to_string(),
            })
    }
}

/// A 1-based, inclusive range of lines, as rustfmt's `--file-lines` counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// Rewritten on disk by this run.
    Formatted,
    /// Differs from its formatted form; `--check` left it alone.
    NeedsFormatting,
}

/// One file the run reached a definite verdict about. Only files that actually
/// differ appear here, which is what separates an honest count from the number
/// of files merely handed to a formatter.
#[derive(Debug, Clone)]
pub struct FileOutcome {
    pub path: PathBuf,
    pub language: Kind,
    pub status: FileStatus,
    pub diff: Option<String>,
}

#[derive(Debug)]
pub struct FormatResult {
    pub exit_code: i32,
    pub targets: Vec<TargetKind>,
    /// Files handed to a formatter. Selected, not written: a clean file still
    /// counts. Use `changed()` for the number this run acted on.
    pub files: usize,
    pub outcomes: Vec<FileOutcome>,
    pub file_errors: Vec<Error>,
    /// Non-fatal findings, already phrased for display. They never reach
    /// `process_exit_code`.
    pub warnings: Vec<String>,
    /// What `--full-versions` did to each dependency it considered.
    pub versions: Vec<(PathBuf, VersionRecord)>,
    /// What `--emit stdout` produced, if that is what was asked for.
    pub preview: Option<Preview>,
}

/// One file's formatted bytes, delivered rather than written. Bytes rather than
/// text so a previewed TOML file keeps the BOM and line endings a write run
/// would have left on disk.
#[derive(Debug)]
pub struct Preview {
    pub path: PathBuf,
    pub language: Kind,
    pub content: Vec<u8>,
}

impl FormatResult {
    /// Per-file failures outrank `exit_code`: a tree that formatted cleanly
    /// apart from one unreadable file is still a tool error, not a diff.
    pub fn process_exit_code(&self) -> i32 {
        if self.file_errors.is_empty() {
            self.exit_code
        } else {
            2
        }
    }

    pub fn changed(&self) -> usize {
        self.outcomes.len()
    }

    pub fn changed_paths(&self) -> impl Iterator<Item = &Path> {
        self.outcomes.iter().map(|outcome| outcome.path.as_path())
    }

    pub fn mismatched_toml(&self) -> impl Iterator<Item = &Path> {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.language == Kind::Toml)
            .map(|outcome| outcome.path.as_path())
    }
}

#[derive(Default)]
struct TomlOutcome {
    changed: Vec<FileOutcome>,
    errors: Vec<Error>,
    /// Kept structured until the whole run is in, so that findings from
    /// parallel workers can be put back into document order.
    findings: Vec<(PathBuf, TomlIssue)>,
    records: Vec<(PathBuf, VersionRecord)>,
}

#[derive(Clone)]
pub(crate) struct SourceText {
    bom: bool,
    crlf: bool,
    pub(crate) text: String,
    mixed_endings_original: Option<String>,
}

impl SourceText {
    pub(crate) fn with_newline_style(self, style: NewlineStyle) -> Self {
        let crlf = match style {
            NewlineStyle::Auto => self.crlf,
            NewlineStyle::Unix => false,
            NewlineStyle::Windows => true,
        };
        Self { crlf, ..self }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NewlineStyle {
    Auto,
    Unix,
    Windows,
}

impl NewlineStyle {
    fn effective(options: &FormatterOptions, project: Option<&Path>) -> Self {
        options
            .config
            .get("newline_style")
            .map(str::to_owned)
            .or_else(|| {
                project.and_then(|config| crate::rustfmt_config::setting(config, "newline_style"))
            })
            .map_or(Self::Auto, |value| Self::named(&value))
    }

    fn named(value: &str) -> Self {
        let value = value.trim().trim_matches('"').to_ascii_lowercase();
        let native = if cfg!(windows) { "windows" } else { "unix" };
        let resolved = if value == "native" {
            native
        } else {
            value.as_str()
        };
        match resolved {
            "unix" => Self::Unix,
            "windows" => Self::Windows,
            _ => Self::Auto,
        }
    }
}

enum TomlFileStatus {
    Clean,
    Changed(FileOutcome),
    Failed(Error),
}

/// Rust files that can share one rustfmt invocation. `edition` is `None` when
/// no `--edition` is to be passed at all, which is how a project's own
/// `rustfmt.toml` keeps the last word on it. `project_config` is the
/// `rustfmt.toml` rustfmt would have discovered for these files, and is carried
/// only when an option has to travel by `--config-path`, which replaces that
/// discovery instead of adding to it.
struct RustBatch {
    edition: Option<String>,
    project_config: Option<PathBuf>,
    files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
struct BatchArgs<'a> {
    edition: Option<&'a str>,
    config_path: Option<&'a Path>,
    /// `skip_children` has a CLI flag only on nightly, so on stable it has to
    /// ride along in the `--config` argument instead. Carrying it here keeps
    /// the decision in one place rather than at each of the invocation sites.
    skip_children: bool,
    newline_style: NewlineStyle,
}

/// What the resolved rustfmt actually offers, rather than what this tool assumes
/// it offers. The two options the wrapper exists to set are unstable, and an
/// option that moved or vanished upstream should be reported by name instead of
/// reaching the user as rustfmt's own `invalid key=val pair`.
#[derive(Debug)]
pub struct RustfmtCapabilities {
    rustfmt: PathBuf,
    options: HashSet<String>,
    version: OnceLock<String>,
    /// Whether this rustfmt accepts `--unstable-features`, which is the gate
    /// on every nightly-only part of the CLI: `--emit json`, `--file-lines`
    /// and `--skip-children`. The two *options* this tool sets are not gated
    /// there -- `--config` delivers them on stable as well -- so this says
    /// which transports are reachable, not whether formatting is possible.
    unstable_cli: bool,
}

impl RustfmtCapabilities {
    pub fn has(&self, option: &str) -> bool {
        self.options.contains(option)
    }

    /// True for a nightly rustfmt, false for stable and beta.
    pub fn unstable_cli(&self) -> bool {
        self.unstable_cli
    }

    /// This probe, as something a later run can read back.
    fn remember(&self) -> cache::Toolchain {
        let mut options: Vec<String> = self.options.iter().cloned().collect();
        options.sort_unstable();
        cache::Toolchain {
            rustfmt: self.rustfmt.clone(),
            version: self.version().to_string(),
            unstable_cli: self.unstable_cli,
            options,
        }
    }

    /// Spawned only when something has to name the toolchain -- a warning, an
    /// error, or `-v`. A run that has nothing to say about rustfmt should not
    /// pay a process to ask it its name.
    pub fn version(&self) -> &str {
        self.version.get_or_init(|| {
            Command::new(&self.rustfmt)
                .arg("--version")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map_or_else(
                    || "rustfmt".to_string(),
                    |output| String::from_utf8_lossy(&output.stdout).trim().to_owned(),
                )
        })
    }

    /// One spawn: `--print-config default OUT` writes rustfmt's own defaults to
    /// OUT and reads no input file, so the answer cannot be coloured by a stray
    /// `rustfmt.toml` next to whatever probe file was chosen.
    ///
    /// The same spawn answers a second question. A nightly rustfmt accepts
    /// `--unstable-features`; a stable one rejects the flag outright, and
    /// rejecting it is the only reliable way to tell the channels apart
    /// without parsing a version string. So the flag is tried first and
    /// dropped on failure: nightly still pays one spawn, stable pays two, and
    /// both end up with the same option inventory -- stable's
    /// `--print-config` lists the unstable option names too, which is what
    /// keeps `reconcile_with_rustfmt` honest on either channel.
    fn probe(rustfmt: &Path) -> Result<Self> {
        let dump = NamedTempFile::with_suffix(".toml").map_err(|err| Error::io("<probe>", err))?;
        let mut unstable_cli = true;
        let mut printed = Self::dump(rustfmt, dump.path(), unstable_cli)?;
        if !printed.success() {
            unstable_cli = false;
            printed = Self::dump(rustfmt, dump.path(), unstable_cli)?;
        }
        if !printed.success() {
            return Err(Error::ToolFailed {
                command: "rustfmt --print-config default".to_string(),
                code: printed.code().unwrap_or(1),
                details: "rustfmt could not dump its default configuration".to_string(),
            });
        }

        let text = fs::read_to_string(dump.path()).map_err(|err| Error::io(dump.path(), err))?;
        let options: HashSet<String> = text
            .lines()
            .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim().to_owned()))
            .filter(|key| !key.is_empty())
            .collect();
        if options.is_empty() {
            return Err(Error::ToolFailed {
                command: "rustfmt --print-config default".to_string(),
                code: 0,
                details: "rustfmt dumped no configuration options".to_string(),
            });
        }

        Ok(Self {
            rustfmt: rustfmt.to_path_buf(),
            options,
            version: OnceLock::new(),
            unstable_cli,
        })
    }

    fn dump(rustfmt: &Path, out: &Path, unstable: bool) -> Result<std::process::ExitStatus> {
        let mut cmd = Command::new(rustfmt);
        if unstable {
            cmd.arg("--unstable-features");
        }
        cmd.arg("--print-config")
            .arg("default")
            .arg(out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|err| Error::CommandExecutionFailed {
                command: "rustfmt --print-config default".to_string(),
                source: err,
            })
    }
}

/// One probe per rustfmt per process. A run spawns rustfmt once per chunk per
/// pass, so asking again each time would be the most expensive question the
/// tool asks.
static CAPABILITIES: OnceLock<Mutex<HashMap<PathBuf, Arc<RustfmtCapabilities>>>> = OnceLock::new();

pub fn capabilities(rustfmt: &Path) -> Result<Arc<RustfmtCapabilities>> {
    let cache = CAPABILITIES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(found) = cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(rustfmt)
    {
        return Ok(Arc::clone(found));
    }
    let probed = Arc::new(RustfmtCapabilities::probe(rustfmt)?);
    cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(rustfmt.to_path_buf(), Arc::clone(&probed));
    Ok(probed)
}

/// The rustfmt a run drives, together with what it can do.
///
/// The two travel as one because the answer to "which arguments does this
/// invocation get" depends on the channel: a stable rustfmt has no
/// `--unstable-features`, no `--emit json`, no `--skip-children` and no
/// `--file-lines`, while accepting through `--config` every option this tool
/// actually sets.
#[derive(Debug, Clone)]
pub struct Rustfmt {
    path: PathBuf,
    capabilities: Arc<RustfmtCapabilities>,
}

impl Rustfmt {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn capabilities(&self) -> &RustfmtCapabilities {
        &self.capabilities
    }

    fn unstable_cli(&self) -> bool {
        self.capabilities.unstable_cli()
    }
}

pub fn verify_toolchain(toolchain: &str) -> Result<PathBuf> {
    resolve_toolchain(toolchain, InstallPolicy::Never, false).map(|(resolved, _)| resolved.rustfmt)
}

/// Find a rustfmt and ask it what it can do.
///
/// The two halves are separate failures. Not finding one at all is an
/// environment problem the resolver reports; finding one that cannot be
/// interrogated is a broken installation, and reporting that as "not nightly"
/// -- which is what an earlier `let Ok(..) else` did -- sent people to install
/// a toolchain they already had.
fn resolve_toolchain(
    toolchain: &str,
    install: InstallPolicy,
    cache: bool,
) -> Result<(Resolved, Arc<RustfmtCapabilities>)> {
    let env: crate::cargo_config::EnvLookup<'_> = &crate::cargo_config::process_env;
    let key = toolchain::resolution_key(toolchain, env);
    if let Some(key) = &key
        && let Some(held) = cache::read_toolchain(key, cache, env)
    {
        let rustfmt = held.rustfmt.clone();
        return Ok((
            Resolved {
                rustfmt,
                requested: toolchain.to_string(),
                via: toolchain::cached_via(toolchain),
                warnings: Vec::new(),
            },
            remembered(held),
        ));
    }

    let resolved = toolchain::resolve(toolchain, install)?;
    let capabilities =
        capabilities(&resolved.rustfmt).map_err(|err| Error::UnstableRustfmtRequired {
            toolchain: toolchain.to_string(),
            rustfmt: resolved.rustfmt.display().to_string(),
            details: err.to_string(),
        })?;
    // Only an answer that will not change is worth remembering. A run that
    // fell back to something other than nightly must ask again, or a nightly
    // installed afterwards would never be noticed.
    if let Some(key) = &key
        && resolved.via.is_preferred()
    {
        cache::write_toolchain(key, &capabilities.remember(), cache, env);
    }
    Ok((resolved, capabilities))
}

/// Rebuild the capability answer from what a previous run wrote down, and put
/// it in the same per-process memo a fresh probe would have filled.
fn remembered(held: cache::Toolchain) -> Arc<RustfmtCapabilities> {
    let capabilities = Arc::new(RustfmtCapabilities {
        rustfmt: held.rustfmt.clone(),
        options: held.options.into_iter().collect(),
        version: OnceLock::from(held.version),
        unstable_cli: held.unstable_cli,
    });
    CAPABILITIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(held.rustfmt, Arc::clone(&capabilities));
    capabilities
}

/// The rustfmt for a path that resolves one outside `run_format_plan`:
/// `--print-config`, `--stdin`, and a preview whose plan never needed Rust.
fn resolve_for_one_off(toolchain: &str, cache: bool) -> Result<Rustfmt> {
    let (resolved, capabilities) = resolve_toolchain(toolchain, InstallPolicy::Never, cache)?;
    Ok(Rustfmt {
        path: resolved.rustfmt,
        capabilities,
    })
}

/// What a run should say about the rustfmt it found.
///
/// A stable rustfmt formats identically -- both options this tool sets are
/// delivered through `--config`, which stable honours -- but it cannot reach
/// `--file-lines`, so a channel switch is never left unsaid.
fn toolchain_notes(resolved: &Resolved, capabilities: &RustfmtCapabilities) -> Vec<String> {
    let mut notes = resolved.warnings.clone();
    if !capabilities.unstable_cli() {
        notes.push(format!(
            "warning: {} is not a nightly rustfmt ({}), so --range is unavailable",
            resolved.via.describe(),
            capabilities.version()
        ));
    }
    notes
}

pub fn run_format(target: TargetKind, options: &FormatterOptions) -> Result<FormatResult> {
    run_format_to(target, options, &Streams::discard())
}

pub fn run_format_to(
    target: TargetKind,
    options: &FormatterOptions,
    streams: &Streams<'_>,
) -> Result<FormatResult> {
    let selector = options.selector()?;
    let mut plan = Plan {
        targets: vec![target],
        ..Plan::default()
    };
    bind_workspaces(&mut plan, options);
    run_format_plan(plan, options, &selector, streams)
}

#[expect(
    clippy::too_many_lines,
    reason = "one pass over every target, accumulating the eight lists a FormatResult \
              carries. Each is threaded through the loop, so extracting a step means \
              passing all of them to it."
)]
pub fn run_format_plan(
    mut plan: Plan,
    options: &FormatterOptions,
    selector: &Selector,
    streams: &Streams<'_>,
) -> Result<FormatResult> {
    let languages = selector.languages();
    let needs_rust = languages.rust() && plan.targets.iter().any(target_needs_rust);
    let rustfmt = if needs_rust {
        let install = InstallPolicy::interactive(options.install_toolchain);
        let (resolved, capabilities) =
            resolve_toolchain(&options.toolchain, install, options.cache)?;
        Some((resolved, capabilities))
    } else {
        None
    };

    let mut options = std::borrow::Cow::Borrowed(options);
    let mut preflight = Vec::new();
    if let Some((resolved, capabilities)) = &rustfmt {
        preflight = toolchain_notes(resolved, capabilities);
        preflight.extend(reconcile_with_rustfmt(options.to_mut(), capabilities)?);
    }
    let options = options.as_ref();
    let rustfmt = rustfmt.map(|(resolved, capabilities)| Rustfmt {
        path: resolved.rustfmt,
        capabilities,
    });
    if let Some(rustfmt) = &rustfmt
        && !rustfmt.unstable_cli()
        && !options.ranges.is_empty()
    {
        return Err(Error::NightlyRustfmtRequired {
            what: "--range".to_string(),
            toolchain: options.toolchain.clone(),
            version: rustfmt.capabilities().version().to_string(),
        });
    }

    if !options.quiet {
        for note in &preflight {
            let _ = streams.note_line(note);
        }
    }
    if options.verbose && !options.quiet {
        report_resolution(options, rustfmt.as_ref(), streams);
    }

    let lookup = options.version_lookup()?;

    if options.emit == Emit::Stdout {
        return preview_one_file(
            plan,
            options,
            selector,
            rustfmt.as_ref(),
            lookup.as_ref(),
            streams,
        );
    }

    let mut rust_code = 0;
    let mut files = 0;
    let mut changed = Vec::new();
    let mut errors = plan.errors;
    let mut findings: Vec<(PathBuf, TomlIssue)> = Vec::new();
    let mut records: Vec<(PathBuf, VersionRecord)> = Vec::new();
    let mut walk_warnings: Vec<String> = Vec::new();
    let mut targets = Vec::with_capacity(plan.targets.len());

    let scoped = scoped_targets(
        plan.targets,
        &mut plan.workspaces,
        options,
        rustfmt.as_ref(),
        streams,
    )?;
    for (target, metadata) in scoped {
        let outcome = match run_one_target(
            &target,
            metadata.as_deref(),
            options,
            selector,
            rustfmt.as_ref(),
            lookup.as_ref(),
            streams,
        ) {
            Ok(outcome) => outcome,
            // A hard failure still stops the run, but targets already formatted
            // keep their entries rather than vanishing with the error.
            Err(err) => {
                errors.push(err);
                break;
            }
        };
        if outcome.code != 0 {
            rust_code = outcome.code;
        }
        files += outcome.files;
        changed.extend(outcome.rust_changed);
        changed.extend(outcome.toml.changed);
        errors.extend(outcome.errors);
        errors.extend(outcome.toml.errors);
        findings.extend(outcome.toml.findings);
        records.extend(outcome.toml.records);
        walk_warnings.extend(outcome.warnings);
        targets.push(outcome.target);
    }

    if let Some(lookup) = &lookup
        && let Some((crate_name, details)) = lookup.take_error()
    {
        errors.push(Error::RegistryLookup {
            crate_name,
            details,
        });
    }

    changed.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    changed.dedup_by(|left, right| left.path == right.path);
    errors.sort_by_key(std::string::ToString::to_string);
    findings.sort_unstable_by(|(left, one), (right, other)| {
        left.cmp(right)
            .then((one.line, one.column).cmp(&(other.line, other.column)))
    });

    records.sort_unstable_by(|(left, one), (right, other)| {
        left.cmp(right)
            .then((one.line, one.column).cmp(&(other.line, other.column)))
    });
    let mut warnings: Vec<String> = findings.iter().map(issue_warning).collect();
    walk_warnings.sort_unstable();
    walk_warnings.dedup();
    warnings.extend(walk_warnings);
    warnings.extend(
        lookup
            .as_ref()
            .map(RegistryLookup::warnings)
            .unwrap_or_default()
            .iter()
            .cloned(),
    );
    warnings.extend(version_warnings(&records, options));

    Ok(FormatResult {
        exit_code: merge_exit(rust_code, &changed, &errors),
        targets,
        files,
        outcomes: changed,
        file_errors: errors,
        warnings,
        versions: records,
        preview: None,
    })
}

/// What `--full-versions` did and what it left alone. A skip that means the run
/// could not do what it was asked always warns; the ordinary ones are counted
/// in one line and named only under `--verbose`.
fn version_warnings(
    records: &[(PathBuf, VersionRecord)],
    options: &FormatterOptions,
) -> Vec<String> {
    if records.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut completed = 0usize;
    let mut counts: Vec<(&'static str, usize)> = Vec::new();

    for (path, record) in records {
        let at = || display_location(path, record);
        match &record.outcome {
            Resolution::Pinned(to) => {
                completed += 1;
                if options.check || options.verbose {
                    out.push(format!(
                        "version: {}: {} \"{}\" completes to \"{to}\"",
                        at(),
                        record.crate_name,
                        record.requirement
                    ));
                }
            }
            // A failure is reported as an error of its own, with the details,
            // so neither it nor an unchanged requirement is counted here.
            Resolution::Unchanged | Resolution::Skipped(SkipReason::LookupFailed) => {}
            Resolution::Skipped(reason) => {
                // A requirement that was already complete is the ordinary case
                // and not something the run "left alone".
                if *reason != SkipReason::AlreadyComplete {
                    match counts
                        .iter_mut()
                        .find(|(label, _)| *label == reason.label())
                    {
                        Some((_, count)) => *count += 1,
                        None => counts.push((reason.label(), 1)),
                    }
                }
                if reason.is_notable() {
                    out.push(format!(
                        "warning: {}: {} left alone: {reason}",
                        at(),
                        record.crate_name
                    ));
                } else if options.verbose {
                    out.push(format!(
                        "version: {}: {} left alone: {reason}",
                        at(),
                        record.crate_name
                    ));
                }
            }
        }
    }

    let skipped: usize = counts.iter().map(|(_, count)| count).sum();
    if completed == 0 && skipped == 0 {
        return out;
    }

    let breakdown = counts
        .iter()
        .map(|(label, count)| format!("{count} {label}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut line = format!("--full-versions: {completed} completed, {skipped} left alone");
    if !breakdown.is_empty() {
        let _ = write!(line, " ({breakdown})");
    }
    out.push(line);
    out
}

fn display_location(path: &Path, record: &VersionRecord) -> String {
    match (record.line, record.column) {
        (Some(line), Some(column)) => format!("{}:{line}:{column}", path.display()),
        _ => path.display().to_string(),
    }
}

fn issue_warning((path, issue): &(PathBuf, TomlIssue)) -> String {
    format!(
        "warning: {}:{}:{}: {}",
        path.display(),
        issue.line,
        issue.column,
        issue.message
    )
}

fn preview_one_file(
    mut plan: Plan,
    options: &FormatterOptions,
    selector: &Selector,
    rustfmt: Option<&Rustfmt>,
    lookup: Option<&RegistryLookup>,
    streams: &Streams<'_>,
) -> Result<FormatResult> {
    let selected = match options.targets.as_slice() {
        [only] if only.is_file() => vec![(canonicalize_path(only), preview_language(only)?)],
        _ => collect_files(&mut plan, options, selector, streams)?,
    };
    let [(path, language)] = selected.as_slice() else {
        return Err(Error::PreviewNotSingleFile {
            count: selected.len(),
        });
    };

    let metadata = match plan.targets.first() {
        Some(target) => {
            plan.workspaces.begin_stage();
            plan.workspaces.get(target, options, rustfmt, streams)?
        }
        None => None,
    };
    let metadata = metadata.as_deref();
    let mut findings: Vec<(PathBuf, TomlIssue)> = Vec::new();
    let mut records: Vec<(PathBuf, VersionRecord)> = Vec::new();
    let content = match language {
        Kind::Toml => preview_toml(path, options, lookup, metadata, &mut findings, &mut records)?,
        Kind::Rust => {
            let owned;
            let rustfmt = if let Some(rustfmt) = rustfmt {
                rustfmt
            } else {
                owned = resolve_for_one_off(&options.toolchain, options.cache)?;
                &owned
            };
            preview_rust(path, options, rustfmt, metadata)?
        }
    };

    let mut errors = plan.errors;
    if let Some(lookup) = &lookup
        && let Some((crate_name, details)) = lookup.take_error()
    {
        errors.push(Error::RegistryLookup {
            crate_name,
            details,
        });
    }

    let mut warnings: Vec<String> = findings.iter().map(issue_warning).collect();
    warnings.extend(
        lookup
            .map(RegistryLookup::warnings)
            .unwrap_or_default()
            .iter()
            .cloned(),
    );
    warnings.extend(version_warnings(&records, options));

    Ok(FormatResult {
        exit_code: merge_exit(0, &[], &errors),
        // One file was previewed, so that is what the summary names -- not the
        // cargo project a named `Cargo.toml` would otherwise stand for.
        targets: vec![TargetKind::SingleFile(path.clone())],
        files: 1,
        outcomes: Vec::new(),
        file_errors: errors,
        warnings,
        versions: records,
        preview: Some(Preview {
            path: path.clone(),
            language: *language,
            content,
        }),
    })
}

fn preview_language(path: &Path) -> Result<Kind> {
    if is_toml_path(path) {
        Ok(Kind::Toml)
    } else if is_rust_path(path) {
        Ok(Kind::Rust)
    } else {
        Err(Error::UnsupportedTarget(path.to_path_buf()))
    }
}

/// The write run's own codec, so a previewed file carries the BOM and the line
/// endings the file on disk would have kept.
fn preview_toml(
    path: &Path,
    options: &FormatterOptions,
    lookup: Option<&RegistryLookup>,
    metadata: Option<&CargoMetadata>,
    findings: &mut Vec<(PathBuf, TomlIssue)>,
    records: &mut Vec<(PathBuf, VersionRecord)>,
) -> Result<Vec<u8>> {
    let bytes = fs::read(path).map_err(|err| Error::io(path, err))?;
    let source = decode_source(&bytes, path)?;

    let version_lookup = lookup
        .filter(|_| is_cargo_manifest(path))
        .map(|lookup| lookup as &dyn VersionLookup);
    let context = match metadata {
        Some(meta) => read_workspace_rust_version(&meta.workspace_root),
        None => workspace_context_from_path(path),
    };
    let formatted = match version_lookup {
        Some(lookup) => {
            format_toml_with_versions(&source.text, &options.toml_style, lookup, &context)
        }
        None => format_toml(&source.text, &options.toml_style).map(|text| TomlFormatOutput {
            text,
            versions: Vec::new(),
        }),
    }
    .map_err(|err| Error::toml_parse(path, &source.text, &err))?;

    records.extend(
        formatted
            .versions
            .into_iter()
            .map(|record| (path.to_path_buf(), record)),
    );
    let formatted = formatted.text;

    if options.toml_style.toml_version == TomlVersion::V1_0 {
        findings.extend(
            toml_1_0_issues(&formatted)
                .into_iter()
                .map(|issue| (path.to_path_buf(), issue)),
        );
    }
    Ok(encode_toml_source(&formatted, &source, &options.toml_style))
}

fn preview_rust(
    path: &Path,
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    metadata: Option<&CargoMetadata>,
) -> Result<Vec<u8>> {
    let mut resolver = EditionResolver::new(options, metadata);
    let edition = resolver.edition(path);
    let project = resolver.project_config(path);
    let newline_style = NewlineStyle::effective(options, project.as_deref());

    let scratch;
    let config_path = if options.config.needs_config_file() {
        scratch = tempfile::TempDir::new().map_err(|err| Error::io("<config>", err))?;
        Some(crate::rustfmt_config::materialize(
            scratch.path(),
            project.as_deref(),
            options
                .config
                .config_file_options()
                .map(|(key, value)| (key.to_owned(), value.to_owned())),
        )?)
    } else {
        project
    };
    let args = BatchArgs {
        edition: edition.as_deref(),
        config_path: config_path.as_deref(),
        skip_children: true,
        newline_style,
    };

    let bytes = fs::read(path).map_err(|err| Error::io(path, err))?;
    let source = decode_source(&bytes, path)?.with_newline_style(newline_style);
    let mut text = first_pass(path, &source.text, options, rustfmt, args)?;
    for _ in 1..rustfmt_passes(options) {
        let next = normalize_formatted_rust(&rustfmt_stdin(&text, options, rustfmt, args)?);
        if next == text {
            break;
        }
        text = next;
    }
    Ok(encode_source(&text, &source))
}

/// The first pass of a preview, taken from the file rather than from the
/// buffer so that a `#![…]` inner attribute and the file's own path still
/// reach rustfmt. `--emit json` reports it as hunks and `--emit stdout` as
/// whole text; only the second is on every channel.
fn first_pass(
    path: &Path,
    source: &str,
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) -> Result<String> {
    if !rustfmt.unstable_cli() {
        let pass = rustfmt_stdout_pass(&[path], options, rustfmt, args)?;
        if let Some(err) = pass.errors.into_iter().next() {
            return Err(err);
        }
        let text = pass
            .split
            .into_texts()
            .into_iter()
            .find(|(found, _)| found == path)
            .map_or_else(|| source.to_string(), |(_, text)| text);
        return Ok(normalize_formatted_rust(&text));
    }

    let files = [path.to_path_buf()];
    let reported = rustfmt_json(&files, options, rustfmt, args)?;
    if let Some(err) = reported.errors.into_iter().next() {
        return Err(err);
    }
    let Some(file) = reported
        .files
        .into_iter()
        .find(|file| simplify_path(PathBuf::from(&file.name)) == path)
    else {
        return Ok(source.to_string());
    };
    Ok(normalize_formatted_rust(&apply_mismatches(
        source,
        &file.mismatches,
    )))
}

struct TargetOutcome {
    code: i32,
    files: usize,
    rust_changed: Vec<FileOutcome>,
    toml: TomlOutcome,
    /// Failures that still leave part of the target formatted. Reporting them
    /// here rather than as an `Err` is what keeps the record of files already
    /// rewritten on disk.
    errors: Vec<Error>,
    /// Findings from the walk that leave the run usable, such as a cargo
    /// configuration file that did not parse.
    warnings: Vec<String>,
    /// The target as it was actually formatted: a workspace-wide run reports
    /// the workspace root, not the member directory the user happened to name.
    target: TargetKind,
}

fn scoped_targets(
    targets: Vec<TargetKind>,
    cache: &mut MetadataCache,
    options: &FormatterOptions,
    rustfmt: Option<&Rustfmt>,
    streams: &Streams<'_>,
) -> Result<Vec<(TargetKind, Option<Arc<CargoMetadata>>)>> {
    cache.begin_stage();
    let mut resolved = Vec::with_capacity(targets.len());
    let mut covered: Vec<PathBuf> = Vec::new();

    for target in targets {
        let metadata = cache.get(&target, options, rustfmt, streams)?;
        if options.all
            && named_scope(&target).is_none()
            && let Some(meta) = &metadata
        {
            let root = canonicalize_path(&meta.workspace_root);
            if covered.contains(&root) {
                continue;
            }
            covered.push(root);
        }
        resolved.push((target, metadata));
    }
    Ok(resolved)
}

#[expect(
    clippy::too_many_lines,
    reason = "the two-language pipeline for one target, in order: collect TOML, plan \
              Rust, format each, merge the outcomes. The order is the meaning."
)]
fn run_one_target(
    target: &TargetKind,
    metadata: Option<&CargoMetadata>,
    options: &FormatterOptions,
    selector: &Selector,
    rustfmt: Option<&Rustfmt>,
    lookup: Option<&RegistryLookup>,
    streams: &Streams<'_>,
) -> Result<TargetOutcome> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // Phase one: find the work. The walk uses the whole thread budget, and
    // does not overlap the formatting phase, so a run never has more threads
    // alive than `-j` asked for.
    let collected = collect_for_run(
        target,
        options,
        selector,
        metadata,
        &mut errors,
        &mut warnings,
    )?;
    let toml_files = collected.toml;
    let toml_count = toml_files.len();
    let plan = plan_rust(collected.rust, target, options, rustfmt, metadata, streams)?;

    // Phase two: the registry barrier. It has to finish before anything is
    // written, because a registry that failed outright must not leave half a
    // tree rewritten with unresolved versions.
    let context = lookup
        .map(|_| workspace_context(target, metadata))
        .unwrap_or_default();
    let prewarmed = match lookup {
        Some(lookup) => {
            let warmed = prewarm_manifest_versions(&toml_files, options, lookup);
            if let Some((crate_name, details)) = lookup.take_error() {
                return Err(Error::RegistryLookup {
                    crate_name,
                    details,
                });
            }
            warmed
        }
        None => AHashMap::new(),
    };

    // Phase three: one pool for both languages. The heaviest work is queued
    // first so the tail of the run is a small job rather than a large one.
    let cache = Cache::load(
        &cache_root(target),
        options.cache,
        &crate::cargo_config::process_env,
    );
    let toml_job = TomlJob {
        options,
        lookup: lookup.map(|lookup| lookup as &dyn VersionLookup),
        context: &context,
        prewarmed: &prewarmed,
        cache: &cache,
        cache_key: toml_cache_key(options),
    };
    let probe_path_attr = plan.skip_children && metadata.is_some();
    let mut jobs: Vec<pool::Job<'_, WorkerState>> = Vec::new();
    enqueue_rust(&mut jobs, &plan, options, streams, &cache, probe_path_attr);
    for path in &toml_files {
        jobs.push(Box::new(
            move |state: &mut WorkerState, control: &pool::Control| {
                match format_toml_file(
                    path,
                    toml_job,
                    &mut state.bytes,
                    &mut state.toml,
                    &mut state.fresh,
                ) {
                    TomlFileStatus::Clean => {}
                    TomlFileStatus::Changed(file) => state.toml.changed.push(file),
                    // Stop scheduling, but keep what is already on disk: an early
                    // exit must not lose the record of files this run rewrote.
                    TomlFileStatus::Failed(err) => {
                        state.toml.errors.push(err);
                        if options.fail_fast {
                            control.stop();
                        }
                    }
                }
            },
        ));
    }

    let (states, panics) = pool::run(jobs, options.worker_threads(), WorkerState::default);
    errors.extend(
        panics
            .into_iter()
            .map(|payload| Error::thread_panicked(&*payload)),
    );

    let mut fresh = Vec::new();

    let mut rust = RustOutcome {
        files: plan.files,
        ..RustOutcome::default()
    };
    let mut toml = TomlOutcome::default();
    let mut saw_path_attr = false;
    for state in states {
        if state.rust.code != 0 {
            rust.code = state.rust.code;
        }
        rust.changed.extend(state.rust.changed);
        rust.errors.extend(state.rust.errors);
        rust.warnings.extend(state.rust.warnings);
        toml.changed.extend(state.toml.changed);
        toml.errors.extend(state.toml.errors);
        toml.findings.extend(state.toml.findings);
        toml.records.extend(state.toml.records);
        fresh.extend(state.fresh);
        saw_path_attr |= state.saw_path_attr;
    }

    let format_failed = !rust.errors.is_empty() || !toml.errors.is_empty() || !errors.is_empty();
    if saw_path_attr
        && !(options.fail_fast && format_failed)
        && let (Some(rustfmt), Some(metadata)) = (plan.rustfmt.as_ref(), metadata)
    {
        let selected = plan.selected();
        let extras = within(
            named_scope(target),
            path_attribute_modules(
                &selected, target, metadata, options, selector, rustfmt, streams,
            )?,
        );
        if !extras.is_empty() {
            let extra_plan = prepare_rust_plan(
                extras,
                plan.skip_children,
                options,
                Some(metadata),
                rustfmt,
                streams,
            )?;
            rust.files += extra_plan.files;
            let mut extra_jobs = Vec::new();
            enqueue_rust(
                &mut extra_jobs,
                &extra_plan,
                options,
                streams,
                &cache,
                false,
            );
            let (extra_states, extra_panics) =
                pool::run(extra_jobs, options.worker_threads(), WorkerState::default);
            errors.extend(
                extra_panics
                    .into_iter()
                    .map(|payload| Error::thread_panicked(&*payload)),
            );
            for state in extra_states {
                if state.rust.code != 0 {
                    rust.code = state.rust.code;
                }
                rust.changed.extend(state.rust.changed);
                rust.errors.extend(state.rust.errors);
                rust.warnings.extend(state.rust.warnings);
                fresh.extend(state.fresh);
            }
        }
    }

    cache.store(fresh);
    errors.append(&mut rust.errors);
    warnings.append(&mut rust.warnings);

    Ok(TargetOutcome {
        code: rust.code,
        files: rust.files + toml_count,
        rust_changed: rust.changed,
        toml,
        errors,
        warnings,
        target: formatted_target(target, options, metadata),
    })
}

/// Everything that can change what formatting a file produces, as one string
/// to hash.
///
/// Written by destructuring `FormatterOptions` without `..`, so a new setting
/// cannot be added without someone deciding whether it belongs here. That
/// decision is the whole safety argument for the cache: a setting that escaped
/// this key would let a stale entry hide a file that needs formatting.
fn cache_identity(options: &FormatterOptions) -> String {
    let FormatterOptions {
        // In the key: everything that decides what the formatted bytes are.
        config,
        unset_configs,
        toml_style,
        extra_args,
        edition,
        edition_fallback,
        style_edition,
        full_versions,
        upgrade,
        allow_yanked,
        ignore_rust_version,
        // Out of it: what the run *does*, which cannot change whether a file
        // is a fixed point. The same bytes are clean whether they are checked,
        // listed or rewritten, and a diff is only rendered for a file that is
        // not one.
        targets: _,
        source: _,
        selection: _,
        check: _,
        toolchain: _,
        ranges: _,
        all: _,
        restage: _,
        verbose: _,
        quiet: _,
        unset_misses: _,
        config_sources: _,
        lenient_keys: _,
        registry_url: _,
        offline: _,
        jobs: _,
        cache: _,
        fail_fast: _,
        install_toolchain: _,
        color: _,
        diff_context: _,
        emit: _,
        list: _,
        message_format: _,
        print_config: _,
        stdin: _,
        stdin_filepath: _,
    } = options;

    format!(
        "1\x1f{}\x1f{unset_configs:?}\x1f{toml_style:?}\x1f{extra_args:?}\
         \x1f{edition:?}\x1f{edition_fallback:?}\x1f{style_edition:?}\
         \x1f{full_versions:?}\x1f{upgrade:?}\x1f{allow_yanked:?}\
         \x1f{ignore_rust_version:?}",
        config.to_config_arg(),
    )
}

/// The TOML half's key: the shared identity and nothing else, because nothing
/// outside this process decides how a TOML file is formatted.
fn toml_cache_key(options: &FormatterOptions) -> u128 {
    cache::config_key(&format!("toml\x1e{}", cache_identity(options)))
}

/// The Rust half's key also carries rustfmt itself. An upgraded toolchain
/// formats differently, and a cache that did not notice would report a tree as
/// clean that the new rustfmt would rewrite.
fn rust_cache_key(options: &FormatterOptions, rustfmt: &Rustfmt, batch: &PreparedBatch) -> u128 {
    let discovered = batch
        .project_config
        .as_deref()
        .and_then(|path| fs::read(path).ok())
        .unwrap_or_default();
    let materialized = batch
        .config_path
        .as_deref()
        .and_then(|path| fs::read(path).ok())
        .unwrap_or_default();
    cache::config_key(&format!(
        "rust\x1e{}\x1e{}\x1e{}\x1e{:?}\x1e{}\x1e{:032x}\x1e{:032x}",
        cache_identity(options),
        rustfmt.path().display(),
        rustfmt.capabilities().version(),
        batch.edition,
        batch.skip_children,
        cache::blob_key(&discovered),
        cache::blob_key(&materialized),
    ))
}

/// Where a target keeps its cache. Two roots must not share one, or moving a
/// checkout would carry a stale verdict with it.
fn cache_root(target: &TargetKind) -> PathBuf {
    match target {
        TargetKind::CargoProject { root_dir, .. } | TargetKind::LooseDirectory { root_dir, .. } => {
            root_dir.clone()
        }
        TargetKind::SingleFile(path) => path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
        TargetKind::FileList { .. } => {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }
    }
}

/// What one worker accumulates. Held per thread and merged once at the end, so
/// the only shared state a job touches is the queue it came from.
#[derive(Default)]
struct WorkerState {
    /// Reused across every TOML file one worker reads.
    bytes: Vec<u8>,
    toml: TomlOutcome,
    rust: RustOutcome,
    /// Files this worker proved to be a fixed point, for the next run.
    fresh: Vec<Fingerprint>,
    saw_path_attr: bool,
}

/// `--all` formats every workspace member, so naming the member directory the
/// user typed would misreport what the run covered.
fn formatted_target(
    target: &TargetKind,
    options: &FormatterOptions,
    metadata: Option<&CargoMetadata>,
) -> TargetKind {
    match (target, metadata) {
        (TargetKind::CargoProject { manifest_path, .. }, Some(meta))
            if options.all && named_scope(target).is_none() =>
        {
            TargetKind::CargoProject {
                manifest_path: manifest_path.clone(),
                root_dir: canonicalize_path(&meta.workspace_root),
                workspace_root: canonicalize_path(&meta.workspace_root),
            }
        }
        _ => target.clone(),
    }
}

/// The directory the user actually named, when the manifest search had to walk
/// up out of it. `rust-formatter crates/a/src` must not rewrite `crates/b`;
/// `rust-formatter crates/a` names the package itself, so `--all` still decides
/// how wide that run is.
pub(crate) fn named_scope(target: &TargetKind) -> Option<&Path> {
    let TargetKind::CargoProject {
        manifest_path,
        root_dir,
        ..
    } = target
    else {
        return None;
    };
    (manifest_path.parent() != Some(root_dir.as_path())).then_some(root_dir.as_path())
}

fn within(scope: Option<&Path>, files: Vec<PathBuf>) -> Vec<PathBuf> {
    match scope {
        Some(scope) => files
            .into_iter()
            .filter(|path| path.starts_with(scope))
            .collect(),
        None => files,
    }
}

fn target_needs_rust(target: &TargetKind) -> bool {
    match target {
        TargetKind::CargoProject { .. } => true,
        TargetKind::SingleFile(path) => is_rust_path(path),
        TargetKind::LooseDirectory { files, .. } => !files.is_empty(),
        TargetKind::FileList { rust_files, .. } => !rust_files.is_empty(),
    }
}

#[derive(Default)]
struct RustOutcome {
    code: i32,
    files: usize,
    changed: Vec<FileOutcome>,
    errors: Vec<Error>,
    /// Things rustfmt said that are about no file and failed nothing.
    /// Deduplicated by the caller, because one invocation per chunk means one
    /// copy per chunk of the same sentence.
    warnings: Vec<String>,
    unconfirmed: Vec<PathBuf>,
    unsettled: Vec<PathBuf>,
}

/// Everything the Rust half decides before a worker starts: which files, at
/// which edition, against which materialized configuration, split into the
/// invocations that will be made.
///
/// Deciding it up front is what lets both languages share one queue. It also
/// means the temporary directory holding the merged configuration is owned
/// here, outliving every spawn that reads it.
struct RustPlan<'a> {
    rustfmt: Option<&'a Rustfmt>,
    batches: Vec<PreparedBatch>,
    /// Files handed to rustfmt, clean ones included.
    files: usize,
    skip_children: bool,
    _scratch: Option<tempfile::TempDir>,
}

struct PreparedBatch {
    edition: Option<String>,
    config_path: Option<PathBuf>,
    project_config: Option<PathBuf>,
    skip_children: bool,
    newline_style: NewlineStyle,
    /// Ordered heaviest first, so the queue's tail is its smallest job.
    chunks: Vec<Vec<PathBuf>>,
}

impl RustPlan<'_> {
    /// Every invocation to make, heaviest first across all batches. Edition
    /// groups interleave here rather than running one after another, which is
    /// what stops a three-edition workspace from formatting serially.
    fn work(&self) -> Vec<(&PreparedBatch, &[PathBuf])> {
        let mut work: Vec<(&PreparedBatch, &[PathBuf])> = self
            .batches
            .iter()
            .flat_map(|batch| {
                batch
                    .chunks
                    .iter()
                    .map(move |chunk| (batch, chunk.as_slice()))
            })
            .collect();
        work.sort_by_key(|(_, chunk)| std::cmp::Reverse(chunk.len()));
        work
    }

    fn selected(&self) -> Vec<PathBuf> {
        self.batches
            .iter()
            .flat_map(|batch| batch.chunks.iter())
            .flatten()
            .cloned()
            .collect()
    }
}

fn plan_rust<'a>(
    files: Vec<PathBuf>,
    target: &TargetKind,
    options: &FormatterOptions,
    rustfmt: Option<&'a Rustfmt>,
    metadata: Option<&CargoMetadata>,
    streams: &Streams<'_>,
) -> Result<RustPlan<'a>> {
    let skip_children = !matches!(target, TargetKind::SingleFile(_));
    let Some(rustfmt) = rustfmt else {
        return Ok(RustPlan {
            rustfmt: None,
            batches: Vec::new(),
            files: 0,
            skip_children,
            _scratch: None,
        });
    };
    prepare_rust_plan(files, skip_children, options, metadata, rustfmt, streams)
}

fn prepare_rust_plan<'a>(
    files: Vec<PathBuf>,
    skip_children: bool,
    options: &FormatterOptions,
    metadata: Option<&CargoMetadata>,
    rustfmt: &'a Rustfmt,
    streams: &Streams<'_>,
) -> Result<RustPlan<'a>> {
    let mut resolver = EditionResolver::new(options, metadata);
    let batches = batch_rust_files(files, options, &mut resolver);

    // Nothing is written into the project: the merged configuration lives in a
    // temporary directory that outlives every spawn the run makes and is
    // removed with it, so the zero-config promise holds.
    let scratch = options
        .config
        .needs_config_file()
        .then(tempfile::TempDir::new)
        .transpose()
        .map_err(|err| Error::io("<config>", err))?;

    let total: usize = batches.iter().map(|batch| batch.files.len()).sum();
    let mut prepared = Vec::with_capacity(batches.len());
    let mut counted = 0;
    for (index, batch) in batches.into_iter().enumerate() {
        if batch.files.is_empty() {
            continue;
        }
        let config_path = match &scratch {
            Some(dir) => {
                let group = dir.path().join(index.to_string());
                fs::create_dir_all(&group).map_err(|err| Error::io(&group, err))?;
                Some(crate::rustfmt_config::materialize(
                    &group,
                    batch.project_config.as_deref(),
                    options
                        .config
                        .config_file_options()
                        .map(|(key, value)| (key.to_owned(), value.to_owned())),
                )?)
            }
            None => None,
        };

        if options.verbose && !options.quiet {
            let _ = streams.note_line(&format!(
                "batch: {} file(s), {}",
                batch.files.len(),
                describe_edition(&batch)
            ));
        }

        counted += batch.files.len();
        let share = share_of(options.worker_threads(), batch.files.len(), total);
        prepared.push(PreparedBatch {
            edition: batch.edition,
            config_path,
            newline_style: NewlineStyle::effective(options, batch.project_config.as_deref()),
            project_config: batch.project_config,
            skip_children,
            chunks: chunk_rust_files(batch.files, options, share),
        });
    }

    Ok(RustPlan {
        rustfmt: Some(rustfmt),
        batches: prepared,
        files: counted,
        skip_children,
        _scratch: scratch,
    })
}

fn enqueue_rust<'a>(
    jobs: &mut Vec<pool::Job<'a, WorkerState>>,
    plan: &'a RustPlan<'_>,
    options: &'a FormatterOptions,
    streams: &'a Streams<'_>,
    cache: &'a Cache,
    probe_path_attr: bool,
) {
    for (batch, chunk) in plan.work() {
        let rustfmt = plan.rustfmt.as_ref().expect("a chunk implies a rustfmt");
        // A `SingleFile` target formats its module tree, so rustfmt reaches
        // files this chunk never named and a per-file verdict would be a lie.
        let key = (batch.skip_children && cache.enabled() && options.ranges.is_empty())
            .then(|| rust_cache_key(options, rustfmt, batch));
        jobs.push(Box::new(
            move |state: &mut WorkerState, control: &pool::Control| {
                let args = BatchArgs {
                    edition: batch.edition.as_deref(),
                    config_path: batch.config_path.as_deref(),
                    skip_children: batch.skip_children,
                    newline_style: batch.newline_style,
                };
                let known = split_cached(chunk, cache, key, probe_path_attr, &mut state.fresh);
                state.saw_path_attr |= known.saw_path_attr;
                if known.pending.is_empty() {
                    return;
                }
                match run_rustfmt_chunks(&known.pending, options, rustfmt, streams, args) {
                    Ok(part) => {
                        if part.code != 0 {
                            state.rust.code = part.code;
                        }
                        let failed = !part.errors.is_empty();
                        record_clean(&known, &part, key, &mut state.fresh);
                        state.rust.changed.extend(part.changed);
                        state.rust.errors.extend(part.errors);
                        state.rust.warnings.extend(part.warnings);
                        if failed && options.fail_fast {
                            control.stop();
                        }
                    }
                    Err(err) => {
                        state.rust.errors.push(err);
                        if options.fail_fast {
                            control.stop();
                        }
                    }
                }
            },
        ));
    }
}

/// What a chunk still has to do, and what it already knows about the rest.
struct Known {
    /// Files rustfmt still has to be given.
    pending: Vec<PathBuf>,
    /// The fingerprint each pending file would have if rustfmt leaves it
    /// alone, computed from the bytes already read.
    candidates: Vec<(PathBuf, Fingerprint)>,
    saw_path_attr: bool,
}

/// Drop the files a previous run already proved to be a fixed point.
///
/// The bytes have to be read to be hashed, which is the whole cost of the
/// cache: reading a file and hashing it with xxh3 is a small fraction of
/// parsing it, and on the Rust side it replaces a rustfmt process outright when
/// a whole chunk is known.
fn split_cached(
    chunk: &[PathBuf],
    cache: &Cache,
    key: Option<u128>,
    probe_path_attr: bool,
    fresh: &mut Vec<Fingerprint>,
) -> Known {
    if key.is_none() && !probe_path_attr {
        return Known {
            pending: chunk.to_vec(),
            candidates: Vec::new(),
            saw_path_attr: false,
        };
    }

    let mut known = Known {
        pending: Vec::with_capacity(chunk.len()),
        candidates: Vec::with_capacity(chunk.len()),
        saw_path_attr: false,
    };
    for path in chunk {
        // A file that cannot be read is handed to rustfmt anyway, so the
        // failure is reported the way it always was rather than by the cache.
        let Ok(bytes) = fs::read(path) else {
            known.pending.push(path.clone());
            continue;
        };
        if probe_path_attr {
            known.saw_path_attr |= mentions_path_attribute_bytes(&bytes);
        }
        let Some(key) = key else {
            known.pending.push(path.clone());
            continue;
        };
        let print = cache::fingerprint(path, &bytes, key);
        if cache.contains(print) {
            fresh.push(print);
            continue;
        }
        known.pending.push(path.clone());
        known.candidates.push((path.clone(), print));
    }
    known
}

fn record_clean(
    known: &Known,
    outcome: &RustOutcome,
    key: Option<u128>,
    fresh: &mut Vec<Fingerprint>,
) {
    let Some(key) = key else { return };
    if has_unattributed_failure(outcome) {
        return;
    }

    let unsettled: HashSet<PathBuf> = outcome
        .unsettled
        .iter()
        .map(|path| simplify_path(path.clone()))
        .collect();
    let named: HashSet<PathBuf> = outcome
        .changed
        .iter()
        .map(|file| file.path.clone())
        .chain(
            outcome
                .errors
                .iter()
                .filter_map(|err| err.diagnostic().path.map(Path::to_path_buf)),
        )
        .chain(
            outcome
                .unconfirmed
                .iter()
                .map(|path| simplify_path(path.clone())),
        )
        .chain(unsettled.iter().cloned())
        .collect();
    let known_names: HashSet<PathBuf> = known
        .pending
        .iter()
        .map(|path| simplify_path(path.clone()))
        .collect();
    if !named.iter().all(|path| known_names.contains(path)) {
        return;
    }

    let rewritten: HashSet<PathBuf> = outcome
        .changed
        .iter()
        .filter(|file| file.status == FileStatus::Formatted)
        .map(|file| file.path.clone())
        .filter(|path| !unsettled.contains(path))
        .collect();

    for (path, print) in &known.candidates {
        let name = simplify_path(path.clone());
        if rewritten.contains(&name) {
            if let Ok(bytes) = fs::read(path) {
                fresh.push(cache::fingerprint(path, &bytes, key));
            }
        } else if !named.contains(&name) {
            fresh.push(*print);
        }
    }
}

fn has_unattributed_failure(outcome: &RustOutcome) -> bool {
    outcome
        .errors
        .iter()
        .any(|err| err.diagnostic().path.is_none())
}

/// How many invocations one edition group gets out of the worker budget.
///
/// Every group used to take the whole budget and run after the one before it,
/// so a four-edition workspace made four rounds of `-j` processes where one
/// would have done. Sharing the budget by size puts every group in the same
/// queue and finishes them together -- and a group of one file still gets one
/// invocation, because zero would drop it.
fn share_of(jobs: usize, files: usize, total: usize) -> usize {
    if total == 0 {
        return 1;
    }
    ((jobs * files).div_ceil(total)).max(1)
}

/// Split one edition group into the invocations that will format it.
///
/// Files are dealt by size rather than round-robin: dealing `index % jobs` let
/// one worker draw every large file, because the sorted file list correlates
/// with directory layout and a generated module directory lands in a handful
/// of buckets. Longest-processing-time assignment into exactly as many bins as
/// there are workers balances them without paying for a single extra process,
/// which is the dominant fixed cost -- rustfmt starts in about as long as it
/// takes to format a small file.
///
/// A bin is then split again if it would overrun the argv budget or the file
/// cap, which is the only thing those limits exist for.
fn chunk_rust_files(
    files: Vec<PathBuf>,
    options: &FormatterOptions,
    bins: usize,
) -> Vec<Vec<PathBuf>> {
    let cost = |path: &Path| argv_cost(path, options.ranges.len());
    let bins = bins.min(files.len()).max(1);
    // An abort can only cancel an invocation that has not started, so
    // `--fail-fast` is worth nothing at one invocation per worker. Asking for
    // an early exit buys it with several smaller invocations instead, which is
    // a handful of extra rustfmt starts on a run that does not fail.
    let cap = if options.fail_fast {
        (files.len() / (bins * 4)).clamp(1, RUSTFMT_CHUNK_SIZE)
    } else {
        RUSTFMT_CHUNK_SIZE
    };
    if bins <= 1 {
        return split_to_budget(files, cap, &cost);
    }

    let mut files = files;
    largest_first(&mut files);

    let mut loads: Vec<(u64, Vec<PathBuf>)> = (0..bins).map(|_| (0, Vec::new())).collect();
    for path in files {
        let size = file_size(&path);
        let lightest = loads
            .iter_mut()
            .min_by_key(|(load, files)| (*load, files.len()))
            .expect("bins is at least one");
        lightest.0 += size;
        lightest.1.push(path);
    }
    loads.sort_by_key(|(load, _)| std::cmp::Reverse(*load));

    loads
        .into_iter()
        .filter(|(_, files)| !files.is_empty())
        .flat_map(|(_, files)| split_to_budget(files, cap, &cost))
        .collect()
}

/// Cut a file list where one rustfmt command line would stop holding it.
fn split_to_budget(
    files: Vec<PathBuf>,
    cap: usize,
    cost: &dyn Fn(&Path) -> usize,
) -> Vec<Vec<PathBuf>> {
    let budget = RUSTFMT_ARGV_BUDGET.saturating_sub(RUSTFMT_ARGV_RESERVE);
    let mut out = Vec::new();
    let mut chunk: Vec<PathBuf> = Vec::new();
    let mut bytes = 0;
    for path in files {
        let len = cost(&path);
        if !chunk.is_empty() && (chunk.len() >= cap || bytes + len > budget) {
            out.push(std::mem::take(&mut chunk));
            bytes = 0;
        }
        bytes += len;
        chunk.push(path);
    }
    if !chunk.is_empty() {
        out.push(chunk);
    }
    out
}

/// What one path adds to a rustfmt command line: the argument itself, plus the
/// `--file-lines` entry each requested range needs for it.
fn argv_cost(path: &Path, ranges: usize) -> usize {
    let len = path.as_os_str().len() + 1;
    len + ranges * (len + 32)
}

/// Put the largest files first, so the heaviest bin is packed first and the
/// tail of the run is its smallest job.
///
/// The sort is stable and the input is in path order, so two files of the same
/// size keep a deterministic order between runs. TOML files are deliberately
/// not sorted this way: each is a job of its own, so the queue balances itself
/// and the stat per file would cost more than the tail it shortens.
fn largest_first(files: &mut [PathBuf]) {
    files.sort_by_cached_key(|path| std::cmp::Reverse(file_size(path)));
}

/// A file whose size cannot be read is scheduled as if it were empty: it is
/// about to fail to be read again, and the failure belongs in the report
/// rather than in the scheduler.
fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |meta| meta.len())
}

/// Modules a `#[path]` attribute puts outside every directory the walk covers.
///
/// `--skip-children` is what keeps rustfmt from rewriting files the selection
/// deliberately left out, but it also means a `#[path = "../shared/x.rs"]`
/// module is never reached and `--check` calls the tree clean. Asking rustfmt to
/// follow the crate roots answers the question exactly -- it is the only thing
/// that resolves `#[path]` -- and every name it comes back with is put through
/// the same filter as a walked file, so `--exclude`, `--include`, `--since` and
/// `--files-from` still decide what is formatted.
///
/// The extra invocation reparses the crate, so it is spent only on trees that
/// actually carry the attribute.
fn path_attribute_modules(
    selected: &[PathBuf],
    target: &TargetKind,
    metadata: &CargoMetadata,
    options: &FormatterOptions,
    selector: &Selector,
    rustfmt: &Rustfmt,
    streams: &Streams<'_>,
) -> Result<Vec<PathBuf>> {
    let TargetKind::CargoProject { manifest_path, .. } = target else {
        return Ok(Vec::new());
    };

    let packages = select_packages(metadata, manifest_path, options.all);
    let roots: Vec<PathBuf> = packages
        .iter()
        .flat_map(|pkg| pkg.targets.iter())
        .map(|target| canonicalize_path(&target.src_path))
        .filter(|root| root.is_file())
        .collect();
    if roots.is_empty() {
        return Ok(Vec::new());
    }

    let known: HashSet<&Path> = selected.iter().map(PathBuf::as_path).collect();
    let target_dir = canonicalize_path(&metadata.target_directory);
    let mut resolver = EditionResolver::new(options, Some(metadata));
    let mut found = Vec::new();
    for batch in batch_rust_files(roots, options, &mut resolver) {
        let args = BatchArgs {
            edition: batch.edition.as_deref(),
            config_path: None,
            skip_children: false,
            newline_style: NewlineStyle::Auto,
        };
        for name in rustfmt_would_touch(&batch.files, options, rustfmt, args)? {
            // rustfmt names a `#[path]` module by the join, `..` and all, while
            // the walk produces canonical paths; without normalizing, a file
            // already selected would be added a second time.
            let path = canonicalize_path(&name);
            if known.contains(path.as_path())
                || found.contains(&path)
                || path.starts_with(&target_dir)
            {
                continue;
            }
            let root = path.parent().unwrap_or(Path::new("."));
            if selector.filter_for(root)?.classify(&path).is_none() {
                continue;
            }
            if !options.quiet && options.verbose {
                let _ = streams.note_line(&format!("#[path] module: {}", path.display()));
            }
            found.push(path);
        }
    }
    Ok(found)
}

/// A cheap gate on the discovery pass above. A false positive only spends that
/// parse; a match in a comment or this file's own constant must not.
fn mentions_path_attribute_bytes(bytes: &[u8]) -> bool {
    contains_slice(bytes, b"path") && bytes.split(|byte| *byte == b'\n').any(opens_path_attribute)
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((first, rest)) = needle.split_first() else {
        return true;
    };
    if rest.is_empty() {
        return haystack.contains(first);
    }
    let mut haystack = haystack;
    while let Some(at) = haystack.iter().position(|byte| *byte == *first) {
        haystack = &haystack[at..];
        if haystack.starts_with(needle) {
            return true;
        }
        haystack = &haystack[1..];
    }
    false
}

fn opens_path_attribute(line: &[u8]) -> bool {
    let line = line.trim_ascii_start();
    let Some(rest) = line.strip_prefix(b"#[") else {
        return false;
    };
    rest.trim_ascii_start()
        .strip_prefix(b"path")
        .is_some_and(|rest| rest.trim_ascii_start().starts_with(b"="))
}

/// The one place a file's edition is decided, for every path into the runner.
///
/// Two policies here cannot agree: an absent answer means "pass no `--edition`",
/// which is rustfmt's 2015 -- an edition in which `async`, `dyn` and `try` are
/// not keywords -- so a path that returns nothing for a modern file makes it
/// fail to parse rather than be formatted.
struct EditionResolver<'a> {
    /// `--edition`, or the `edition` lifted out of `--config`. Wins outright.
    explicit: Option<&'a str>,
    /// An edition a configuration source named. It describes the tree rather
    /// than this run, so it replaces the built-in default and nothing else.
    fallback: Option<&'a str>,
    /// A target's own edition, which a manifest may set per `[[bin]]`.
    by_src_path: HashMap<PathBuf, String>,
    /// `package.edition`, which `cargo metadata` reports directly.
    by_package_dir: HashMap<PathBuf, String>,
    manifests: HashMap<PathBuf, Option<String>>,
    by_file_dir: HashMap<PathBuf, Option<String>>,
    discovery: crate::rustfmt_config::Discovery,
}

impl<'a> EditionResolver<'a> {
    fn new(options: &'a FormatterOptions, metadata: Option<&CargoMetadata>) -> Self {
        let mut by_src_path = HashMap::new();
        let mut by_package_dir = HashMap::new();
        if let Some(meta) = metadata {
            for pkg in &meta.packages {
                if let Some(dir) = pkg.manifest_path.parent() {
                    by_package_dir.insert(canonicalize_path(dir), pkg.edition.clone());
                }
                for target in &pkg.targets {
                    if target.edition != pkg.edition {
                        by_src_path
                            .insert(canonicalize_path(&target.src_path), target.edition.clone());
                    }
                }
            }
        }
        Self {
            explicit: options.edition.as_deref(),
            fallback: options.edition_fallback.as_deref(),
            by_src_path,
            by_package_dir,
            manifests: HashMap::new(),
            by_file_dir: HashMap::new(),
            discovery: crate::rustfmt_config::Discovery::new(),
        }
    }

    /// `None` means "pass no `--edition`", and is reserved for the one case that
    /// wants it: the project's own `rustfmt.toml` sets one, and an inferred
    /// edition on the command line would silently override it, because rustfmt
    /// ranks the flag higher than the file.
    fn edition(&mut self, file: &Path) -> Option<String> {
        if let Some(explicit) = self.explicit {
            return Some(explicit.to_string());
        }
        if self.discovery.setting_for_file(file, "edition").is_some() {
            return None;
        }
        if let Some(edition) = self.by_src_path.get(file) {
            return Some(edition.clone());
        }
        self.owning_manifest_edition(file)
            .or_else(|| Some(self.fallback.unwrap_or(DEFAULT_EDITION).to_string()))
    }

    fn owning_manifest_edition(&mut self, file: &Path) -> Option<String> {
        let shared_by_siblings = file.parent().filter(|_| file.is_file());
        if let Some(dir) = shared_by_siblings
            && let Some(known) = self.by_file_dir.get(dir)
        {
            return known.clone();
        }

        let found = self.find_owning_manifest_edition(file);
        if let Some(dir) = shared_by_siblings {
            self.by_file_dir.insert(dir.to_path_buf(), found.clone());
        }
        found
    }

    fn find_owning_manifest_edition(&mut self, file: &Path) -> Option<String> {
        let manifest = find_cargo_manifest(file)?;
        if let Some(dir) = manifest.parent()
            && let Some(edition) = self.by_package_dir.get(&canonicalize_path(dir))
        {
            return Some(edition.clone());
        }
        self.manifests
            .entry(manifest.clone())
            .or_insert_with(|| edition_from_manifest(&manifest))
            .clone()
    }

    fn project_config(&mut self, file: &Path) -> Option<PathBuf> {
        self.discovery.for_file(file)
    }
}

/// Group files into the invocations that can share one command line. A
/// `--files-from` list or a loose tree can span packages, so one edition for the
/// whole batch would format some of them at the wrong one; and when an option
/// has to travel by `--config-path`, files that would have discovered different
/// `rustfmt.toml` files cannot share one either.
fn batch_rust_files(
    files: Vec<PathBuf>,
    _options: &FormatterOptions,
    resolver: &mut EditionResolver<'_>,
) -> Vec<RustBatch> {
    if files.is_empty() {
        return Vec::new();
    }

    let mut grouped: BTreeMap<(Option<String>, Option<PathBuf>), Vec<PathBuf>> = BTreeMap::new();
    for file in files {
        let edition = resolver.edition(&file);
        let config = resolver.project_config(&file);
        grouped.entry((edition, config)).or_default().push(file);
    }

    grouped
        .into_iter()
        .map(|((edition, project_config), mut files)| {
            files.sort_unstable();
            files.dedup();
            RustBatch {
                edition,
                project_config,
                files,
            }
        })
        .collect()
}

fn describe_edition(batch: &RustBatch) -> String {
    match &batch.edition {
        Some(edition) => format!("edition {edition}"),
        None => "edition per rustfmt.toml".to_string(),
    }
}

/// How one rustfmt invocation is asked to report itself.
///
/// Every mode that only inspects goes through `--emit json`. rustfmt's `--check`
/// exits 1 both for "these files need formatting" and for a parse failure, which
/// took a heuristic over its stderr to tell apart; `--emit json` exits 1 only for
/// the second, and names each file and hunk instead of printing a diff that is
/// not unified and cannot be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RustfmtMode {
    /// Report what differs, write nothing.
    Check,
    /// `-l`: rewrites the file and names only what it changed.
    Write,
}

impl RustfmtMode {
    fn of(options: &FormatterOptions) -> Self {
        if options.check {
            Self::Check
        } else {
            Self::Write
        }
    }
}

fn run_rustfmt_chunks(
    files: &[PathBuf],
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    streams: &Streams<'_>,
    args: BatchArgs<'_>,
) -> Result<RustOutcome> {
    let mode = RustfmtMode::of(options);
    if mode == RustfmtMode::Check && rustfmt.unstable_cli() {
        return check_chunks(files, options, rustfmt, args);
    }

    let files = with_child_modules(files, options, rustfmt, args)?;
    let args = BatchArgs {
        skip_children: true,
        ..args
    };
    match mode {
        RustfmtMode::Check => check_chunks_by_text(&files, options, rustfmt, args),
        RustfmtMode::Write => write_chunks(&files, options, rustfmt, streams, args),
    }
}

fn with_child_modules(
    files: &[PathBuf],
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) -> Result<Vec<PathBuf>> {
    let mut all = files.to_vec();
    if args.skip_children || !options.ranges.is_empty() {
        return Ok(all);
    }

    let mut seen: HashSet<PathBuf> = files.iter().map(|path| canonicalize_path(path)).collect();
    for module in rustfmt_would_touch(files, options, rustfmt, args)? {
        if seen.insert(canonicalize_path(&module)) {
            all.push(module);
        }
    }
    Ok(all)
}

fn check_chunks(
    files: &[PathBuf],
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) -> Result<RustOutcome> {
    let mut outcome = RustOutcome::default();
    for chunk in files.chunks(RUSTFMT_CHUNK_SIZE) {
        let reported = rustfmt_json(chunk, options, rustfmt, args)?;
        let mut examined: HashSet<PathBuf> = reported
            .errors
            .iter()
            .filter_map(|err| err.diagnostic().path.map(Path::to_path_buf))
            .collect();
        outcome.errors.extend(reported.errors);

        for file in reported.files {
            let path = simplify_path(PathBuf::from(&file.name));
            outcome
                .changed
                .extend(check_one(&path, &file.mismatches, options, args));
            examined.insert(path);
        }

        if args.newline_style == NewlineStyle::Auto {
            continue;
        }
        for path in chunk {
            let path = simplify_path(path.clone());
            if !examined.contains(&path) && path.is_file() {
                outcome.changed.extend(check_one(&path, &[], options, args));
            }
        }
    }
    Ok(outcome)
}

fn check_one(
    path: &Path,
    mismatches: &[RustfmtJsonMismatch],
    options: &FormatterOptions,
    args: BatchArgs<'_>,
) -> Option<FileOutcome> {
    let unreadable = || rust_outcome(path, FileStatus::NeedsFormatting, None);
    let Ok(bytes) = fs::read(path) else {
        return Some(unreadable());
    };
    let Ok(source) = decode_source(&bytes, path) else {
        return Some(unreadable());
    };

    let source = source.with_newline_style(args.newline_style);
    let formatted = normalize_formatted_rust(&apply_mismatches(&source.text, mismatches));
    if encode_source(&formatted, &source) == bytes {
        return None;
    }
    let diff = options
        .wants_diff()
        .then(|| crate::report::unified_diff(path, &source.text, &formatted, options.diff_context));
    Some(rust_outcome(path, FileStatus::NeedsFormatting, diff))
}

fn check_chunks_by_text(
    files: &[PathBuf],
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) -> Result<RustOutcome> {
    let mut outcome = RustOutcome::default();
    for chunk in files.chunks(RUSTFMT_CHUNK_SIZE) {
        let sources = read_sources(chunk, args.newline_style);
        let emitted = emit_stdout_texts(chunk, &source_texts(&sources), options, rustfmt, args)?;
        outcome.errors.extend(emitted.errors);
        outcome.warnings.extend(emitted.warnings);

        for (path, origin) in chunk.iter().zip(&sources) {
            let Some((bytes, source)) = origin else {
                continue;
            };
            let Some(after) = emitted.texts.get(path) else {
                outcome.unconfirmed.push(path.clone());
                continue;
            };
            let after = normalize_formatted_rust(after);
            if encode_source(&after, source) == *bytes {
                continue;
            }
            let shown = simplify_path(path.clone());
            let diff = options.wants_diff().then(|| {
                crate::report::unified_diff(&shown, &source.text, &after, options.diff_context)
            });
            outcome
                .changed
                .push(rust_outcome(&shown, FileStatus::NeedsFormatting, diff));
        }
    }
    Ok(outcome)
}

type SourceFile = Option<(Vec<u8>, SourceText)>;

fn read_sources(files: &[PathBuf], newline_style: NewlineStyle) -> Vec<SourceFile> {
    files
        .iter()
        .map(|path| {
            let bytes = fs::read(path).ok()?;
            decode_source(&bytes, path)
                .ok()
                .map(|source| (bytes, source.with_newline_style(newline_style)))
        })
        .collect()
}

fn source_texts(sources: &[SourceFile]) -> Vec<Option<&str>> {
    sources
        .iter()
        .map(|item| item.as_ref().map(|(_, source)| source.text.as_str()))
        .collect()
}

fn partition_for_emit_stdout<'a>(
    chunk: &'a [PathBuf],
    sources: &[Option<&str>],
) -> (Vec<&'a Path>, Vec<Vec<&'a Path>>) {
    let mut together = Vec::with_capacity(chunk.len());
    let mut alone = Vec::new();
    for (path, source) in chunk.iter().zip(sources) {
        if source.is_none_or(|text| text.contains(".rs:")) {
            alone.push(vec![path.as_path()]);
        } else {
            together.push(path.as_path());
        }
    }
    (together, alone)
}

struct EmittedTexts {
    texts: HashMap<PathBuf, String>,
    errors: Vec<Error>,
    warnings: Vec<String>,
}

impl EmittedTexts {
    fn absorb(&mut self, pass: TextPass) {
        self.errors.extend(pass.errors);
        self.warnings.extend(pass.warnings);
        self.texts.extend(pass.split.into_texts());
    }
}

fn emit_stdout_texts(
    files: &[PathBuf],
    sources: &[Option<&str>],
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) -> Result<EmittedTexts> {
    let (together, alone) = partition_for_emit_stdout(files, sources);

    let mut emitted = EmittedTexts {
        texts: HashMap::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    for group in std::iter::once(together).chain(alone) {
        if group.is_empty() {
            continue;
        }
        let pass = rustfmt_stdout_pass(&group, options, rustfmt, args)?;
        if !matches!(pass.split, Split::Ambiguous) {
            emitted.absorb(pass);
            continue;
        }
        for path in group {
            emitted.absorb(rustfmt_stdout_pass(&[path], options, rustfmt, args)?);
        }
    }
    Ok(emitted)
}

struct TextPass {
    split: Split,
    errors: Vec<Error>,
    warnings: Vec<String>,
}

enum Split {
    Texts(Vec<(PathBuf, String)>),
    Ambiguous,
}

impl Split {
    fn into_texts(self) -> Vec<(PathBuf, String)> {
        match self {
            Self::Texts(texts) => texts,
            Self::Ambiguous => Vec::new(),
        }
    }
}

fn rustfmt_stdout_pass(
    files: &[&Path],
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) -> Result<TextPass> {
    let mut cmd = Command::new(rustfmt.path());
    cmd.args(["--emit", "stdout"]);
    push_rustfmt_args(&mut cmd, options, rustfmt, args);
    let owned: Vec<PathBuf> = files.iter().map(|path| path.to_path_buf()).collect();
    push_file_lines(&mut cmd, options, &owned);
    cmd.args(files);

    let output = capture_command(cmd, "rustfmt")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (errors, warnings) = sort_diagnostics(&stderr, output.code);
    Ok(TextPass {
        split: split_emit_stdout(&stdout, files),
        errors,
        warnings,
    })
}

/// Split what rustfmt said into what failed the run and what merely happened.
///
/// A stable rustfmt refuses each unstable option a project's own
/// `rustfmt.toml` carries, says so once per invocation, and exits zero.
/// Reporting that as an error would fail a run nothing went wrong in; dropping
/// it would leave a difference in output unexplained, because a `rustfmt.toml`
/// is the one place those options do not reach rustfmt -- which is exactly why
/// this tool passes its own through `--config` instead.
fn sort_diagnostics(stderr: &str, code: i32) -> (Vec<Error>, Vec<String>) {
    let reported = rustfmt_diagnostics(stderr, code);
    if code != 0 {
        return (reported, Vec::new());
    }
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for error in reported {
        match error {
            Error::ToolFailed { details, .. } => warnings.extend(
                details
                    .lines()
                    .map(str::trim_end)
                    .filter(|line| !line.is_empty())
                    .map(|line| format!("warning: rustfmt: {line}")),
            ),
            other => errors.push(other),
        }
    }
    (errors, warnings)
}

fn split_emit_stdout(stdout: &str, files: &[&Path]) -> Split {
    let stdout = normalize_source_newlines(stdout);
    if let [only] = files {
        return Split::Texts(sole_text(&stdout, only).into_iter().collect());
    }

    let order: HashMap<String, usize> = files
        .iter()
        .enumerate()
        .flat_map(|(index, path)| {
            header_spellings(path)
                .into_iter()
                .map(move |spelling| (spelling, index))
        })
        .collect();

    let mut headers: Vec<(usize, usize, usize)> = Vec::new();
    let mut search = 0;
    while let Some(found) = stdout[search..].find(":\n\n") {
        let colon = search + found;
        let line_start = stdout[..colon].rfind('\n').map_or(0, |at| at + 1);
        if let Some(&index) = order.get(&stdout[line_start..colon]) {
            if headers
                .last()
                .is_some_and(|&(previous, _, _)| previous >= index)
            {
                return Split::Ambiguous;
            }
            headers.push((index, line_start, colon + 3));
        }
        search = colon + 3;
    }

    let opens_on_a_header = headers
        .first()
        .map_or(stdout.is_empty(), |&(_, start, _)| start == 0);
    if !opens_on_a_header {
        return Split::Ambiguous;
    }

    let texts = headers
        .iter()
        .enumerate()
        .map(|(position, &(index, _, body))| {
            let end = headers
                .get(position + 1)
                .map_or(stdout.len(), |&(_, next, _)| next);
            (files[index].to_path_buf(), stdout[body..end].to_string())
        })
        .collect();
    Split::Texts(texts)
}

fn sole_text(stdout: &str, path: &Path) -> Option<(PathBuf, String)> {
    header_spellings(path).iter().find_map(|spelling| {
        stdout
            .strip_prefix(spelling.as_str())?
            .strip_prefix(":\n\n")
            .map(|body| (path.to_path_buf(), body.to_string()))
    })
}

fn header_spellings(path: &Path) -> Vec<String> {
    let mut spellings = vec![path.display().to_string()];
    if let Ok(canonical) = fs::canonicalize(path) {
        spellings.push(canonical.display().to_string());
        spellings.push(simplify_path(canonical).display().to_string());
    }
    spellings
}

/// Rebuild what a write run would leave on disk, from the hunks `--emit json`
/// reported.
///
/// Each hunk replaces the lines of `original`, starting at `original_begin_line`.
/// The span comes from `original` itself and not from `original_end_line`,
/// because for a pure insertion rustfmt leaves the two line numbers equal while
/// `original` is empty -- reading the span off them would delete a line.
/// A leading byte-order mark goes with the rewrite: rustfmt drops it whenever it
/// writes the file, and reports no hunk for it.
fn apply_mismatches(original: &str, mismatches: &[RustfmtJsonMismatch]) -> String {
    if mismatches.is_empty() {
        return original.to_string();
    }
    let original = original.strip_prefix('\u{feff}').unwrap_or(original);
    let lines: Vec<&str> = original.split_inclusive('\n').collect();

    let mut out = String::with_capacity(original.len());
    let mut next = 0;
    for hunk in mismatches {
        let begin = hunk
            .original_begin_line
            .saturating_sub(1)
            .min(lines.len())
            .max(next);
        let end = (begin + hunk.original.split_inclusive('\n').count()).min(lines.len());
        for line in &lines[next..begin] {
            out.push_str(line);
        }
        out.push_str(&hunk.expected);
        next = end.max(next);
    }
    for line in &lines[next..] {
        out.push_str(line);
    }
    out
}

fn write_chunks(
    files: &[PathBuf],
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    streams: &Streams<'_>,
    args: BatchArgs<'_>,
) -> Result<RustOutcome> {
    let mut outcome = RustOutcome::default();
    for chunk in files.chunks(RUSTFMT_CHUNK_SIZE) {
        let part = write_chunk(chunk, options, rustfmt, args)?;
        if part.code != 0 {
            outcome.code = part.code;
        }
        outcome.errors.extend(part.errors);
        outcome.warnings.extend(part.warnings);
        outcome.unconfirmed.extend(part.unconfirmed);
        outcome.unsettled.extend(part.unsettled);
        outcome.changed.extend(part.changed);
    }
    if !options.quiet {
        for path in &outcome.unsettled {
            let _ = streams.note_line(&format!(
                "warning: rustfmt did not settle after {MAX_RUSTFMT_PASSES} passes: {}",
                path.display()
            ));
        }
    }
    Ok(outcome)
}

struct WriteChunk {
    code: i32,
    changed: Vec<FileOutcome>,
    errors: Vec<Error>,
    warnings: Vec<String>,
    unsettled: Vec<PathBuf>,
    unconfirmed: Vec<PathBuf>,
}

fn write_chunk(
    files: &[PathBuf],
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) -> Result<WriteChunk> {
    let origins = read_sources(files, args.newline_style);
    let emitted = emit_stdout_texts(files, &source_texts(&origins), options, rustfmt, args)?;

    let mut outcome = WriteChunk {
        code: i32::from(!emitted.errors.is_empty()),
        changed: Vec::new(),
        errors: emitted.errors,
        warnings: emitted.warnings,
        unsettled: Vec::new(),
        unconfirmed: Vec::new(),
    };
    for (path, origin) in files.iter().zip(&origins) {
        let Some((bytes, source)) = origin else {
            continue;
        };
        let Some(after) = emitted.texts.get(path) else {
            outcome.unconfirmed.push(path.clone());
            continue;
        };
        let written = write_formatted_rust(
            path,
            bytes,
            source,
            after,
            options,
            rustfmt,
            args,
            &mut outcome,
        );
        if let Err(err) = written {
            outcome.code = 1;
            outcome.errors.push(err);
        }
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn write_formatted_rust(
    path: &Path,
    original: &[u8],
    source: &SourceText,
    after: &str,
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
    outcome: &mut WriteChunk,
) -> Result<()> {
    let mut text = normalize_formatted_rust(after);
    let mut settled = text == source.text || rustfmt_passes(options) == 1;
    if !settled {
        for _ in 1..rustfmt_passes(options) {
            let next = normalize_formatted_rust(&rustfmt_stdin(&text, options, rustfmt, args)?);
            if next == text {
                settled = true;
                break;
            }
            text = next;
        }
    }
    if !settled {
        outcome.unsettled.push(path.to_path_buf());
    }
    let encoded = encode_source(&text, source);
    if encoded == original {
        return Ok(());
    }
    atomic_write(path, &encoded).map_err(|err| attributed_to(path, err))?;
    outcome.changed.push(rust_outcome(
        &simplify_path(path.to_path_buf()),
        FileStatus::Formatted,
        None,
    ));
    Ok(())
}

fn rustfmt_passes(options: &FormatterOptions) -> usize {
    if options.ranges.is_empty() {
        MAX_RUSTFMT_PASSES
    } else {
        1
    }
}

fn attributed_to(path: &Path, err: Error) -> Error {
    match err {
        Error::Io { source, .. } => Error::io(path, source),
        other => other,
    }
}

const NEWLINE_ONLY_MISMATCH: &str = "Incorrect newline style in ";

fn rustfmt_would_touch(
    files: &[PathBuf],
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) -> Result<Vec<PathBuf>> {
    if rustfmt.unstable_cli() {
        let reported = rustfmt_json(files, options, rustfmt, args)?;
        return Ok(reported
            .files
            .into_iter()
            .map(|file| PathBuf::from(file.name))
            .collect());
    }

    let mut cmd = Command::new(rustfmt.path());
    cmd.args(["--check", "-l"]);
    push_rustfmt_args(&mut cmd, options, rustfmt, args);
    cmd.args(files);
    let output = capture_command(cmd, "rustfmt --check -l")?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.strip_prefix(NEWLINE_ONLY_MISMATCH).unwrap_or(line))
        .map(PathBuf::from)
        .collect())
}

struct RustfmtReport {
    files: Vec<RustfmtJsonFile>,
    errors: Vec<Error>,
}

/// One `--emit json` invocation: what differs, per file, without writing.
fn rustfmt_json(
    files: &[PathBuf],
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) -> Result<RustfmtReport> {
    let mut cmd = Command::new(rustfmt.path());
    if options.quiet {
        cmd.arg("--quiet");
    }
    cmd.args(["--emit", "json"]);
    push_rustfmt_args(&mut cmd, options, rustfmt, args);
    push_file_lines(&mut cmd, options, files);
    cmd.args(files);

    let output = capture_command(cmd, "rustfmt")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut errors = rustfmt_diagnostics(&stderr, output.code);
    let parsed = match decode_rustfmt_json(&stdout, output.code) {
        Ok(files) => files,
        Err(err) => {
            errors.push(err);
            Vec::new()
        }
    };
    Ok(RustfmtReport {
        files: parsed,
        errors,
    })
}

fn push_file_lines(cmd: &mut Command, options: &FormatterOptions, files: &[PathBuf]) {
    if options.ranges.is_empty() {
        return;
    }
    cmd.arg("--file-lines").arg(file_lines_argument(
        files.iter().map(|path| path.display().to_string()),
        &options.ranges,
    ));
}

/// rustfmt's `--file-lines` payload: a JSON array of `{file, range}`, where the
/// file has to be spelled exactly as it is on the command line.
fn file_lines_argument(files: impl Iterator<Item = String>, ranges: &[LineRange]) -> String {
    let entries: Vec<serde_json::Value> = files
        .flat_map(|file| {
            ranges.iter().map(
                move |range| serde_json::json!({ "file": file, "range": [range.start, range.end] }),
            )
        })
        .collect();
    serde_json::Value::Array(entries).to_string()
}

fn rustfmt_diagnostics(stderr: &str, code: i32) -> Vec<Error> {
    let mut errors = Vec::new();
    let mut unattributed = String::new();

    for block in diagnostic_blocks(stderr) {
        let Some((headline, rest)) = block.split_once('\n') else {
            push_unattributed(&mut unattributed, block);
            continue;
        };
        let Some(location) = rest
            .lines()
            .map(str::trim_start)
            .find_map(|line| line.strip_prefix("--> "))
        else {
            push_unattributed(&mut unattributed, block);
            continue;
        };
        let Some((path, line, column)) = split_location(location.trim()) else {
            push_unattributed(&mut unattributed, block);
            continue;
        };
        // Everything but the `-->`, whose three fields are carried structurally.
        // The snippet and the `= help:` notes are what make the message worth
        // reading, and are what the TOML half keeps too.
        let mut message = headline.trim_end().to_string();
        for line in rest
            .lines()
            .filter(|line| !line.trim_start().starts_with("--> "))
        {
            message.push('\n');
            message.push_str(line.trim_end());
        }
        errors.push(Error::RustfmtDiagnostic {
            path,
            line,
            column,
            message: message.trim_end().to_string(),
        });
    }

    let unattributed = unattributed.trim();
    let failed_silently = code != 0 && errors.is_empty();
    if !unattributed.is_empty() || failed_silently {
        errors.push(Error::ToolFailed {
            command: "rustfmt".to_string(),
            code,
            details: unattributed.to_string(),
        });
    }
    errors
}

fn push_unattributed(buffer: &mut String, block: &str) {
    if !buffer.is_empty() {
        buffer.push('\n');
    }
    buffer.push_str(block.trim_end());
}

fn diagnostic_blocks(stderr: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut start = None;
    let mut offset = 0;
    for line in stderr.split_inclusive('\n') {
        if starts_diagnostic(line)
            && let Some(from) = start.replace(offset)
        {
            blocks.push(&stderr[from..offset]);
        }
        offset += line.len();
    }
    match start {
        Some(from) => blocks.push(&stderr[from..]),
        None if !stderr.trim().is_empty() => blocks.push(stderr),
        None => {}
    }
    blocks
}

/// A block runs from one of these lines to the next, so the indented snippet,
/// the `= help:` notes and the `-->` under a diagnostic stay with it.
/// `Rustfmt failed` is rustfmt's own shape for an internal failure, and is
/// listed so one that follows a diagnostic is still reported in its own right.
fn starts_diagnostic(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) {
        return false;
    }
    line.starts_with("error:")
        || line.starts_with("error[")
        || line.starts_with("warning:")
        || line
            .get(..14)
            .is_some_and(|head| head.eq_ignore_ascii_case("rustfmt failed"))
}

/// `path:line:col`, where the path may itself hold colons on the platforms that
/// allow them, so the two line numbers are taken from the right.
fn split_location(location: &str) -> Option<(PathBuf, usize, usize)> {
    let (rest, column) = location.rsplit_once(':')?;
    let (path, line) = rest.rsplit_once(':')?;
    let column = column.trim().parse().ok()?;
    let line = line.trim().parse().ok()?;
    if path.is_empty() {
        return None;
    }
    Some((simplify_path(PathBuf::from(path)), line, column))
}

fn rust_outcome(path: &Path, status: FileStatus, diff: Option<String>) -> FileOutcome {
    FileOutcome {
        path: path.to_path_buf(),
        language: Kind::Rust,
        status,
        diff,
    }
}

#[derive(Debug, Deserialize)]
struct RustfmtJsonFile {
    name: String,
    mismatches: Vec<RustfmtJsonMismatch>,
}

#[derive(Debug, Deserialize)]
struct RustfmtJsonMismatch {
    original_begin_line: usize,
    original: String,
    expected: String,
}

/// A payload that will not decode is not the same as a clean tree: `--emit json`
/// exits 0 even with mismatches, so swallowing the failure would report a run
/// that verified nothing as a success.
fn decode_rustfmt_json(stdout: &str, code: i32) -> Result<Vec<RustfmtJsonFile>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let files =
        serde_json::from_str::<Vec<RustfmtJsonFile>>(trimmed).map_err(|err| Error::ToolFailed {
            command: "rustfmt --emit json".to_string(),
            code,
            details: err.to_string(),
        })?;
    Ok(files
        .into_iter()
        .filter(|file| !file.mismatches.is_empty())
        .collect())
}

fn edition_from_manifest(manifest: &Path) -> Option<String> {
    let source = fs::read_to_string(manifest).ok()?;
    let doc = source.parse::<toml_edit::DocumentMut>().ok()?;

    if let Some(edition) = doc
        .get("package")
        .and_then(|item| item.get("edition"))
        .and_then(|item| item.as_str())
    {
        return Some(edition.to_string());
    }

    let parent = manifest.parent()?;
    let boundary = Boundary::workspace();
    for dir in bounded_ancestors(parent, &boundary).skip(1) {
        let ancestor = dir.join("Cargo.toml");
        if !ancestor.is_file() {
            continue;
        }
        let Ok(source) = fs::read_to_string(&ancestor) else {
            continue;
        };
        let Ok(doc) = source.parse::<toml_edit::DocumentMut>() else {
            continue;
        };
        if let Some(edition) = doc
            .get("workspace")
            .and_then(|item| item.get("package"))
            .and_then(|item| item.get("edition"))
            .and_then(|item| item.as_str())
        {
            return Some(edition.to_string());
        }
    }

    None
}

/// The arguments every rustfmt invocation of a run shares.
///
/// Colour is never asked for: the product this tool prints is its own unified
/// diff, painted by `report::paint_diff`, and rustfmt's escapes on *stderr* are
/// what would otherwise have to be undone before a diagnostic could be read
/// apart into a path, a line and a column.
fn push_rustfmt_args(
    cmd: &mut Command,
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) {
    if rustfmt.unstable_cli() {
        cmd.arg("--unstable-features");
    }
    if let Some(path) = args.config_path {
        cmd.arg("--config-path").arg(path);
    }
    // `--skip-children` is a nightly-only flag, but `skip_children` is an
    // ordinary option, and `--config` delivers it on either channel. Sending
    // it that way everywhere would change nothing; sending it that way only
    // where it has to keeps the nightly command line the one every earlier
    // release produced.
    let mut config = options.config.to_config_arg();
    if args.skip_children && !rustfmt.unstable_cli() {
        if !config.is_empty() {
            config.push(',');
        }
        config.push_str("skip_children=true");
    }
    if args.skip_children && rustfmt.unstable_cli() {
        cmd.arg("--skip-children");
    }
    if !config.is_empty() {
        cmd.arg("--config").arg(config);
    }
    cmd.args(["--color", "never"]);

    if let Some(edition) = args.edition {
        cmd.arg("--edition").arg(edition);
    }
    if let Some(style_edition) = &options.style_edition {
        cmd.arg("--style-edition").arg(style_edition);
    }

    cmd.args(&options.extra_args);
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
    workspace_root: PathBuf,
    target_directory: PathBuf,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    id: String,
    manifest_path: PathBuf,
    /// `package.edition`, which is the answer for every file of the package.
    /// Reading `targets[0]` instead answered for one target and guessed for the
    /// rest.
    edition: String,
    targets: Vec<CargoTarget>,
}

#[derive(Debug, Deserialize)]
struct CargoTarget {
    edition: String,
    src_path: PathBuf,
}

#[derive(Debug, Default)]
struct Collected {
    rust: Vec<PathBuf>,
    toml: Vec<PathBuf>,
}

/// Every file a cargo target's run should reach, both languages, one walk.
///
/// One walk of the workspace root (or the named package) followed by the same
/// scope filter for both languages, rather than a walk per language or per
/// package: the per-package version could not see a virtual workspace root's
/// loose files at all -- a virtual manifest is not a package -- and swept
/// `[workspace] exclude` and nested non-member packages into whichever member's
/// walk happened to reach them.
fn collect_cargo_files(
    meta: &CargoMetadata,
    manifest_path: &Path,
    options: &FormatterOptions,
    selector: &Selector,
    wanted: Languages,
    errors: &mut Vec<Error>,
    warnings: &mut Vec<String>,
) -> Result<Collected> {
    let packages = select_packages(meta, manifest_path, options.all);
    if packages.is_empty() {
        return Ok(Collected::default());
    }

    let members: Vec<PathBuf> = packages
        .iter()
        .filter_map(|pkg| pkg.manifest_path.parent().map(canonicalize_path))
        .collect();
    let root = if options.all {
        canonicalize_path(&meta.workspace_root)
    } else {
        members[0].clone()
    };
    let prune = [canonicalize_path(&meta.target_directory)];
    let mut collected = take_walk(&root, selector, wanted, &prune, errors, warnings)?;
    let mut scope = WorkspaceScope::new(&root, &members);
    collected.rust.retain(|path| scope.contains(path));
    collected.toml.retain(|path| scope.contains(path));
    Ok(collected)
}

fn select_packages<'a>(
    meta: &'a CargoMetadata,
    manifest_path: &Path,
    all: bool,
) -> Vec<&'a CargoPackage> {
    if all {
        let members: HashSet<&str> = meta.workspace_members.iter().map(String::as_str).collect();
        return meta
            .packages
            .iter()
            .filter(|pkg| members.contains(pkg.id.as_str()))
            .collect();
    }

    meta.packages
        .iter()
        .filter(|pkg| same_path(&pkg.manifest_path, manifest_path))
        .collect()
}

/// Cargo's own words for "this needed the network and could not have it".
fn needs_network(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr);
    text.contains("--offline")
        || text.contains("net.offline")
        || text.contains("no matching package")
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn cargo_metadata(cargo: &Path, manifest_path: &Path, offline: bool) -> Result<CargoMetadata> {
    let run = |offline: bool| {
        let mut cmd = Command::new(cargo);
        cmd.args(["metadata", "--format-version", "1", "--no-deps", "--quiet"]);
        if offline {
            cmd.arg("--offline");
        }
        cmd.arg("--manifest-path").arg(manifest_path);
        cmd.stdin(Stdio::null()).output()
    };

    let first = run(true).map_err(|err| Error::CommandExecutionFailed {
        command: "cargo metadata".to_string(),
        source: err,
    })?;
    // Only a missing dependency is worth a second, online attempt; a manifest
    // that does not parse fails identically and a hermetic run must not reach
    // the network at all.
    let output = match (first.status.success(), offline) {
        (false, false) if needs_network(&first.stderr) => {
            run(false).map_err(|err| Error::CommandExecutionFailed {
                command: "cargo metadata".to_string(),
                source: err,
            })?
        }
        _ => first,
    };

    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        return Err(Error::ToolFailed {
            command: "cargo metadata".to_string(),
            code: output.status.code().unwrap_or(1),
            details: details.trim().to_owned(),
        });
    }

    serde_json::from_slice(&output.stdout).map_err(|err| Error::ToolFailed {
        command: "cargo metadata".to_string(),
        code: 0,
        details: err.to_string(),
    })
}

struct Captured {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Capturing rather than inheriting is what lets the tool own stdout: it can
/// then list, convert or suppress what rustfmt printed, and emit each chunk as
/// one locked write instead of letting parallel workers interleave line by line.
fn capture_command(mut cmd: Command, display_name: &str) -> Result<Captured> {
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| Error::CommandExecutionFailed {
            command: display_name.to_string(),
            source: err,
        })?;

    Ok(Captured {
        code: output.status.code().unwrap_or(1),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn collect_for_run(
    target: &TargetKind,
    options: &FormatterOptions,
    selector: &Selector,
    metadata: Option<&CargoMetadata>,
    errors: &mut Vec<Error>,
    warnings: &mut Vec<String>,
) -> Result<Collected> {
    let wanted = selector.languages();
    let scope = named_scope(target);
    match target {
        TargetKind::CargoProject { manifest_path, .. } => {
            let collected = collect_cargo_for_run(
                manifest_path,
                options,
                selector,
                metadata,
                wanted,
                scope,
                errors,
                warnings,
            )?;
            Ok(Collected {
                rust: within(scope, collected.rust),
                toml: within(scope, collected.toml),
            })
        }
        TargetKind::SingleFile(path) => {
            let mut collected = Collected::default();
            if wanted.rust() && is_rust_path(path) {
                collected.rust.push(path.clone());
            }
            if wanted.toml() && is_toml_path(path) {
                let root = path.parent().unwrap_or(Path::new("."));
                if selector.filter_for(root)?.classify(path).is_some() {
                    collected.toml.push(path.clone());
                }
            }
            Ok(collected)
        }
        TargetKind::LooseDirectory {
            files, toml_files, ..
        } => Ok(Collected {
            rust: if wanted.rust() {
                files.clone()
            } else {
                Vec::new()
            },
            toml: if wanted.toml() {
                toml_files.clone()
            } else {
                Vec::new()
            },
        }),
        TargetKind::FileList {
            rust_files,
            toml_files,
            ..
        } => Ok(Collected {
            rust: if wanted.rust() {
                rust_files.clone()
            } else {
                Vec::new()
            },
            toml: if wanted.toml() {
                toml_files.clone()
            } else {
                Vec::new()
            },
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_cargo_for_run(
    manifest_path: &Path,
    options: &FormatterOptions,
    selector: &Selector,
    metadata: Option<&CargoMetadata>,
    wanted: Languages,
    scope: Option<&Path>,
    errors: &mut Vec<Error>,
    warnings: &mut Vec<String>,
) -> Result<Collected> {
    if let Some(meta) = metadata {
        return collect_cargo_files(
            meta,
            manifest_path,
            options,
            selector,
            wanted,
            errors,
            warnings,
        );
    }
    if options.all && scope.is_none() {
        return collect_workspace_fallback(manifest_path, selector, wanted, errors, warnings);
    }
    let dir = scope.unwrap_or_else(|| manifest_path.parent().unwrap_or(Path::new(".")));
    take_walk(dir, selector, wanted, &[], errors, warnings)
}

fn collect_workspace_fallback(
    manifest_path: &Path,
    selector: &Selector,
    wanted: Languages,
    errors: &mut Vec<Error>,
    warnings: &mut Vec<String>,
) -> Result<Collected> {
    let root = workspace_root(manifest_path);
    let members = workspace_member_dirs(&root);
    let mut collected = take_walk(&root, selector, wanted, &[], errors, warnings)?;
    let mut scope = WorkspaceScope::new(&root, &members);
    collected.rust.retain(|path| scope.contains(path));
    collected.toml.retain(|path| scope.contains(path));
    Ok(collected)
}

fn take_walk(
    root: &Path,
    selector: &Selector,
    wanted: Languages,
    prune: &[PathBuf],
    errors: &mut Vec<Error>,
    warnings: &mut Vec<String>,
) -> Result<Collected> {
    let walk = collect(root, selector, wanted, prune, false)?;
    errors.extend(walk.errors);
    warnings.extend(walk.warnings);
    Ok(Collected {
        rust: walk.rust_files,
        toml: walk.toml_files,
    })
}

fn canonicalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).map_or_else(|_| path.to_path_buf(), simplify_path)
}

fn package_is_standalone(manifest_path: &Path) -> bool {
    if manifest_defines_workspace(manifest_path) {
        return false;
    }
    let pkg_dir = manifest_path.parent().unwrap_or(Path::new("."));
    workspace_root(manifest_path) == pkg_dir
}

fn manifest_defines_workspace(manifest_path: &Path) -> bool {
    let Ok(source) = fs::read_to_string(manifest_path) else {
        return false;
    };
    let Ok(doc) = source.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    doc.get("workspace")
        .and_then(toml_edit::Item::as_table)
        .is_some()
}

struct PrewarmedManifest {
    bytes: Vec<u8>,
    source: SourceText,
}

impl PrewarmedManifest {
    fn source_if_unchanged(&self, on_disk: &[u8]) -> Option<&SourceText> {
        (self.bytes == on_disk).then_some(&self.source)
    }
}

fn prewarm_manifest_versions(
    files: &[PathBuf],
    options: &FormatterOptions,
    lookup: &dyn VersionLookup,
) -> AHashMap<PathBuf, PrewarmedManifest> {
    let manifests: Vec<&PathBuf> = files
        .iter()
        .filter(|path| is_cargo_manifest(path))
        .collect();
    if manifests.is_empty() {
        lookup.prewarm(&[]);
        return AHashMap::new();
    }

    let jobs: Vec<pool::Job<'_, Vec<(PathBuf, PrewarmedManifest)>>> = manifests
        .into_iter()
        .map(|path| {
            Box::new(
                move |state: &mut Vec<(PathBuf, PrewarmedManifest)>, _: &pool::Control| {
                    let Ok(bytes) = fs::read(path) else { return };
                    let Ok(source) = decode_source(&bytes, path) else {
                        return;
                    };
                    state.push((path.clone(), PrewarmedManifest { bytes, source }));
                },
            ) as pool::Job<'_, Vec<(PathBuf, PrewarmedManifest)>>
        })
        .collect();
    let (states, _) = pool::run(jobs, options.worker_threads(), Vec::new);

    let read: AHashMap<PathBuf, PrewarmedManifest> = states.into_iter().flatten().collect();
    let mut owned = Vec::new();
    for warmed in read.values() {
        if let Ok(requests) = owned_dep_requests(&warmed.source.text, &options.toml_style) {
            owned.extend(requests);
        }
    }
    let refs: Vec<DepRequest<'_>> = owned
        .iter()
        .map(|(name, req)| DepRequest {
            name: name.as_str(),
            req: req.as_str(),
        })
        .collect();
    lookup.prewarm(&refs);
    read
}

/// The workspace `rust-version` a member manifest inherits, which is not in the
/// member's own file.
fn workspace_context(target: &TargetKind, metadata: Option<&CargoMetadata>) -> ManifestContext {
    match (target, metadata) {
        (_, Some(meta)) => read_workspace_rust_version(&meta.workspace_root),
        (TargetKind::CargoProject { manifest_path, .. }, None) => {
            read_workspace_rust_version(&workspace_root(manifest_path))
        }
        (TargetKind::SingleFile(path), None) => workspace_context_from_path(path),
        _ => ManifestContext::default(),
    }
}

fn workspace_context_from_path(path: &Path) -> ManifestContext {
    find_cargo_manifest(path)
        .map(|manifest| read_workspace_rust_version(&workspace_root(&manifest)))
        .unwrap_or_default()
}

fn read_workspace_rust_version(root: &Path) -> ManifestContext {
    let Ok(source) = fs::read_to_string(root.join("Cargo.toml")) else {
        return ManifestContext::default();
    };
    let Ok(doc) = source.parse::<toml_edit::DocumentMut>() else {
        return ManifestContext::default();
    };
    ManifestContext {
        workspace_rust_version: doc
            .get("workspace")
            .and_then(|workspace| workspace.get("package"))
            .and_then(|package| package.get("rust-version"))
            .and_then(toml_edit::Item::as_str)
            .and_then(PartialVersion::parse),
    }
}

/// What every TOML file in a run shares, so the per-file calls carry a path and
/// their own scratch rather than six repeated parameters.
#[derive(Clone, Copy)]
struct TomlJob<'a> {
    options: &'a FormatterOptions,
    lookup: Option<&'a dyn VersionLookup>,
    context: &'a ManifestContext,
    prewarmed: &'a AHashMap<PathBuf, PrewarmedManifest>,
    cache: &'a Cache,
    cache_key: u128,
}

fn format_toml_file(
    path: &Path,
    job: TomlJob<'_>,
    bytes: &mut Vec<u8>,
    notes: &mut TomlOutcome,
    fresh: &mut Vec<Fingerprint>,
) -> TomlFileStatus {
    match read_and_format_toml(path, job, bytes, notes, fresh) {
        Ok(status) => status,
        Err(err) => TomlFileStatus::Failed(err),
    }
}

fn read_and_format_toml(
    path: &Path,
    job: TomlJob<'_>,
    bytes: &mut Vec<u8>,
    notes: &mut TomlOutcome,
    fresh: &mut Vec<Fingerprint>,
) -> Result<TomlFileStatus> {
    let options = job.options;

    // A manifest whose versions are being resolved is not cacheable: its
    // `VersionRecord`s are output the run has to produce, and they come from
    // the network rather than from the file.
    let cacheable = job.cache.enabled() && !(job.lookup.is_some() && is_cargo_manifest(path));
    let unchanged_prewarm = job.prewarmed.get(path).and_then(|warmed| {
        let on_disk = fs::read(path).ok()?;
        warmed.source_if_unchanged(&on_disk).cloned()
    });
    let source = if let Some(source) = unchanged_prewarm {
        source
    } else {
        bytes.clear();
        {
            // The handle has to be closed before `atomic_write` renames
            // over this path: Windows refuses MoveFileEx with
            // ACCESS_DENIED while another handle is open, and `File::open`
            // does not grant FILE_SHARE_DELETE.
            let mut file = File::open(path).map_err(|err| Error::io(path, err))?;
            file.read_to_end(bytes)
                .map_err(|err| Error::io(path, err))?;
        }
        if cacheable {
            let print = cache::fingerprint(path, bytes, job.cache_key);
            if job.cache.contains(print) {
                fresh.push(print);
                return Ok(TomlFileStatus::Clean);
            }
        }
        decode_source(bytes, path)?
    };

    let version_lookup = job.lookup.filter(|_| is_cargo_manifest(path));
    let formatted = match version_lookup {
        Some(lookup) => {
            format_toml_with_versions(&source.text, &options.toml_style, lookup, job.context)
        }
        None => format_toml(&source.text, &options.toml_style).map(|text| TomlFormatOutput {
            text,
            versions: Vec::new(),
        }),
    };
    let formatted = match formatted {
        Ok(formatted) => formatted,
        Err(err) => {
            return Ok(TomlFileStatus::Failed(Error::toml_parse(
                path,
                &source.text,
                &err,
            )));
        }
    };
    notes.records.extend(
        formatted
            .versions
            .into_iter()
            .map(|record| (path.to_path_buf(), record)),
    );
    let formatted = formatted.text;

    let findings_before = notes.findings.len();
    if options.toml_style.toml_version == TomlVersion::V1_0 {
        notes.findings.extend(
            toml_1_0_issues(&formatted)
                .into_iter()
                .map(|issue| (path.to_path_buf(), issue)),
        );
    }
    // A lint finding is output the run has to produce, and a hit skips the
    // parse that produces it. Only a file that said nothing is remembered.
    let quiet = notes.findings.len() == findings_before;

    if formatted == source.text {
        if cacheable && quiet {
            fresh.push(cache::fingerprint(path, bytes, job.cache_key));
        }
        return Ok(TomlFileStatus::Clean);
    }

    if options.check {
        return Ok(TomlFileStatus::Changed(FileOutcome {
            path: path.to_path_buf(),
            language: Kind::Toml,
            status: FileStatus::NeedsFormatting,
            diff: options.wants_diff().then(|| {
                crate::report::unified_diff(path, &source.text, &formatted, options.diff_context)
            }),
        }));
    }

    let encoded = encode_toml_source(&formatted, &source, &options.toml_style);
    atomic_write(path, &encoded)?;
    // What is on disk now is a fixed point, so a check run straight after a
    // write run is warm rather than paying for the whole tree again.
    if cacheable && quiet {
        fresh.push(cache::fingerprint(path, &encoded, job.cache_key));
    }
    Ok(TomlFileStatus::Changed(FileOutcome {
        path: path.to_path_buf(),
        language: Kind::Toml,
        status: FileStatus::Formatted,
        diff: None,
    }))
}

pub(crate) fn decode_source(bytes: &[u8], path: &Path) -> Result<SourceText> {
    let (bom, rest) = match bytes.strip_prefix(UTF8_BOM) {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };
    let text = std::str::from_utf8(rest).map_err(|_| {
        Error::io(
            path,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            ),
        )
    })?;
    let crlf = first_newline_is_crlf(text);
    let normalized = normalize_source_newlines(text);
    let mixed_endings_original = (text.contains('\r')
        && encode_newlines(&normalized, crlf) != text)
        .then(|| text.to_owned());
    Ok(SourceText {
        bom,
        crlf,
        text: normalized.into_owned(),
        mixed_endings_original,
    })
}

fn first_newline_is_crlf(text: &str) -> bool {
    let bytes = text.as_bytes();
    match bytes
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r')
    {
        Some(index) => bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n'),
        None => false,
    }
}

fn normalize_source_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(cr) = rest.find('\r') {
        out.push_str(&rest[..cr]);
        out.push('\n');
        rest = &rest[cr + 1..];
        rest = rest.strip_prefix('\n').unwrap_or(rest);
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

fn normalize_formatted_rust(text: &str) -> String {
    normalize_source_newlines(text.strip_prefix('\u{feff}').unwrap_or(text)).into_owned()
}

fn encode_newlines(text: &str, crlf: bool) -> std::borrow::Cow<'_, str> {
    if crlf {
        std::borrow::Cow::Owned(text.replace('\n', "\r\n"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

pub(crate) fn encode_source(formatted: &str, source: &SourceText) -> Vec<u8> {
    with_bom(&encode_newlines(formatted, source.crlf), source.bom)
}

fn with_bom(body: &str, bom: bool) -> Vec<u8> {
    if !bom {
        return body.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(UTF8_BOM.len() + body.len());
    out.extend_from_slice(UTF8_BOM);
    out.extend_from_slice(body.as_bytes());
    out
}

pub(crate) fn encode_toml_source(
    formatted: &str,
    source: &SourceText,
    style: &TomlStyle,
) -> Vec<u8> {
    let Some(original) = source
        .mixed_endings_original
        .as_deref()
        .filter(|_| style.directives)
    else {
        return encode_source(formatted, source);
    };

    let written = Regions::scan(original);
    let placed = Regions::scan(formatted);
    let mut body = String::with_capacity(formatted.len() + formatted.len() / 16);
    let mut cursor = 0;
    for (placed, written) in placed.spans().iter().zip(written.spans()) {
        let frozen = &original[written.clone()];
        if normalize_source_newlines(frozen) != formatted[placed.clone()] {
            continue;
        }
        body.push_str(&encode_newlines(
            &formatted[cursor..placed.start],
            source.crlf,
        ));
        body.push_str(frozen);
        cursor = placed.end;
    }
    body.push_str(&encode_newlines(&formatted[cursor..], source.crlf));
    with_bom(&body, source.bom)
}

pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    // Renaming over a symlink would replace the link with a regular file and
    // leave the real file holding the old bytes, so resolve it first. The
    // temporary file has to land in the resolved directory too, or the rename
    // crosses a filesystem.
    let dest = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = dest.as_path();
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(dir).map_err(|err| Error::io(dir, err))?;
    tmp.write_all(contents)
        .map_err(|err| Error::io(tmp.path(), err))?;
    tmp.flush().map_err(|err| Error::io(tmp.path(), err))?;
    tmp.as_file()
        .sync_data()
        .map_err(|err| Error::io(tmp.path(), err))?;
    if let Ok(meta) = fs::metadata(path) {
        let _ = tmp.as_file().set_permissions(meta.permissions());
    }
    tmp.persist(path)
        .map_err(|err| Error::io(path, err.error))?;
    Ok(())
}

/// The edition of the manifest owning `path`, for a buffer that has no file of
/// its own to be found from.
pub fn edition_for_path(path: &Path) -> Option<String> {
    find_cargo_manifest(path).and_then(|manifest| edition_from_manifest(&manifest))
}

/// The edition a `--stdin` buffer is formatted at.
///
/// [`EditionResolver`] answers this for a file, but it needs `cargo metadata`
/// and a target, and a buffer has neither. This is the same ladder without
/// them: the project's own `rustfmt.toml`, then the owning manifest, then a
/// configuration source, then the default -- and, as there, `None` means "pass
/// no `--edition`" and is reserved for the one case that wants it.
///
/// It has to end in a real edition, because passing nothing is rustfmt's 2015,
/// where `async`, `dyn` and `try` are not keywords. An editor that cannot hand
/// over the buffer's path -- rust-analyzer's `rustfmt.overrideCommand`
/// substitutes nothing -- would otherwise have every modern buffer refused.
pub fn stdin_edition(named: Option<&Path>, options: &FormatterOptions) -> Option<String> {
    if let Some(explicit) = &options.edition {
        return Some(explicit.clone());
    }
    // With no path at all, the working directory is what the buffer belongs to:
    // both rust-analyzer and Helix run the formatter there.
    let probe = named.map_or_else(
        || crate::detector::absolutize(Path::new("<stdin>.rs")),
        Path::to_path_buf,
    );
    let mut discovery = crate::rustfmt_config::Discovery::new();
    if discovery.setting_for_file(&probe, "edition").is_some() {
        return None;
    }
    if let Some(edition) = edition_for_path(&probe) {
        return Some(edition);
    }
    Some(
        options
            .edition_fallback
            .as_deref()
            .unwrap_or(DEFAULT_EDITION)
            .to_string(),
    )
}

/// What `-v` cannot know before the run: the toolchain actually resolved, the
/// config actually sent, and how the work will be split.
/// Check what the run is about to ask rustfmt for against what this rustfmt
/// actually has, before a single file is formatted.
///
/// An option the tool sets by default that this rustfmt does not have is dropped
/// with a warning -- passing it fails the whole invocation, which would be a
/// worse answer than formatting without it. An option a configuration source
/// named is dropped the same way: a repository's own file is read by everyone
/// who checks it out, on whatever rustfmt they have. An option the *caller*
/// named is an error, because a silently ignored `--config` or a
/// `--unset-config` that hit nothing is how a typo goes unnoticed.
fn reconcile_with_rustfmt(
    options: &mut FormatterOptions,
    capabilities: &RustfmtCapabilities,
) -> Result<Vec<String>> {
    let mut notes = Vec::new();

    for key in &options.unset_configs {
        if !capabilities.has(key) {
            if !options.lenient_keys.contains(key) {
                return Err(Error::UnknownRustfmtOption {
                    option: key.clone(),
                    version: capabilities.version().to_string(),
                });
            }
            notes.push(format!(
                "warning: {} has no option `{key}`; ignoring the request to unset it",
                capabilities.version()
            ));
        }
    }
    for key in options.unset_misses.clone() {
        notes.push(format!(
            "warning: --unset-config {key} had nothing to drop; this run never set it"
        ));
    }

    let unknown: Vec<String> = options
        .config
        .options()
        .map(|(key, _)| key.to_owned())
        .filter(|key| !capabilities.has(key))
        .collect();
    for key in unknown {
        let droppable = options.lenient_keys.contains(&key)
            || crate::config::DEFAULT_CONFIGS
                .iter()
                .any(|&(default, _)| default == key);
        if !droppable {
            return Err(Error::UnknownRustfmtOption {
                option: key,
                version: capabilities.version().to_string(),
            });
        }
        notes.push(format!(
            "warning: {} has no option `{key}`; formatting without it",
            capabilities.version()
        ));
        options.config.unset(&key);
    }

    Ok(notes)
}

/// What `-v` cannot know before the run: the toolchain actually resolved, the
/// config actually sent, and how the work will be split.
fn report_resolution(options: &FormatterOptions, rustfmt: Option<&Rustfmt>, streams: &Streams<'_>) {
    let _ = streams.note_line(&format!(
        "rustfmt: {}",
        rustfmt.map_or_else(
            || "not needed".to_string(),
            |found| found.path().display().to_string()
        )
    ));
    if let Some(found) = rustfmt {
        let _ = streams.note_line(&format!("version: {}", found.capabilities().version()));
    }
    if !options.config_sources.is_empty() {
        let _ = streams.note_line(&format!(
            "settings from: {}",
            options.config_sources.join(", ")
        ));
    }
    let config = options.config.to_config_arg();
    let _ = streams.note_line(&format!(
        "config: {}",
        if config.is_empty() { "none" } else { &config }
    ));
    let file_config: Vec<String> = options
        .config
        .config_file_options()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if !file_config.is_empty() {
        let _ = streams.note_line(&format!("config file: {}", file_config.join(", ")));
    }
    let _ = streams.note_line(&format!("jobs: {}", options.worker_threads()));
    let _ = streams.note_line(&format!(
        "edition: {}",
        options
            .edition
            .as_deref()
            .or(options.edition_fallback.as_deref())
            .unwrap_or("per manifest")
    ));
    if let Some(style_edition) = &options.style_edition {
        let _ = streams.note_line(&format!("style edition: {style_edition}"));
    }
    if !options.ranges.is_empty() {
        let ranges: Vec<String> = options
            .ranges
            .iter()
            .map(|range| format!("{}-{}", range.start, range.end))
            .collect();
        let _ = streams.note_line(&format!("ranges: {}", ranges.join(", ")));
    }
    let _ = streams.note_line(&format!("strategy: {:?}", RustfmtMode::of(options)));
    let _ = streams.note_line(&format!("color: {}", streams.product_color()));
}

pub fn collect_files(
    plan: &mut Plan,
    options: &FormatterOptions,
    selector: &Selector,
    streams: &Streams<'_>,
) -> Result<Vec<(PathBuf, Kind)>> {
    let mut found: Vec<(PathBuf, Kind)> = Vec::new();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    plan.workspaces.begin_stage();

    for target in &plan.targets {
        let metadata = plan.workspaces.get(target, options, None, streams)?;
        let collected = collect_for_run(
            target,
            options,
            selector,
            metadata.as_deref(),
            &mut errors,
            &mut warnings,
        )?;
        found.extend(collected.rust.into_iter().map(|path| (path, Kind::Rust)));
        found.extend(collected.toml.into_iter().map(|path| (path, Kind::Toml)));
    }
    // A listing that silently omitted an unreadable subtree would describe a
    // run that reaches more than it says.
    if let Some(err) = errors.into_iter().next() {
        return Err(err);
    }

    found.sort_unstable();
    found.dedup();
    Ok(found)
}

enum MetadataAnswer {
    Known(Option<Arc<CargoMetadata>>),
    Unasked,
}

#[derive(Debug, Default)]
pub(crate) struct MetadataCache {
    by_manifest: HashMap<PathBuf, Option<Arc<CargoMetadata>>>,
    loaded: Vec<Arc<CargoMetadata>>,
    cargo: Option<PathBuf>,
    cargo_resolved: bool,
}

impl MetadataCache {
    fn get(
        &mut self,
        target: &TargetKind,
        options: &FormatterOptions,
        rustfmt: Option<&Rustfmt>,
        streams: &Streams<'_>,
    ) -> Result<Option<Arc<CargoMetadata>>> {
        let TargetKind::CargoProject { manifest_path, .. } = target else {
            return Ok(None);
        };
        if let MetadataAnswer::Known(known) = self.known(manifest_path) {
            return Ok(known);
        }
        let Some(cargo) = self.cargo_bin(rustfmt, options, streams) else {
            return Ok(None);
        };
        let cargo = cargo.to_path_buf();
        load_metadata(self, manifest_path, |manifest| {
            match cargo_metadata(&cargo, manifest, options.offline) {
                Ok(meta) => Ok(Some(meta)),
                Err(_) if package_is_standalone(manifest) => Ok(None),
                Err(err) => Err(err),
            }
        })
    }

    fn cargo_bin(
        &mut self,
        rustfmt: Option<&Rustfmt>,
        options: &FormatterOptions,
        streams: &Streams<'_>,
    ) -> Option<&Path> {
        if !self.cargo_resolved {
            self.cargo_resolved = true;
            self.cargo =
                toolchain::cargo_for_metadata(rustfmt.map(Rustfmt::path), &options.toolchain);
            if self.cargo.is_none() && !options.quiet {
                let _ = streams.note_line(
                    "warning: cargo was not found, so workspace members and the build \
                     directory are unresolved",
                );
            }
        }
        self.cargo.as_deref()
    }

    fn known(&mut self, manifest: &Path) -> MetadataAnswer {
        let key = canonicalize_path(manifest);
        if let Some(cached) = self.by_manifest.get(&key) {
            return MetadataAnswer::Known(cached.clone());
        }
        let Some(hit) = self
            .loaded
            .iter()
            .find(|meta| metadata_covers(meta, &key))
            .cloned()
        else {
            return MetadataAnswer::Unasked;
        };
        self.by_manifest.insert(key, Some(Arc::clone(&hit)));
        MetadataAnswer::Known(Some(hit))
    }

    fn begin_stage(&mut self) {
        self.by_manifest.retain(|_, found| found.is_some());
        self.cargo = None;
        self.cargo_resolved = false;
    }
}

/// Overlay cargo's workspace root onto every cargo target and collapse again.
/// A metadata failure keeps the parser root so `--watch` can still start.
pub(crate) fn bind_workspaces(plan: &mut Plan, options: &FormatterOptions) {
    let streams = Streams::discard();
    plan.workspaces.begin_stage();
    for target in &mut plan.targets {
        let Ok(metadata) = plan.workspaces.get(target, options, None, &streams) else {
            continue;
        };
        let Some(meta) = metadata else {
            continue;
        };
        let TargetKind::CargoProject { workspace_root, .. } = target else {
            continue;
        };
        *workspace_root = canonicalize_path(&meta.workspace_root);
    }
    let targets = std::mem::take(&mut plan.targets);
    plan.targets = selection::collapse_targets(targets, options.all);
}

fn load_metadata(
    cache: &mut MetadataCache,
    manifest: &Path,
    load: impl FnOnce(&Path) -> Result<Option<CargoMetadata>>,
) -> Result<Option<Arc<CargoMetadata>>> {
    if let MetadataAnswer::Known(known) = cache.known(manifest) {
        return Ok(known);
    }
    let result = load(manifest)?.map(Arc::new);
    if let Some(meta) = &result {
        cache.loaded.push(Arc::clone(meta));
    }
    cache
        .by_manifest
        .insert(canonicalize_path(manifest), result.clone());
    Ok(result)
}

fn metadata_covers(meta: &CargoMetadata, manifest: &Path) -> bool {
    same_path(&meta.workspace_root.join("Cargo.toml"), manifest)
        || meta
            .packages
            .iter()
            .any(|pkg| same_path(&pkg.manifest_path, manifest))
}

/// Ask rustfmt for the configuration it will actually apply, rather than
/// echoing back the overrides this process passed in.
pub fn print_config(options: &FormatterOptions, probe: Option<&Path>) -> Result<String> {
    let rustfmt = resolve_for_one_off(&options.toolchain, options.cache)?;

    let scratch;
    let probe_path = match probe {
        Some(path) if is_rust_path(path) => path.to_path_buf(),
        _ => {
            scratch = NamedTempFile::with_suffix(".rs")
                .map_err(|err| Error::io("<print-config>", err))?;
            scratch.path().to_path_buf()
        }
    };

    // Whatever the run would pass, `--config-path` included, or the report would
    // describe a run that is not the one about to happen.
    let config_scratch;
    let config_path = if options.config.needs_config_file() {
        config_scratch = tempfile::TempDir::new().map_err(|err| Error::io("<config>", err))?;
        Some(crate::rustfmt_config::materialize(
            config_scratch.path(),
            crate::rustfmt_config::discover(probe_path.parent().unwrap_or(Path::new(".")))
                .as_deref(),
            options
                .config
                .config_file_options()
                .map(|(key, value)| (key.to_owned(), value.to_owned())),
        )?)
    } else {
        None
    };

    let mut cmd = Command::new(rustfmt.path());
    cmd.arg("--print-config").arg("current").arg(&probe_path);
    push_rustfmt_args(
        &mut cmd,
        options,
        &rustfmt,
        BatchArgs {
            edition: options.edition.as_deref(),
            config_path: config_path.as_deref(),
            skip_children: false,
            newline_style: NewlineStyle::Auto,
        },
    );

    let output = capture_command(cmd, "rustfmt --print-config")?;
    if output.code != 0 {
        let details = String::from_utf8_lossy(&output.stderr);
        return Err(Error::ToolFailed {
            command: "rustfmt --print-config".to_string(),
            code: output.code,
            details: details.trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One buffer formatted without touching the filesystem.
pub struct StdinOutput {
    pub text: String,
    pub versions: Vec<(PathBuf, VersionRecord)>,
    pub warnings: Vec<String>,
}

pub fn format_stdin(
    source: &str,
    language: Kind,
    options: &FormatterOptions,
) -> Result<StdinOutput> {
    if language == Kind::Toml {
        // A buffer is a manifest only if the caller says so, and only a manifest
        // has dependency requirements to complete.
        let is_manifest = options
            .stdin_filepath
            .as_deref()
            .is_some_and(is_cargo_manifest);
        let lookup = if is_manifest {
            options.version_lookup()?
        } else {
            None
        };

        let context = options
            .stdin_filepath
            .as_deref()
            .map(workspace_context_from_path)
            .unwrap_or_default();
        let formatted = match &lookup {
            Some(lookup) => format_toml_with_versions(
                source,
                &options.toml_style,
                lookup as &dyn VersionLookup,
                &context,
            ),
            None => format_toml(source, &options.toml_style).map(|text| TomlFormatOutput {
                text,
                versions: Vec::new(),
            }),
        }
        .map_err(|err| Error::toml_parse("<stdin>", source, &err))?;

        if let Some(lookup) = &lookup
            && let Some((crate_name, details)) = lookup.take_error()
        {
            return Err(Error::RegistryLookup {
                crate_name,
                details,
            });
        }

        let name = options
            .stdin_filepath
            .clone()
            .unwrap_or_else(|| PathBuf::from("<stdin>"));
        let versions: Vec<(PathBuf, VersionRecord)> = formatted
            .versions
            .into_iter()
            .map(|record| (name.clone(), record))
            .collect();
        let mut warnings: Vec<String> = lookup
            .as_ref()
            .map(RegistryLookup::warnings)
            .unwrap_or_default()
            .to_vec();
        warnings.extend(version_warnings(&versions, options));

        return Ok(StdinOutput {
            text: formatted.text,
            versions,
            warnings,
        });
    }

    let rustfmt = resolve_for_one_off(&options.toolchain, options.cache)?;

    let scratch;
    let project = stdin_project(options);
    let config_path = if options.config.needs_config_file() {
        scratch = tempfile::TempDir::new().map_err(|err| Error::io("<config>", err))?;
        Some(crate::rustfmt_config::materialize(
            scratch.path(),
            project.as_deref(),
            options
                .config
                .config_file_options()
                .map(|(key, value)| (key.to_owned(), value.to_owned())),
        )?)
    } else {
        project
    };
    let args = BatchArgs {
        edition: options.edition.as_deref(),
        config_path: config_path.as_deref(),
        skip_children: false,
        newline_style: NewlineStyle::Auto,
    };

    let mut current = normalize_formatted_rust(&rustfmt_stdin(source, options, &rustfmt, args)?);
    for _ in 1..rustfmt_passes(options) {
        let next = normalize_formatted_rust(&rustfmt_stdin(&current, options, &rustfmt, args)?);
        if next == current {
            break;
        }
        current = next;
    }
    Ok(StdinOutput {
        text: current,
        versions: Vec::new(),
        warnings: Vec::new(),
    })
}

/// `stdin` is the name rustfmt gives the buffer, and the name `--file-lines`
/// has to use to reach it.
const STDIN_NAME: &str = "stdin";

fn stdin_project(options: &FormatterOptions) -> Option<PathBuf> {
    match options.stdin_filepath.as_deref() {
        Some(path) => crate::rustfmt_config::discover(path.parent().unwrap_or(Path::new("."))),
        None => std::env::current_dir()
            .ok()
            .and_then(|dir| crate::rustfmt_config::discover(&dir)),
    }
}

pub(crate) fn stdin_newline_style(options: &FormatterOptions) -> NewlineStyle {
    NewlineStyle::effective(options, stdin_project(options).as_deref())
}

fn rustfmt_stdin(
    source: &str,
    options: &FormatterOptions,
    rustfmt: &Rustfmt,
    args: BatchArgs<'_>,
) -> Result<String> {
    let mut cmd = Command::new(rustfmt.path());
    cmd.args(["--emit", "stdout"]);
    push_rustfmt_args(&mut cmd, options, rustfmt, args);
    if !options.ranges.is_empty() {
        cmd.arg("--file-lines").arg(file_lines_argument(
            std::iter::once(STDIN_NAME.to_string()),
            &options.ranges,
        ));
    }

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| Error::CommandExecutionFailed {
            command: "rustfmt".to_string(),
            source: err,
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(source.as_bytes())
            .map_err(|err| Error::io("<stdin>", err))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|err| Error::CommandExecutionFailed {
            command: "rustfmt".to_string(),
            source: err,
        })?;

    if !output.status.success() {
        let code = output.status.code().unwrap_or(1);
        let details = String::from_utf8_lossy(&output.stderr);
        // The same split a file gets, so an editor buffer reports
        // `<stdin>:line:column: message` rather than one opaque blob.
        return Err(rustfmt_diagnostics(&details, code)
            .into_iter()
            .next()
            .unwrap_or_else(|| Error::ToolFailed {
                command: "rustfmt".to_string(),
                code,
                details: details.trim().to_owned(),
            }));
    }

    String::from_utf8(output.stdout).map_err(|_| {
        Error::io(
            "<stdin>",
            io::Error::new(
                io::ErrorKind::InvalidData,
                "rustfmt did not return valid UTF-8",
            ),
        )
    })
}

fn merge_exit(rust_code: i32, changed: &[FileOutcome], errors: &[Error]) -> i32 {
    if !errors.is_empty() {
        2
    } else if rust_code != 0 {
        rust_code
    } else {
        i32::from(
            changed
                .iter()
                .any(|file| file.status == FileStatus::NeedsFormatting),
        )
    }
}

fn is_cargo_manifest(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "Cargo.toml")
}

fn parallelism() -> usize {
    thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edition_from_manifest_reads_package_edition() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("Cargo.toml");
        fs::write(
            &manifest,
            r#"[package]
name = "ed"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        assert_eq!(edition_from_manifest(&manifest).as_deref(), Some("2021"));
    }

    #[test]
    fn test_edition_from_manifest_missing_edition() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("Cargo.toml");
        fs::write(
            &manifest,
            r#"[package]
name = "ed"
version = "0.1.0"
"#,
        )
        .unwrap();
        assert_eq!(edition_from_manifest(&manifest), None);
    }

    #[test]
    fn cargo_metadata_selects_workspace_members() {
        let meta: CargoMetadata = serde_json::from_str(
            r#"{
                "packages": [
                    {"id": "foo","manifest_path": "/tmp/foo/Cargo.toml","edition": "2024",
                     "targets": [{"edition": "2024", "src_path": "/tmp/foo/src/lib.rs"}]},
                    {"id": "bar","manifest_path": "/tmp/bar/Cargo.toml","edition": "2021",
                     "targets": [{"edition": "2021", "src_path": "/tmp/bar/src/lib.rs"}]}
                ],
                "workspace_members": ["foo"],
                "workspace_root": "/tmp",
                "target_directory": "/tmp/target"
            }"#,
        )
        .unwrap();
        let all = select_packages(&meta, Path::new("/tmp/foo/Cargo.toml"), true);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "foo");
        assert_eq!(all[0].edition, "2024");

        let one = select_packages(&meta, Path::new("/tmp/bar/Cargo.toml"), false);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "bar");
    }

    fn fixture_metadata() -> CargoMetadata {
        serde_json::from_str(
            r#"{
                "packages": [
                    {"id": "foo","manifest_path": "/tmp/foo/Cargo.toml","edition": "2024",
                     "targets": [{"edition": "2024", "src_path": "/tmp/foo/src/lib.rs"}]},
                    {"id": "bar","manifest_path": "/tmp/bar/Cargo.toml","edition": "2021",
                     "targets": [{"edition": "2021", "src_path": "/tmp/bar/src/lib.rs"}]}
                ],
                "workspace_members": ["foo", "bar"],
                "workspace_root": "/tmp",
                "target_directory": "/tmp/target"
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn metadata_covers_members_and_the_virtual_root() {
        let meta = fixture_metadata();
        assert!(metadata_covers(&meta, Path::new("/tmp/foo/Cargo.toml")));
        assert!(metadata_covers(&meta, Path::new("/tmp/bar/Cargo.toml")));
        assert!(metadata_covers(&meta, Path::new("/tmp/Cargo.toml")));
        assert!(!metadata_covers(&meta, Path::new("/tmp/other/Cargo.toml")));
    }

    #[test]
    fn metadata_cache_loads_a_workspace_once() {
        let mut cache = MetadataCache::default();
        let mut loads = 0usize;
        let mut load = |manifest: &Path| {
            loads += 1;
            assert!(
                manifest.ends_with("foo/Cargo.toml")
                    || manifest.ends_with("bar/Cargo.toml")
                    || manifest.ends_with("Cargo.toml"),
                "{}",
                manifest.display()
            );
            Ok(Some(fixture_metadata()))
        };

        let first = load_metadata(&mut cache, Path::new("/tmp/foo/Cargo.toml"), &mut load)
            .unwrap()
            .unwrap();
        let second = load_metadata(&mut cache, Path::new("/tmp/bar/Cargo.toml"), &mut load)
            .unwrap()
            .unwrap();
        let root = load_metadata(&mut cache, Path::new("/tmp/Cargo.toml"), &mut load)
            .unwrap()
            .unwrap();
        assert_eq!(loads, 1);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&first, &root));

        let unrelated = load_metadata(&mut cache, Path::new("/tmp/other/Cargo.toml"), |_| {
            loads += 1;
            Ok(None)
        })
        .unwrap();
        assert!(unrelated.is_none());
        assert_eq!(loads, 2);

        let again = load_metadata(&mut cache, Path::new("/tmp/other/Cargo.toml"), |_| {
            panic!("standalone miss must stay cached")
        })
        .unwrap();
        assert!(again.is_none());
        assert_eq!(loads, 2);
    }

    #[test]
    fn a_later_stage_keeps_what_cargo_described_and_asks_again_about_the_rest() {
        let mut cache = MetadataCache::default();
        let mut loads = 0usize;

        let bound = load_metadata(&mut cache, Path::new("/tmp/foo/Cargo.toml"), |_| {
            loads += 1;
            Ok(Some(fixture_metadata()))
        })
        .unwrap()
        .unwrap();
        let standalone = load_metadata(&mut cache, Path::new("/tmp/other/Cargo.toml"), |_| {
            loads += 1;
            Ok(None)
        })
        .unwrap();

        assert!(standalone.is_none());
        assert_eq!(loads, 2);

        cache.begin_stage();
        let member = load_metadata(&mut cache, Path::new("/tmp/bar/Cargo.toml"), |_| {
            panic!("a workspace cargo already described must not be asked again")
        })
        .unwrap()
        .unwrap();
        let asked_again = load_metadata(&mut cache, Path::new("/tmp/other/Cargo.toml"), |_| {
            loads += 1;
            Ok(None)
        })
        .unwrap();

        assert!(Arc::ptr_eq(&bound, &member));
        assert!(asked_again.is_none());
        assert_eq!(loads, 3);
    }

    #[test]
    fn a_run_starts_from_the_metadata_its_plan_bound() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"bound\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(temp.path().join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();
        let options = FormatterOptions::for_path(temp.path());
        let selector = options.selector().unwrap();

        let mut plan = crate::plan(&options, &selector).unwrap();

        assert_eq!(plan.workspaces.loaded.len(), 1);

        let bound = Arc::clone(&plan.workspaces.loaded[0]);
        let scoped = scoped_targets(
            std::mem::take(&mut plan.targets),
            &mut plan.workspaces,
            &options,
            None,
            &Streams::discard(),
        )
        .unwrap();

        assert_eq!(scoped.len(), 1);
        assert!(Arc::ptr_eq(scoped[0].1.as_ref().unwrap(), &bound));
        assert_eq!(plan.workspaces.loaded.len(), 1);
    }

    #[test]
    fn path_attribute_opens_its_own_line() {
        let yes = [
            b"#[path = \"x.rs\"]".as_slice(),
            b"#[path=\"x.rs\"]",
            b"#[ path = \"x.rs\"]",
            b"  #[path = \"x.rs\"]",
            b"\t#[path = \"x.rs\"]",
        ];
        let no = [
            b"// #[path = \"x.rs\"]".as_slice(),
            b"/// #[path = \"x.rs\"]",
            b"mod x; #[path = \"x.rs\"]",
            b"let PATH_ATTRIBUTE = b\"#[path\";",
            b"#![path = \"x.rs\"]",
            b"#[cfg_attr(unix, path = \"x.rs\")]",
        ];
        for line in yes {
            assert!(
                opens_path_attribute(line),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
        for line in no {
            assert!(
                !opens_path_attribute(line),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
        assert!(mentions_path_attribute_bytes(
            b"fn f() {}\n#[path = \"x.rs\"]\nmod x;\n"
        ));
        assert!(!mentions_path_attribute_bytes(
            b"fn f() {}\n// no path here\n"
        ));
        assert!(!mentions_path_attribute_bytes(
            b"// #[path = \"x.rs\"] is documented\nfn f() {}\n"
        ));
    }

    fn batches_of(files: Vec<PathBuf>, options: &FormatterOptions) -> Vec<RustBatch> {
        let mut resolver = EditionResolver::new(options, None);
        batch_rust_files(files, options, &mut resolver)
    }

    /// A `--files-from` list can cross packages, so one edition for the whole
    /// batch would format some of the files at the wrong one.
    #[test]
    fn batching_splits_a_list_that_crosses_packages() {
        let temp = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for (name, edition) in [("old", "2015"), ("new", "2024")] {
            let dir = temp.path().join(name).join("src");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                temp.path().join(name).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nedition = \"{edition}\"\n"),
            )
            .unwrap();
            let file = dir.join("lib.rs");
            fs::write(&file, "pub fn a() {}\n").unwrap();
            files.push(file);
        }

        let batches = batches_of(files, &FormatterOptions::default());
        let editions: Vec<_> = batches.iter().map(|batch| batch.edition.clone()).collect();
        assert_eq!(
            editions,
            vec![Some("2015".to_string()), Some("2024".to_string())]
        );
        assert!(batches.iter().all(|batch| batch.files.len() == 1));
    }

    #[test]
    fn an_explicit_edition_keeps_a_list_in_one_batch() {
        let options = FormatterOptions {
            edition: Some("2021".to_string()),
            ..FormatterOptions::default()
        };
        let batches = batches_of(
            vec![PathBuf::from("/a/x.rs"), PathBuf::from("/b/y.rs")],
            &options,
        );
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].edition.as_deref(), Some("2021"));
    }

    /// rustfmt's own default is 2015, where `async` is not a keyword, so a file
    /// with no manifest above it used to fail to parse rather than be formatted.
    #[test]
    fn a_file_with_no_manifest_gets_a_modern_edition() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("script.rs");
        fs::write(&file, "async fn f() {}\n").unwrap();

        let batches = batches_of(vec![file], &FormatterOptions::default());
        assert_eq!(batches[0].edition.as_deref(), Some(DEFAULT_EDITION));
    }

    /// An inferred edition on the command line outranks the project's own
    /// `rustfmt.toml`, so the only honest thing to do is pass nothing.
    #[test]
    fn a_project_rustfmt_toml_keeps_the_last_word_on_edition() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("rustfmt.toml"), "edition = \"2018\"\n").unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let dir = temp.path().join("src");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lib.rs");
        fs::write(&file, "pub fn a() {}\n").unwrap();

        let batches = batches_of(vec![file.clone()], &FormatterOptions::default());
        assert_eq!(batches[0].edition, None);

        // An edition the caller asked for still wins over the file.
        let options = FormatterOptions {
            edition: Some("2024".to_string()),
            ..FormatterOptions::default()
        };
        assert_eq!(
            batches_of(vec![file], &options)[0].edition.as_deref(),
            Some("2024")
        );
    }

    /// `--config-path` replaces rustfmt's discovery, so two files that would
    /// discover different project files cannot share one invocation.
    #[test]
    fn a_config_file_route_splits_batches_by_project_config() {
        let temp = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for name in ["a", "b"] {
            let dir = temp.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("rustfmt.toml"), "max_width = 80\n").unwrap();
            let file = dir.join("lib.rs");
            fs::write(&file, "pub fn a() {}\n").unwrap();
            files.push(file);
        }

        let plain = batches_of(files.clone(), &FormatterOptions::default());
        assert_eq!(plain.len(), 2);
        assert!(plain.iter().all(|batch| batch.project_config.is_some()));

        let mut config = RustfmtConfig::default();
        config.extend_from_str(r#"ignore=["x","y"]"#).unwrap();
        let options = FormatterOptions {
            config,
            ..FormatterOptions::default()
        };
        let split = batches_of(files, &options);
        assert_eq!(split.len(), 2);
        assert!(split.iter().all(|batch| batch.project_config.is_some()));
    }

    #[test]
    fn worker_threads_honors_jobs_override() {
        let mut options = FormatterOptions::default();
        assert!(options.worker_threads() >= 1);
        options.jobs = NonZeroUsize::new(1);
        assert_eq!(options.worker_threads(), 1);
        options.jobs = NonZeroUsize::new(32);
        assert_eq!(options.worker_threads(), 32);
    }

    #[test]
    fn decode_preserves_bom_and_crlf() {
        let path = Path::new("mem.toml");
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(b"a = 1\r\nb = 2\r\n");
        let source = decode_source(&bytes, path).unwrap();
        assert!(source.bom);
        assert!(source.crlf);
        assert_eq!(source.text, "a = 1\nb = 2\n");
        let encoded = encode_source("a = 1\nb = 2\n", &source);
        assert_eq!(encoded, bytes);
    }

    fn mixed_source(bytes: &[u8]) -> SourceText {
        decode_source(bytes, Path::new("mixed.toml")).unwrap()
    }

    #[test]
    fn a_frozen_region_keeps_its_bytes_in_an_lf_majority_file() {
        let source = mixed_source(b"a=1\n# fmt: off\nb   =  2\r\n# fmt: on\nc=3\n");

        let encoded = encode_toml_source(
            "a = 1\n# fmt: off\nb   =  2\n# fmt: on\nc = 3\n",
            &source,
            &TomlStyle::default(),
        );

        assert_eq!(
            encoded,
            b"a = 1\n# fmt: off\nb   =  2\r\n# fmt: on\nc = 3\n"
        );
    }

    #[test]
    fn a_frozen_region_keeps_its_bytes_in_a_crlf_majority_file() {
        let source = mixed_source(b"a=1\r\n# fmt: off\r\nb   =  2\n# fmt: on\r\nc=3\r\n");

        let encoded = encode_toml_source(
            "a = 1\n# fmt: off\nb   =  2\n# fmt: on\nc = 3\n",
            &source,
            &TomlStyle::default(),
        );

        assert_eq!(
            encoded,
            b"a = 1\r\n# fmt: off\r\nb   =  2\n# fmt: on\r\nc = 3\r\n"
        );
    }

    #[test]
    fn without_directives_the_whole_file_takes_one_line_ending() {
        let source = mixed_source(b"a=1\n# fmt: off\nb   =  2\r\n# fmt: on\nc=3\n");
        let style = TomlStyle {
            directives: false,
            ..TomlStyle::default()
        };

        let encoded = encode_toml_source(
            "a = 1\n# fmt: off\nb = 2\n# fmt: on\nc = 3\n",
            &source,
            &style,
        );

        assert_eq!(encoded, b"a = 1\n# fmt: off\nb = 2\n# fmt: on\nc = 3\n");
    }

    #[test]
    fn a_uniform_file_keeps_no_second_copy() {
        assert!(
            mixed_source(b"a = 1\r\nb = 2\r\n")
                .mixed_endings_original
                .is_none()
        );
        assert!(
            mixed_source(b"a = 1\nb = 2\n")
                .mixed_endings_original
                .is_none()
        );
        assert!(
            mixed_source(b"a = 1\nb = 2\r\n")
                .mixed_endings_original
                .is_some()
        );
    }

    #[test]
    fn check_ignores_crlf_only_difference() {
        assert!(!first_newline_is_crlf("a = 1\n"));
        assert!(first_newline_is_crlf("a = 1\r\n"));
        assert!(!first_newline_is_crlf("a = 1\nb = 2\r\n"));
    }

    #[test]
    fn atomic_write_replaces_and_keeps_mode() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.toml");
        fs::write(&path, "old\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        }
        atomic_write(&path, b"new\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
    }

    fn hunk(begin: usize, original: &str, expected: &str) -> RustfmtJsonMismatch {
        RustfmtJsonMismatch {
            original_begin_line: begin,
            original: original.to_string(),
            expected: expected.to_string(),
        }
    }

    /// The span of a hunk is the length of the text it replaces, not the
    /// difference of the two line numbers: for a pure insertion rustfmt leaves
    /// `original_begin_line` and `original_end_line` equal while `original` is
    /// empty, and reading the span off them deletes a line that should stay.
    #[test]
    fn an_insertion_hunk_does_not_consume_a_line() {
        let original = "use serde::Serialize;\nuse std::io::Read;\nfn main() {}\n";
        let rebuilt = apply_mismatches(
            original,
            &[
                hunk(1, "", "use std::io::Read;\n\n"),
                hunk(2, "use std::io::Read;\n", ""),
            ],
        );
        assert_eq!(
            rebuilt,
            "use std::io::Read;\n\nuse serde::Serialize;\nfn main() {}\n"
        );
    }

    #[test]
    fn a_replacement_and_a_deletion_rebuild_exactly() {
        let original = "fn a() {}\n\n\n\n\nfn b() {}\n";
        assert_eq!(
            apply_mismatches(original, &[hunk(3, "\n\n\n", "")]),
            "fn a() {}\n\nfn b() {}\n"
        );
        assert_eq!(
            apply_mismatches("fn  a( ){}\n", &[hunk(1, "fn  a( ){}\n", "fn a() {}\n")]),
            "fn a() {}\n"
        );
        assert_eq!(apply_mismatches(original, &[]), original);
    }

    /// rustfmt drops a leading byte-order mark whenever it rewrites a file and
    /// reports no hunk for it, so a reconstruction that kept it would not be the
    /// bytes a write run leaves behind.
    #[test]
    fn a_rewrite_drops_a_byte_order_mark() {
        let original = "\u{feff}fn  a( ){}\n";
        assert_eq!(
            apply_mismatches(original, &[hunk(1, "fn  a( ){}\n", "fn a() {}\n")]),
            "fn a() {}\n"
        );
        assert_eq!(apply_mismatches(original, &[]), original);
    }

    /// One error per file, the way the TOML half already reports them, rather
    /// than one per invocation carrying five hundred files' worth of stderr.
    #[test]
    fn rustfmt_stderr_becomes_one_error_per_file() {
        let stderr = "\
error: expected expression, found `;`
 --> /tmp/a.rs:2:10
  |
2 |  let x = ;
  |          ^ expected expression

error[E0670]: `async fn` is not permitted in Rust 2015
 --> /tmp/b.rs:1:1
  |
1 | async fn f() {}
  | ^^^^^
";
        let errors = rustfmt_diagnostics(stderr, 1);
        assert_eq!(errors.len(), 2);
        match &errors[0] {
            Error::RustfmtDiagnostic {
                path,
                line,
                column,
                message,
            } => {
                assert_eq!(path, Path::new("/tmp/a.rs"));
                assert_eq!((*line, *column), (2, 10));
                assert!(
                    message.starts_with("error: expected expression, found `;`"),
                    "{message}"
                );
                assert!(message.contains("expected expression"), "{message}");
                assert!(!message.contains("-->"), "the location is structural now");
            }
            other => panic!("expected a per-file diagnostic, got {other:?}"),
        }
        match &errors[1] {
            Error::RustfmtDiagnostic { path, line, .. } => {
                assert_eq!(path, Path::new("/tmp/b.rs"));
                assert_eq!(*line, 1);
            }
            other => panic!("expected a per-file diagnostic, got {other:?}"),
        }
    }

    /// A failure with no location -- a bad option, a panic -- has no file to be
    /// attributed to, and is kept whole rather than dropped.
    #[test]
    fn a_diagnostic_without_a_location_is_still_reported() {
        let errors = rustfmt_diagnostics("invalid key=val pair: `max_width=true`\n", 1);
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], Error::ToolFailed { code: 1, .. }));

        let mixed = rustfmt_diagnostics(
            "error: bad\n --> /tmp/a.rs:1:1\n\nRustfmt failed at /tmp/a.rs: internal error\n",
            101,
        );
        assert_eq!(mixed.len(), 2);
        assert!(matches!(mixed[0], Error::RustfmtDiagnostic { .. }));
        assert!(matches!(mixed[1], Error::ToolFailed { .. }));

        assert!(rustfmt_diagnostics("", 0).is_empty());
        assert!(rustfmt_diagnostics("   \n", 0).is_empty());
        assert!(matches!(
            rustfmt_diagnostics("", 101).as_slice(),
            [Error::ToolFailed { code: 101, .. }]
        ));
        // A multi-byte first character must not be sliced apart.
        assert_eq!(rustfmt_diagnostics("é\n", 1).len(), 1);
    }

    #[test]
    fn a_location_is_read_from_the_right_so_a_colon_in_a_path_survives() {
        assert_eq!(
            split_location("/tmp/a:b.rs:12:3"),
            Some((PathBuf::from("/tmp/a:b.rs"), 12, 3))
        );
        assert_eq!(split_location("nonsense"), None);
        assert_eq!(split_location(":1:1"), None);
    }

    #[test]
    fn undecodable_rustfmt_json_is_an_error_not_a_clean_tree() {
        assert!(decode_rustfmt_json("", 0).unwrap().is_empty());
        assert!(decode_rustfmt_json("[]", 0).unwrap().is_empty());
        assert!(decode_rustfmt_json("not json", 0).is_err());
        assert!(decode_rustfmt_json("[{\"name\":\"a.rs\"", 0).is_err());
    }

    fn texts_of(split: Split) -> Vec<(PathBuf, String)> {
        match split {
            Split::Texts(texts) => texts,
            Split::Ambiguous => panic!("the split was ambiguous"),
        }
    }

    #[test]
    fn a_header_for_a_path_not_asked_for_stays_in_the_text() {
        let a = Path::new("/nonexistent/a.rs");
        let b = Path::new("/nonexistent/b.rs");
        let stdout = "/nonexistent/a.rs:\n\nfn a() {}\n/nonexistent/victim.rs:\n\nfn x() {}\n\
                      /nonexistent/b.rs:\n\nfn b() {}\n";

        let texts = texts_of(split_emit_stdout(stdout, &[a, b]));

        assert_eq!(
            texts,
            vec![
                (
                    a.to_path_buf(),
                    "fn a() {}\n/nonexistent/victim.rs:\n\nfn x() {}\n".to_string()
                ),
                (b.to_path_buf(), "fn b() {}\n".to_string()),
            ]
        );
    }

    #[test]
    fn a_forged_header_naming_a_sibling_makes_the_split_ambiguous() {
        let a = Path::new("/nonexistent/a.rs");
        let b = Path::new("/nonexistent/b.rs");
        let repeated = "/nonexistent/a.rs:\n\nfn a() {}\n/nonexistent/b.rs:\n\nfn stolen() {}\n\
                        /nonexistent/b.rs:\n\nfn b() {}\n";
        let reordered = "/nonexistent/a.rs:\n\nfn a() {}\n/nonexistent/b.rs:\n\nfn stolen() {}\n\
                         /nonexistent/a.rs:\n\nfn b() {}\n";
        let unanchored = "fn stray() {}\n/nonexistent/a.rs:\n\nfn a() {}\n";

        assert!(matches!(
            split_emit_stdout(repeated, &[a, b]),
            Split::Ambiguous
        ));
        assert!(matches!(
            split_emit_stdout(reordered, &[a, b]),
            Split::Ambiguous
        ));
        assert!(matches!(
            split_emit_stdout(unanchored, &[a, b]),
            Split::Ambiguous
        ));
    }

    #[test]
    fn a_file_formatted_alone_owns_everything_after_its_header() {
        let a = Path::new("/nonexistent/a.rs");
        let stdout = "/nonexistent/a.rs:\n\nfn a() {}\n/nonexistent/b.rs:\n\nfn b() {}\n";

        let texts = texts_of(split_emit_stdout(stdout, &[a]));
        let unheaded = texts_of(split_emit_stdout("fn a() {}\n", &[a]));

        assert_eq!(
            texts,
            vec![(
                a.to_path_buf(),
                "fn a() {}\n/nonexistent/b.rs:\n\nfn b() {}\n".to_string()
            )]
        );
        assert!(unheaded.is_empty());
    }

    #[test]
    fn a_file_rustfmt_returned_no_text_for_is_not_remembered_as_clean() {
        let silent = PathBuf::from("/nonexistent/silent.rs");
        let clean = PathBuf::from("/nonexistent/clean.rs");
        let known = Known {
            pending: vec![silent.clone(), clean.clone()],
            candidates: vec![
                (silent.clone(), cache::fingerprint(&silent, b"a", 7)),
                (clean.clone(), cache::fingerprint(&clean, b"b", 7)),
            ],
            saw_path_attr: false,
        };
        let outcome = RustOutcome {
            unconfirmed: vec![silent],
            ..RustOutcome::default()
        };
        let mut fresh = Vec::new();

        record_clean(&known, &outcome, Some(7), &mut fresh);

        assert_eq!(fresh, vec![cache::fingerprint(&clean, b"b", 7)]);
    }

    #[test]
    fn newline_style_is_read_the_way_rustfmt_reads_it() {
        let native = if cfg!(windows) {
            NewlineStyle::Windows
        } else {
            NewlineStyle::Unix
        };

        assert_eq!(NewlineStyle::named("Unix"), NewlineStyle::Unix);
        assert_eq!(NewlineStyle::named("\"windows\""), NewlineStyle::Windows);
        assert_eq!(NewlineStyle::named("Native"), native);
        assert_eq!(NewlineStyle::named("Auto"), NewlineStyle::Auto);
        assert_eq!(NewlineStyle::named("sideways"), NewlineStyle::Auto);
    }

    #[test]
    fn an_explicit_newline_style_replaces_the_one_the_source_had() {
        let path = Path::new("a.rs");
        let crlf = decode_source(b"fn a() {}\r\n", path).unwrap();
        let lf = decode_source(b"fn a() {}\n", path).unwrap();

        assert_eq!(
            encode_source(
                &crlf.text,
                &crlf.clone().with_newline_style(NewlineStyle::Auto)
            ),
            b"fn a() {}\r\n"
        );
        assert_eq!(
            encode_source(
                &crlf.text,
                &crlf.clone().with_newline_style(NewlineStyle::Unix)
            ),
            b"fn a() {}\n"
        );
        assert_eq!(
            encode_source(
                &lf.text,
                &lf.clone().with_newline_style(NewlineStyle::Windows)
            ),
            b"fn a() {}\r\n"
        );
    }

    #[test]
    fn a_failure_no_file_is_named_by_remembers_nothing() {
        let clean = PathBuf::from("/nonexistent/clean.rs");
        let known = Known {
            pending: vec![clean.clone()],
            candidates: vec![(clean.clone(), cache::fingerprint(&clean, b"b", 7))],
            saw_path_attr: false,
        };
        let outcome = RustOutcome {
            code: 1,
            errors: rustfmt_diagnostics("Could not parse TOML: max_width = \n", 1),
            ..RustOutcome::default()
        };
        let mut fresh = Vec::new();

        record_clean(&known, &outcome, Some(7), &mut fresh);

        assert!(fresh.is_empty());
    }

    #[test]
    fn a_file_that_never_settled_is_not_remembered_as_clean() {
        let temp = tempfile::tempdir().unwrap();
        let rewritten = temp.path().join("rewritten.rs");
        let oscillating = temp.path().join("oscillating.rs");
        let clean = temp.path().join("clean.rs");
        fs::write(&rewritten, "fn a() {}\n").unwrap();

        let known = Known {
            pending: vec![rewritten.clone(), oscillating.clone(), clean.clone()],
            candidates: vec![
                (rewritten.clone(), cache::fingerprint(&rewritten, b"a", 7)),
                (
                    oscillating.clone(),
                    cache::fingerprint(&oscillating, b"o", 7),
                ),
                (clean.clone(), cache::fingerprint(&clean, b"c", 7)),
            ],
            saw_path_attr: false,
        };
        let outcome = RustOutcome {
            changed: vec![rust_outcome(&rewritten, FileStatus::Formatted, None)],
            unsettled: vec![rewritten, oscillating],
            ..RustOutcome::default()
        };
        let mut fresh = Vec::new();

        record_clean(&known, &outcome, Some(7), &mut fresh);

        assert_eq!(fresh, vec![cache::fingerprint(&clean, b"c", 7)]);
    }

    #[test]
    fn a_source_that_could_forge_a_header_is_formatted_alone() {
        let chunk = [
            PathBuf::from("/nonexistent/a.rs"),
            PathBuf::from("/nonexistent/b.rs"),
            PathBuf::from("/nonexistent/c.rs"),
        ];
        let sources = [Some("fn a() {}\n"), Some("// /x/y.rs:\n"), None];

        let (together, alone) = partition_for_emit_stdout(&chunk, &sources);

        assert_eq!(together, vec![chunk[0].as_path()]);
        assert_eq!(
            alone,
            vec![vec![chunk[1].as_path()], vec![chunk[2].as_path()]]
        );
    }

    #[test]
    fn file_lines_names_every_file_and_range() {
        let ranges = [
            LineRange { start: 7, end: 13 },
            LineRange { start: 21, end: 21 },
        ];
        let payload = file_lines_argument(
            ["src/lib.rs".to_string(), "src/foo.rs".to_string()].into_iter(),
            &ranges,
        );
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 4);
        assert_eq!(parsed[0]["file"], "src/lib.rs");
        assert_eq!(parsed[0]["range"], serde_json::json!([7, 13]));
        assert_eq!(parsed[3]["file"], "src/foo.rs");
        assert_eq!(parsed[3]["range"], serde_json::json!([21, 21]));
    }

    #[test]
    fn lookup_error_before_write_leaves_files_alone() {
        struct FailLookup;
        impl VersionLookup for FailLookup {
            fn resolve(&self, _: DepRequest<'_>, _: Option<PartialVersion>) -> Resolution {
                Resolution::Skipped(SkipReason::LookupFailed)
            }
            fn take_error(&self) -> Option<(String, String)> {
                Some(("clap".into(), "boom".into()))
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("Cargo.toml");
        let original =
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nclap = \"4.6\"\n";
        fs::write(&manifest, original).unwrap();

        prewarm_manifest_versions(
            std::slice::from_ref(&manifest),
            &FormatterOptions::default(),
            &FailLookup,
        );
        assert_eq!(
            FailLookup.take_error(),
            Some(("clap".into(), "boom".into()))
        );
        assert_eq!(fs::read_to_string(&manifest).unwrap(), original);
    }

    #[test]
    fn a_manifest_edited_during_the_registry_barrier_is_formatted_from_its_new_bytes() {
        struct EditingLookup<'a> {
            manifest: &'a Path,
            edited: &'a str,
        }
        impl VersionLookup for EditingLookup<'_> {
            fn resolve(&self, _: DepRequest<'_>, _: Option<PartialVersion>) -> Resolution {
                Resolution::Skipped(SkipReason::NoMatchingRelease)
            }
            fn prewarm(&self, _: &[DepRequest<'_>]) {
                fs::write(self.manifest, self.edited).unwrap();
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("Cargo.toml");
        let original = "[package]\nname   =   \"x\"\n[dependencies]\nclap = \"4.6\"\n";
        let edited =
            "[package]\nname   =   \"x\"\n[dependencies]\nclap = \"4.6\"\nserde   =   \"1\"\n";
        fs::write(&manifest, original).unwrap();
        let lookup = EditingLookup {
            manifest: &manifest,
            edited,
        };
        let options = FormatterOptions::default();
        let prewarmed =
            prewarm_manifest_versions(std::slice::from_ref(&manifest), &options, &lookup);
        let cache = Cache::default();
        let job = TomlJob {
            options: &options,
            lookup: Some(&lookup),
            context: &ManifestContext::default(),
            prewarmed: &prewarmed,
            cache: &cache,
            cache_key: 0,
        };

        let status = read_and_format_toml(
            &manifest,
            job,
            &mut Vec::new(),
            &mut TomlOutcome::default(),
            &mut Vec::new(),
        )
        .unwrap();

        assert!(matches!(status, TomlFileStatus::Changed(_)));
        assert_eq!(
            fs::read_to_string(&manifest).unwrap(),
            "[package]\nname = \"x\"\n[dependencies]\nclap = \"4.6\"\nserde = \"1\"\n"
        );
    }

    #[test]
    fn a_prewarmed_source_is_reused_only_for_the_bytes_it_was_read_from() {
        let bytes = b"a = 1\n".to_vec();
        let warmed = PrewarmedManifest {
            source: decode_source(&bytes, Path::new("Cargo.toml")).unwrap(),
            bytes,
        };

        assert!(warmed.source_if_unchanged(b"a = 1\n").is_some());
        assert!(warmed.source_if_unchanged(b"a = 2\n").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_capability_probe_rejects_a_rustfmt_without_unstable_features() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("rustfmt");
        fs::write(
            &bin,
            "#!/bin/sh\necho \"Unrecognized option: 'unstable-features'\" >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(RustfmtCapabilities::probe(&bin).is_err());
    }

    /// The probe reads the option names off rustfmt itself, so an option that
    /// moved upstream is named rather than reaching the user as rustfmt's own
    /// `invalid key=val pair`.
    #[cfg(unix)]
    #[test]
    fn the_capability_probe_reads_the_option_names() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("rustfmt");
        fs::write(
            &bin,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then echo 'rustfmt 9.9.9-nightly'; exit 0; fi\n\
             printf 'max_width = 100\\ngroup_imports = \"Preserve\"\\n' > \"$4\"\n\
             exit 0\n",
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let probed = RustfmtCapabilities::probe(&bin).unwrap();
        assert_eq!(probed.version(), "rustfmt 9.9.9-nightly");
        assert!(probed.has("max_width"));
        assert!(probed.has("group_imports"));
        assert!(!probed.has("imports_granularity"));
    }

    /// A default the tool sets that rustfmt does not have is dropped with a
    /// warning; a key the caller named is an error, because that is how a typo
    /// goes unnoticed.
    #[test]
    fn reconciling_drops_a_stale_default_and_refuses_an_unknown_key() {
        let capabilities = RustfmtCapabilities {
            rustfmt: PathBuf::from("rustfmt"),
            options: ["group_imports".to_string(), "max_width".to_string()]
                .into_iter()
                .collect(),
            version: OnceLock::from("rustfmt 9.9.9-nightly".to_string()),
            unstable_cli: true,
        };

        let mut options = FormatterOptions::default();
        let notes = reconcile_with_rustfmt(&mut options, &capabilities).unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("imports_granularity"), "{notes:?}");
        assert_eq!(options.config.get("imports_granularity"), None);
        assert_eq!(
            options.config.get("group_imports"),
            Some("StdExternalCrate")
        );

        let mut typo = FormatterOptions {
            unset_configs: vec!["group_imprts".to_string()],
            ..FormatterOptions::default()
        };
        assert!(matches!(
            reconcile_with_rustfmt(&mut typo, &capabilities),
            Err(Error::UnknownRustfmtOption { .. })
        ));

        let mut miss = FormatterOptions {
            unset_misses: vec!["max_width".to_string()],
            ..FormatterOptions::default()
        };
        let notes = reconcile_with_rustfmt(&mut miss, &capabilities).unwrap();
        assert!(
            notes.iter().any(|note| note.contains("nothing to drop")),
            "{notes:?}"
        );
    }
}
