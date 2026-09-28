//! Reformat on save, without configuring an editor.
//!
//! `--watch` re-runs *exactly the run that was asked for* on each debounced
//! batch of filesystem events: it re-walks and re-plans from the paths on the
//! command line rather than feeding the changed paths back in as a file list.
//! That is not the cheap way round, it is the only correct one -- a file list is
//! classified by [`Filter`] alone, which carries the `--include`/`--exclude`
//! globs and nothing else, so the gitignore rules a walk applies would be
//! bypassed for paths nobody named. The content cache absorbs the difference:
//! every file a batch did not touch is a fingerprint hit.
//!
//! The event filter here is therefore a pre-filter, not the decision. Its only
//! job is to stop an event that cannot change the answer from paying for a run.
//! Anything it lets through costs one cached run that reports nothing; nothing
//! it drops could have changed what the walk finds.

use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use notify_debouncer_full::{
    DebouncedEvent, new_debouncer,
    notify::{self, EventKind, RecursiveMode, event::AccessKind},
};

use crate::{
    Result, Streams,
    cargo_config::{self, process_env},
    detector::{self, DirGate, TargetKind},
    error::Error,
    report::paint_error,
    runner::{self, FormatterOptions},
    selection::{Filter, Selector},
};

/// How long a path has to go quiet before its batch is delivered.
///
/// Long enough that an editor's save -- write a temporary file, rename it over
/// the old one -- arrives as one batch rather than two. Short enough that the
/// reformatted buffer is back on disk before anyone looks at it again.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// How often the debouncer looks for a batch that has gone quiet. Passing
/// `None` would derive a quarter of `DEBOUNCE`, which is this -- naming it means
/// changing the timeout cannot silently change the polling cost. Worst-case
/// added latency is `DEBOUNCE + TICK`.
const TICK: Duration = Duration::from_millis(50);

/// How many consecutive rounds of "our own writes caused more writes" it takes
/// before the watcher says so and stops chasing its own tail.
///
/// The same bound as `MAX_RUSTFMT_PASSES`, for the same reason: three passes is
/// the most a convergent formatter needs, so a fourth means it is not one.
const MAX_SELF_TRIGGER_ROUNDS: usize = 3;

/// How many paths a `changed:` note names before it counts the rest.
const NAMED_TRIGGERS: usize = 3;

/// Whether a filesystem watcher can be started on this machine at all.
///
/// The one thing `--watch` needs that no other mode does, and the one that can
/// fail for a reason outside this process -- so the tests gate on it the way
/// they gate on a rustfmt.
pub fn available() -> bool {
    let (events, _batches) = mpsc::channel();
    new_debouncer(DEBOUNCE, Some(TICK), events).is_ok()
}

/// Format once, then keep formatting as the tree changes.
///
/// Unlike [`crate::run`] this returns a `Result`: `crate::run` is the one place
/// a fatal is rendered, in JSON when that is what was asked for.
pub fn run(options: &FormatterOptions, streams: &Streams<'_>) -> Result<i32> {
    let selector = options.selector()?;
    let mut scopes = dedupe(scopes(options, &selector)?);

    let (events, batches) = mpsc::channel();
    let mut debouncer = new_debouncer(DEBOUNCE, Some(TICK), events).map_err(|err| failed(&err))?;
    for scope in &scopes {
        debouncer
            .watch(&scope.root, scope.recursive)
            .map_err(|err| failed(&err))?;
    }

    if talkative(options) {
        let _ = streams.note_line(&watching(&scopes));
    }

    // The first run comes after the watcher is installed, not before: a save
    // made during it would otherwise land in the gap and be lost.
    let mut session = Session::default();
    iterate(options, &selector, streams, &mut session, false)?;

    let stream = std::iter::from_fn(|| batches.recv().ok()).map(|batch| match batch {
        Ok(events) => events,
        // A backend that dropped events cannot say what changed, so it is worth
        // saying out loud -- and an empty batch below forces nothing, because
        // `need_rescan` on the events themselves is what asks for a re-run.
        Err(errors) => {
            if !options.quiet {
                for err in errors {
                    let _ = streams.note_line(&format!("warning: watch: {err}"));
                }
            }
            Vec::new()
        }
    });

    drive(
        stream,
        &mut scopes,
        options,
        &selector,
        streams,
        &mut session,
    )
}

/// The loop, over an event source.
///
/// Taking an iterator rather than the receiver is what makes it testable
/// without a filesystem watcher: a test hands it a finite sequence of batches
/// and asserts what it wrote.
fn drive(
    batches: impl Iterator<Item = Vec<DebouncedEvent>>,
    scopes: &mut [Scope],
    options: &FormatterOptions,
    selector: &Selector,
    streams: &Streams<'_>,
    session: &mut Session,
) -> Result<i32> {
    for events in batches {
        let changed = triggers(&events, scopes);
        if changed.is_empty() {
            continue;
        }
        let self_only = changed.iter().all(|path| session.wrote(path));
        if self_only && session.braked {
            continue;
        }
        // A batch caused only by the previous run's own writes still runs --
        // dropping it is how an edit gets lost -- but it has nothing to
        // announce, and the run itself will report anything it finds.
        if !self_only && talkative(options) {
            let _ = streams.note_line(&triggered(&changed));
        }
        iterate(options, selector, streams, session, self_only)?;
        for scope in scopes.iter_mut() {
            scope.refresh_build_dirs();
        }
    }
    // The channel only closes when the debouncer is dropped, which cannot
    // happen while `run` holds it -- so this is reached by a test, or by a
    // watcher that shut itself down.
    Ok(0)
}

/// One iteration: re-plan, re-run, report.
///
/// A failure is reported and returns `Ok`. Ending the loop on the first
/// unparseable file would defeat the point of watching: the next save is very
/// likely the fix.
fn iterate(
    options: &FormatterOptions,
    selector: &Selector,
    streams: &Streams<'_>,
    session: &mut Session,
    self_only: bool,
) -> Result<()> {
    let plan = match crate::plan(options, selector) {
        Ok(plan) => plan,
        // A tree with nothing formattable in it yet is what a watch is for: the
        // first file to land in it is the run's first work.
        Err(Error::NoFormattableFilesFound(_)) => return Ok(()),
        Err(err) => {
            let _ = streams.paint_note(|out| paint_error(&err, out));
            return Ok(());
        }
    };

    if !options.quiet && plan.warnings != session.warnings {
        for warning in &plan.warnings {
            let _ = streams.note_line(warning);
        }
        session.warnings.clone_from(&plan.warnings);
    }

    let explicit = plan.explicit;
    let warnings = plan.warnings.clone();
    let result = match runner::run_format_plan(plan, options, selector, streams) {
        Ok(result) => result,
        Err(err) => {
            let _ = streams.paint_note(|out| paint_error(&err, out));
            return Ok(());
        }
    };
    crate::report_run(&result, &warnings, explicit, options, streams, true)?;

    // Under `--check` nothing is written, so a changed path is one that *would*
    // change: recording it would make the next batch look self-caused.
    let rewrote: Vec<PathBuf> = if options.check {
        Vec::new()
    } else {
        result.changed_paths().map(Path::to_path_buf).collect()
    };
    if let Some(chasing) = session.observe(self_only, &rewrote) {
        let _ = streams.note_line(&chasing);
    }
    Ok(())
}

/// Whether commentary is wanted: `-q` silences it, and a JSON consumer is
/// reading envelopes rather than prose.
fn talkative(options: &FormatterOptions) -> bool {
    !options.quiet && options.message_format == crate::MessageFormat::Human
}

fn failed(err: &notify::Error) -> Error {
    Error::Watch {
        path: err.paths.first().cloned(),
        limit: matches!(err.kind, notify::ErrorKind::MaxFilesWatch),
        details: err.to_string(),
    }
}

fn watching(scopes: &[Scope]) -> String {
    let mut roots: Vec<String> = scopes
        .iter()
        .map(|scope| scope.root.display().to_string())
        .collect();
    roots.sort_unstable();
    format!("watching {}; Ctrl-C to stop", roots.join(", "))
}

fn triggered(changed: &[PathBuf]) -> String {
    let named: Vec<String> = changed
        .iter()
        .take(NAMED_TRIGGERS)
        .map(|path| path.display().to_string())
        .collect();
    let rest = changed.len() - named.len();
    if rest == 0 {
        format!("changed: {}", named.join(", "))
    } else {
        format!("changed: {} and {rest} more", named.join(", "))
    }
}

/// What the loop carries between iterations.
#[derive(Default)]
struct Session {
    /// Paths the previous run rewrote.
    ///
    /// Never used to drop an event on its own. A ledger of what this tool wrote
    /// can only be built by looking at the files *after* the run, and a save
    /// that landed during that window would be recorded as ours and then
    /// discarded -- a lost edit, which is the one failure a formatter must not
    /// have. Idempotence is what settles the loop instead: the write-triggered
    /// run is a cache hit that reports nothing.
    written: Vec<PathBuf>,
    rounds: usize,
    /// Set once a file has been named as never reaching a fixed point. Only
    /// then are self-caused batches skipped, and only until something else
    /// changes.
    braked: bool,
    warnings: Vec<String>,
}

impl Session {
    fn wrote(&self, path: &Path) -> bool {
        self.written.iter().any(|written| written == path)
    }

    /// Record what a run did, and say so when the watcher is chasing itself.
    fn observe(&mut self, self_only: bool, rewrote: &[PathBuf]) -> Option<String> {
        let mut chasing = None;
        if self_only && !rewrote.is_empty() {
            self.rounds += 1;
            if self.rounds >= MAX_SELF_TRIGGER_ROUNDS && !self.braked {
                self.braked = true;
                let mut names: Vec<String> = rewrote
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                names.sort_unstable();
                chasing = Some(format!(
                    "warning: {} is rewritten on every pass and never settles; \
                     ignoring further changes to it until something else changes",
                    names.join(", ")
                ));
            }
        } else {
            self.rounds = 0;
            self.braked = false;
        }
        self.written = rewrote.to_vec();
        chasing
    }
}

/// A directory to watch, and the rules that decide which of its events matter.
#[derive(Debug)]
struct Scope {
    root: PathBuf,
    recursive: RecursiveMode,
    filter: Filter,
    /// Where cargo would build this tree. Recomputed after each run, so an edit
    /// to `.cargo/config.toml` cannot leave the watcher pruning a directory
    /// that has moved -- `detector::collect` resolves the same thing per walk.
    build_dirs: Vec<PathBuf>,
    hidden: bool,
    /// A single-file target watches its parent *directory*, because an editor
    /// saves by renaming a new file over the old one and a watch on the file
    /// itself follows the inode that was replaced. Only the named path matters.
    only: Option<PathBuf>,
}

impl Scope {
    fn recursive(root: PathBuf, selector: &Selector) -> Result<Self> {
        let filter = selector.filter_for(&root)?;
        let mut scope = Self {
            root,
            recursive: RecursiveMode::Recursive,
            filter,
            build_dirs: Vec::new(),
            hidden: selector.hidden(),
            only: None,
        };
        scope.refresh_build_dirs();
        Ok(scope)
    }

    /// The directory holding a named file, watched for that file alone.
    fn containing(file: &Path, selector: &Selector) -> Result<Self> {
        let root = detector::absolutize(
            file.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        );
        let filter = selector.filter_for(&root)?;
        Ok(Self {
            root,
            recursive: RecursiveMode::NonRecursive,
            filter,
            build_dirs: Vec::new(),
            hidden: selector.hidden(),
            only: Some(detector::absolutize(file)),
        })
    }

    fn refresh_build_dirs(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| self.root.clone());
        self.build_dirs = cargo_config::target_dir(&self.root, &cwd, &process_env)
            .path
            .into_iter()
            .collect();
    }

    fn gate(&self) -> DirGate<'_> {
        DirGate {
            filter: &self.filter,
            prune_dirs: &[],
            build_dirs: &self.build_dirs,
            hidden: self.hidden,
        }
    }
}

/// The directories a run over `options.targets` reads from.
///
/// The same [`crate::plan`] the run uses, except a directory with nothing
/// formattable in it yet, which a plan refuses and a watch must not.
fn scopes(options: &FormatterOptions, selector: &Selector) -> Result<Vec<Scope>> {
    match crate::plan(options, selector) {
        Ok(plan) => {
            let mut scopes = Vec::with_capacity(plan.targets.len().max(1));
            for target in &plan.targets {
                scopes.push(scope_of(target, options, selector)?);
            }
            if scopes.is_empty() {
                let fallback = options
                    .targets
                    .first()
                    .map_or_else(|| PathBuf::from("."), PathBuf::clone);
                scopes.push(Scope::recursive(detector::absolutize(&fallback), selector)?);
            }
            Ok(scopes)
        }
        Err(Error::NoFormattableFilesFound(directory)) => Ok(vec![Scope::recursive(
            detector::absolutize(&directory),
            selector,
        )?]),
        Err(err) => Err(err),
    }
}

/// The tree one resolved target is read from.
///
/// A cargo target is watched from the Plan's workspace root under `--all`,
/// which is the tree the run formats. A named subdirectory is watched as named,
/// the same exception [`runner::named_scope`] makes.
fn scope_of(target: &TargetKind, options: &FormatterOptions, selector: &Selector) -> Result<Scope> {
    match target {
        TargetKind::CargoProject {
            manifest_path,
            root_dir,
            workspace_root,
        } => {
            let tree = if runner::named_scope(target).is_some() {
                root_dir.clone()
            } else if options.all {
                workspace_root.clone()
            } else {
                manifest_path
                    .parent()
                    .map_or_else(|| root_dir.clone(), Path::to_path_buf)
            };
            Scope::recursive(detector::absolutize(&tree), selector)
        }
        TargetKind::LooseDirectory { root_dir, .. } => {
            Scope::recursive(detector::absolutize(root_dir), selector)
        }
        TargetKind::SingleFile(file) => Scope::containing(file, selector),
        TargetKind::FileList {
            rust_files,
            toml_files,
        } => {
            // A file list has no tree of its own, so the run is watched through
            // the directories its files live in.
            let first = rust_files.iter().chain(toml_files).next();
            match first {
                Some(file) => Scope::containing(file, selector),
                None => Scope::recursive(detector::absolutize(Path::new(".")), selector),
            }
        }
    }
}

/// Drop every root another root already covers, so one edit arrives as one
/// event rather than one per nested watch.
fn dedupe(mut scopes: Vec<Scope>) -> Vec<Scope> {
    scopes.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| covers(right).cmp(&covers(left)))
    });

    let recursive: Vec<PathBuf> = scopes
        .iter()
        .filter(|scope| covers(scope))
        .map(|scope| scope.root.clone())
        .collect();

    let mut kept: Vec<Scope> = Vec::with_capacity(scopes.len());
    for scope in scopes {
        let covered = recursive
            .iter()
            .any(|outer| *outer != scope.root && scope.root.starts_with(outer));
        // A file scope inside a recursive root is dropped only when the root
        // would deliver its events anyway; a second scope for the same
        // directory is a duplicate whichever mode it has.
        if covered && scope.only.is_none() {
            continue;
        }
        if covered && scope.only.is_some() {
            continue;
        }
        if kept
            .iter()
            .any(|earlier| earlier.root == scope.root && earlier.only == scope.only)
        {
            continue;
        }
        kept.push(scope);
    }
    kept
}

fn covers(scope: &Scope) -> bool {
    matches!(scope.recursive, RecursiveMode::Recursive) && scope.only.is_none()
}

/// The paths in a batch that can change what a run would do.
fn triggers(events: &[DebouncedEvent], scopes: &[Scope]) -> Vec<PathBuf> {
    let mut changed: Vec<PathBuf> = events
        .iter()
        .filter_map(|event| triggered_by(event, scopes))
        .collect();
    changed.sort_unstable();
    changed.dedup();
    changed
}

fn triggered_by(event: &DebouncedEvent, scopes: &[Scope]) -> Option<PathBuf> {
    // The backend dropped events, so what changed is unknown and the only
    // honest answer is to re-run. inotify does this when its queue overflows.
    if event.need_rescan() {
        return Some(PathBuf::new());
    }
    // A read or an open changes nothing. notify's inotify backend does not
    // register for these, but a fsevent or polling backend can synthesise them,
    // so the arm is written rather than assumed.
    if matches!(event.kind, EventKind::Access(kind) if kind != AccessKind::Close(
        notify_debouncer_full::notify::event::AccessMode::Write
    )) {
        return None;
    }

    for path in &event.paths {
        for scope in scopes {
            if !path.starts_with(&scope.root) {
                continue;
            }
            if let Some(only) = &scope.only {
                if path == only {
                    return Some(path.clone());
                }
                continue;
            }
            if !reachable(path, scope) {
                continue;
            }
            // `classify` is the one place that decides whether a path is
            // formatted and as what. A walked entry, an explicitly named file
            // and now an event all come through it, so the three cannot
            // disagree.
            if scope.filter.classify(path).is_some() {
                return Some(path.clone());
            }
            // A directory renamed into the tree gives one event for the
            // directory and none for the files inside it, so the run has to be
            // told about the directory itself. It costs one reporting-nothing
            // run when the directory holds nothing formattable.
            //
            // The gate is asked about the directory itself and not merely
            // `allows_dir`, because `reachable` above only gates a path's
            // *ancestors* -- and the event that creates `target/` arrives
            // before there is a `CACHEDIR.TAG` in it to recognise.
            if path.is_dir()
                && let Some(name) = path.file_name()
                && !gate_prunes(scope, path, name)
            {
                return Some(path.clone());
            }
        }
    }
    None
}

fn gate_prunes(scope: &Scope, path: &Path, name: &std::ffi::OsStr) -> bool {
    scope.gate().prunes(path, name)
}

/// Whether the walk would reach `path` at all: every directory between the
/// scope root and it has to be one the walk descends into, and the entry itself
/// must not be hidden when hidden entries are not wanted.
fn reachable(path: &Path, scope: &Scope) -> bool {
    // The same test `WalkScope::classify` makes on an entry it walked to. A
    // hidden *file* is skipped outright; `.cargo/config` is not one -- its own
    // name is `config`, and it is the enclosing `.cargo` that the directory
    // gate lets through.
    if let Some(name) = path.file_name()
        && !scope.hidden
        && detector::hides_entry(name)
    {
        return false;
    }
    let gate = scope.gate();
    let mut directory = path.parent();
    while let Some(current) = directory {
        if current == scope.root || !current.starts_with(&scope.root) {
            return true;
        }
        let Some(name) = current.file_name() else {
            return true;
        };
        if gate.prunes(current, name) {
            return false;
        }
        directory = current.parent();
    }
    true
}

#[cfg(test)]
mod tests {
    use std::fs;

    use notify_debouncer_full::notify::{
        Event, EventKind,
        event::{AccessKind, CreateKind, Flag, ModifyKind, RemoveKind},
    };
    use tempfile::TempDir;

    use super::*;
    use crate::selection::{Languages, SelectionOptions};

    fn selector(options: &SelectionOptions) -> Selector {
        Selector::new(options).expect("selector")
    }

    fn plain() -> Selector {
        selector(&SelectionOptions::default())
    }

    fn options(target: &Path) -> FormatterOptions {
        FormatterOptions {
            targets: vec![target.to_path_buf()],
            ..FormatterOptions::default()
        }
    }

    fn tree() -> TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, contents).expect("write");
    }

    fn event(kind: EventKind, path: &Path) -> DebouncedEvent {
        DebouncedEvent {
            event: Event::new(kind).add_path(path.to_path_buf()),
            time: std::time::Instant::now(),
        }
    }

    fn modified(path: &Path) -> DebouncedEvent {
        event(EventKind::Modify(ModifyKind::Any), path)
    }

    fn only_scope(root: &Path) -> Vec<Scope> {
        vec![Scope::recursive(detector::absolutize(root), &plain()).expect("scope")]
    }

    // -- what gets watched ------------------------------------------------

    /// `--all` is the default and formats every member from the workspace root,
    /// so watching only the member the caller stood in would miss the files the
    /// run rewrites.
    #[test]
    fn a_workspace_member_is_watched_from_the_workspace_root() {
        let dir = tree();
        let root = dir.path();
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"a\"]\n");
        write(
            &root.join("a/Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        );
        write(&root.join("a/src/lib.rs"), "pub fn a() {}\n");

        let member = root.join("a");
        let scopes = scopes(&options(&member), &plain()).expect("scopes");
        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].root, detector::absolutize(root));
        assert!(covers(&scopes[0]));
    }

    #[test]
    fn two_workspace_members_are_one_watch_scope() {
        let dir = tree();
        let root = dir.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\n",
        );
        write(
            &root.join("a/Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        );
        write(&root.join("a/src/lib.rs"), "pub fn a() {}\n");
        write(
            &root.join("b/Cargo.toml"),
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\n",
        );
        write(&root.join("b/src/lib.rs"), "pub fn b() {}\n");

        let options = FormatterOptions {
            targets: vec![root.join("a"), root.join("b")],
            ..FormatterOptions::default()
        };
        let scopes = scopes(&options, &plain()).expect("scopes");
        assert_eq!(scopes.len(), 1, "{scopes:?}");
        assert_eq!(scopes[0].root, detector::absolutize(root));
    }

    #[test]
    fn no_all_is_watched_from_the_package() {
        let dir = tree();
        let root = dir.path();
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"a\"]\n");
        write(
            &root.join("a/Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        );
        write(&root.join("a/src/lib.rs"), "pub fn a() {}\n");

        let member = root.join("a");
        let mut options = options(&member);
        options.all = false;
        let scopes = scopes(&options, &plain()).expect("scopes");
        assert_eq!(scopes[0].root, detector::absolutize(&member));
    }

    /// A directory named inside a project is the scope the run uses, so it is
    /// the scope the watcher uses too.
    #[test]
    fn a_named_subdirectory_is_watched_as_named() {
        let dir = tree();
        let root = dir.path();
        write(
            &root.join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        );
        write(&root.join("src/lib.rs"), "pub fn a() {}\n");

        let source = root.join("src");
        let scopes = scopes(&options(&source), &plain()).expect("scopes");
        assert_eq!(scopes[0].root, detector::absolutize(&source));
    }

    /// An editor replaces a file by renaming a new one over it, so a watch on
    /// the file follows an inode nobody reads any more.
    #[test]
    fn a_single_file_is_watched_through_its_directory() {
        let dir = tree();
        let file = dir.path().join("x.toml");
        write(&file, "a = 1\n");

        let scopes = scopes(&options(&file), &plain()).expect("scopes");
        assert_eq!(scopes[0].root, detector::absolutize(dir.path()));
        assert!(!covers(&scopes[0]));
        assert_eq!(scopes[0].only, Some(detector::absolutize(&file)));
    }

    /// The tree a watch is started on may be the tree that has nothing in it
    /// yet. A plan refuses that; a watch is exactly what it is for.
    #[test]
    fn an_empty_directory_is_watched_rather_than_refused() {
        let dir = tree();
        let scopes = scopes(&options(dir.path()), &plain()).expect("scopes");
        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].root, detector::absolutize(dir.path()));
    }

    #[test]
    fn a_missing_path_is_still_a_failure() {
        let dir = tree();
        let missing = dir.path().join("nowhere");
        let err = scopes(&options(&missing), &plain()).expect_err("missing path");
        assert!(matches!(err, Error::PathNotFound(_)), "{err:?}");
    }

    #[test]
    fn nested_roots_collapse_to_the_outermost() {
        let dir = tree();
        let root = detector::absolutize(dir.path());
        let inner = root.join("a/b");
        fs::create_dir_all(&inner).expect("dirs");
        let file = inner.join("x.toml");
        write(&file, "a = 1\n");

        let kept = dedupe(vec![
            Scope::recursive(root.clone(), &plain()).expect("outer"),
            Scope::recursive(inner.clone(), &plain()).expect("inner"),
            Scope::containing(&file, &plain()).expect("file"),
        ]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].root, root);
    }

    #[test]
    fn the_same_root_is_watched_once() {
        let dir = tree();
        let root = detector::absolutize(dir.path());
        let kept = dedupe(vec![
            Scope::recursive(root.clone(), &plain()).expect("one"),
            Scope::recursive(root.clone(), &plain()).expect("two"),
        ]);
        assert_eq!(kept.len(), 1);
    }

    // -- what wakes a run -------------------------------------------------

    #[test]
    fn a_source_file_triggers() {
        let dir = tree();
        let scopes = only_scope(dir.path());
        for name in ["a.rs", "b.toml", "nested/c.rs"] {
            let path = detector::absolutize(&dir.path().join(name));
            write(&path, "");
            assert!(
                triggered_by(&modified(&path), &scopes).is_some(),
                "{name} should trigger"
            );
        }
    }

    #[test]
    fn anything_else_does_not_trigger() {
        let dir = tree();
        let scopes = only_scope(dir.path());
        for name in ["README.md", "a.txt", "Makefile"] {
            let path = detector::absolutize(&dir.path().join(name));
            write(&path, "");
            assert!(
                triggered_by(&modified(&path), &scopes).is_none(),
                "{name} should not trigger"
            );
        }
    }

    #[test]
    fn the_git_directory_does_not_trigger() {
        let dir = tree();
        let scopes = only_scope(dir.path());
        let path = detector::absolutize(&dir.path().join(".git/x.toml"));
        write(&path, "");
        assert!(triggered_by(&modified(&path), &scopes).is_none());
    }

    /// The reason the filter exists: one `cargo build` writes thousands of
    /// files, and a run for each of them would make `--watch` unusable.
    #[test]
    fn a_build_directory_does_not_trigger() {
        let dir = tree();
        write(
            &dir.path().join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        );
        let scopes = only_scope(dir.path());
        let path = detector::absolutize(&dir.path().join("target/debug/build/x.rs"));
        write(&path, "");
        assert!(triggered_by(&modified(&path), &scopes).is_none());
    }

    #[test]
    fn a_vendored_crate_does_not_trigger() {
        let dir = tree();
        let scopes = only_scope(dir.path());
        let vendored = dir.path().join("vendor/foo");
        write(&vendored.join(".cargo-checksum.json"), "{}");
        let path = detector::absolutize(&vendored.join("src/lib.rs"));
        write(&path, "");
        assert!(triggered_by(&modified(&path), &scopes).is_none());
    }

    #[test]
    fn a_cachedir_tagged_directory_does_not_trigger() {
        let dir = tree();
        let scopes = only_scope(dir.path());
        let built = dir.path().join("out");
        write(
            &built.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55",
        );
        let path = detector::absolutize(&built.join("x.rs"));
        write(&path, "");
        assert!(triggered_by(&modified(&path), &scopes).is_none());
    }

    #[test]
    fn an_excluded_path_does_not_trigger() {
        let dir = tree();
        let scopes = vec![
            Scope::recursive(
                detector::absolutize(dir.path()),
                &selector(&SelectionOptions {
                    exclude: vec!["gen/**".to_string()],
                    ..SelectionOptions::default()
                }),
            )
            .expect("scope"),
        ];
        let path = detector::absolutize(&dir.path().join("gen/x.rs"));
        write(&path, "");
        assert!(triggered_by(&modified(&path), &scopes).is_none());
    }

    /// `.cargo/config` is TOML with no extension, and the walk reaches it -- so
    /// the filter has to as well, while an ordinary hidden file stays out.
    #[test]
    fn a_hidden_file_does_not_trigger_but_a_cargo_config_does() {
        let dir = tree();
        let scopes = only_scope(dir.path());

        let hidden = detector::absolutize(&dir.path().join(".hidden.toml"));
        write(&hidden, "");
        assert!(triggered_by(&modified(&hidden), &scopes).is_none());

        let config = detector::absolutize(&dir.path().join(".cargo/config"));
        write(&config, "");
        assert!(triggered_by(&modified(&config), &scopes).is_some());
    }

    #[test]
    fn a_language_the_run_excluded_does_not_trigger() {
        let dir = tree();
        let scopes = vec![
            Scope::recursive(
                detector::absolutize(dir.path()),
                &selector(&SelectionOptions {
                    languages: Languages::Toml,
                    ..SelectionOptions::default()
                }),
            )
            .expect("scope"),
        ];
        let rust = detector::absolutize(&dir.path().join("a.rs"));
        write(&rust, "");
        assert!(triggered_by(&modified(&rust), &scopes).is_none());
    }

    #[test]
    fn reading_a_file_does_not_trigger() {
        let dir = tree();
        let scopes = only_scope(dir.path());
        let path = detector::absolutize(&dir.path().join("a.rs"));
        write(&path, "");
        let read = event(EventKind::Access(AccessKind::Read), &path);
        assert!(triggered_by(&read, &scopes).is_none());
    }

    /// The backend gave up on saying what changed, so the only honest answer is
    /// to run.
    #[test]
    fn a_dropped_event_queue_triggers() {
        let dir = tree();
        let scopes = only_scope(dir.path());
        let rescan = DebouncedEvent {
            event: Event::new(EventKind::Any).set_flag(Flag::Rescan),
            time: std::time::Instant::now(),
        };
        assert!(triggered_by(&rescan, &scopes).is_some());
    }

    /// A directory moved into the tree arrives as one event for the directory
    /// and none for the files inside it.
    #[test]
    fn a_new_directory_triggers() {
        let dir = tree();
        let scopes = only_scope(dir.path());
        let created = detector::absolutize(&dir.path().join("new"));
        fs::create_dir_all(&created).expect("dir");
        let birth = event(EventKind::Create(CreateKind::Folder), &created);
        assert!(triggered_by(&birth, &scopes).is_some());
    }

    /// A `cargo build` creates `target/` before there is a `CACHEDIR.TAG` in it,
    /// so the creation event has to be judged by the directory itself rather
    /// than by what it contains yet.
    #[test]
    fn a_new_build_directory_does_not_trigger() {
        let dir = tree();
        write(
            &dir.path().join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        );
        let scopes = only_scope(dir.path());
        let built = detector::absolutize(&dir.path().join("target"));
        fs::create_dir_all(&built).expect("dir");
        let birth = event(EventKind::Create(CreateKind::Folder), &built);
        assert!(triggered_by(&birth, &scopes).is_none());
    }

    #[test]
    fn a_deleted_source_file_triggers() {
        let dir = tree();
        let scopes = only_scope(dir.path());
        let path = detector::absolutize(&dir.path().join("gone.rs"));
        let removal = event(EventKind::Remove(RemoveKind::File), &path);
        assert!(triggered_by(&removal, &scopes).is_some());
    }

    #[test]
    fn a_sibling_of_a_watched_file_does_not_trigger() {
        let dir = tree();
        let watched = dir.path().join("x.toml");
        write(&watched, "a = 1\n");
        let scopes = vec![Scope::containing(&watched, &plain()).expect("scope")];

        assert!(triggered_by(&modified(&detector::absolutize(&watched)), &scopes).is_some());
        let sibling = detector::absolutize(&dir.path().join("y.toml"));
        write(&sibling, "");
        assert!(triggered_by(&modified(&sibling), &scopes).is_none());
    }

    #[test]
    fn a_path_outside_every_root_does_not_trigger() {
        let dir = tree();
        let other = tree();
        let scopes = only_scope(dir.path());
        let path = detector::absolutize(&other.path().join("a.rs"));
        write(&path, "");
        assert!(triggered_by(&modified(&path), &scopes).is_none());
    }

    #[test]
    fn a_batch_reports_each_path_once() {
        let dir = tree();
        let scopes = only_scope(dir.path());
        let path = detector::absolutize(&dir.path().join("a.rs"));
        write(&path, "");
        let batch = vec![modified(&path), modified(&path)];
        assert_eq!(triggers(&batch, &scopes), vec![path]);
    }

    // -- the self-trigger brake -------------------------------------------

    /// A run's own writes wake the watcher again. That batch still runs -- the
    /// alternative is dropping a save that landed in the same window -- but a
    /// file that is rewritten every time is named once and then left alone.
    #[test]
    fn three_self_caused_rewrites_stop_the_chase() {
        let mut session = Session::default();
        let churning = vec![PathBuf::from("/x.rs")];

        assert!(session.observe(false, &churning).is_none());
        assert!(session.wrote(Path::new("/x.rs")));
        assert!(session.observe(true, &churning).is_none());
        assert!(session.observe(true, &churning).is_none());
        let warning = session.observe(true, &churning).expect("a warning");
        assert!(warning.contains("/x.rs"), "{warning}");
        assert!(session.braked);
        // Named once, not once per batch.
        assert!(session.observe(true, &churning).is_none());
    }

    #[test]
    fn a_change_from_outside_resumes_the_watch() {
        let mut session = Session::default();
        let churning = vec![PathBuf::from("/x.rs")];
        for _ in 0..MAX_SELF_TRIGGER_ROUNDS {
            session.observe(true, &churning);
        }
        assert!(session.braked);
        session.observe(false, &churning);
        assert!(!session.braked);
        assert_eq!(session.rounds, 0);
    }

    /// A self-caused batch that rewrote nothing is the normal case -- the run
    /// converged -- and must not count towards the brake.
    #[test]
    fn a_settled_run_does_not_count_towards_the_brake() {
        let mut session = Session::default();
        for _ in 0..10 {
            assert!(session.observe(true, &[]).is_none());
        }
        assert!(!session.braked);
    }

    // -- the loop ---------------------------------------------------------

    fn toml_run(target: &Path) -> FormatterOptions {
        FormatterOptions {
            targets: vec![target.to_path_buf()],
            selection: SelectionOptions {
                languages: Languages::Toml,
                ..SelectionOptions::default()
            },
            ..FormatterOptions::default()
        }
    }

    /// Two identical batches: the file is formatted once, and the second batch
    /// says nothing. A watcher that narrated every one of its own writes would
    /// be unreadable.
    #[test]
    fn a_repeat_batch_formats_once_and_reports_once() {
        let dir = tree();
        let file = detector::absolutize(&dir.path().join("x.toml"));
        write(&file, "a={b=\"c\"}\n");

        let options = toml_run(dir.path());
        let selector = options.selector().expect("selector");
        let mut scopes = dedupe(scopes(&options, &selector).expect("scopes"));
        let batch = vec![modified(&file)];

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let streams = Streams::plain(&mut out, &mut err);
            let mut session = Session::default();
            let code = drive(
                vec![batch.clone(), batch].into_iter(),
                &mut scopes,
                &options,
                &selector,
                &streams,
                &mut session,
            )
            .expect("drive");
            assert_eq!(code, 0);
        }

        assert_eq!(fs::read_to_string(&file).expect("read"), "a.b = \"c\"\n");
        let commentary = String::from_utf8(err).expect("utf8");
        assert_eq!(
            commentary.matches("Formatted").count(),
            1,
            "one summary, not one per batch:\n{commentary}"
        );
    }

    /// A file that cannot be parsed is the normal state of a file someone is
    /// halfway through editing, so it must not end the watch.
    #[test]
    fn a_failure_does_not_end_the_loop() {
        let dir = tree();
        let broken = detector::absolutize(&dir.path().join("x.toml"));
        write(&broken, "a = = 1\n");
        let later = detector::absolutize(&dir.path().join("y.toml"));

        let options = toml_run(dir.path());
        let selector = options.selector().expect("selector");
        let mut scopes = dedupe(scopes(&options, &selector).expect("scopes"));

        // The second batch is delivered after the file it names exists, which
        // is what a real save looks like.
        write(&later, "d={e=\"f\"}\n");
        let batches = vec![vec![modified(&broken)], vec![modified(&later)]];

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let streams = Streams::plain(&mut out, &mut err);
            let mut session = Session::default();
            drive(
                batches.into_iter(),
                &mut scopes,
                &options,
                &selector,
                &streams,
                &mut session,
            )
            .expect("drive");
        }

        let commentary = String::from_utf8(err).expect("utf8");
        assert!(commentary.contains("x.toml"), "{commentary}");
        assert_eq!(
            fs::read_to_string(&later).expect("read"),
            "d.e = \"f\"\n",
            "the batch after a failure still formatted:\n{commentary}"
        );
    }

    #[test]
    fn a_batch_of_nothing_the_filter_wants_does_not_run() {
        let dir = tree();
        let file = detector::absolutize(&dir.path().join("x.toml"));
        write(&file, "a={b=\"c\"}\n");
        let ignored = detector::absolutize(&dir.path().join("notes.md"));
        write(&ignored, "");

        let options = toml_run(dir.path());
        let selector = options.selector().expect("selector");
        let mut scopes = dedupe(scopes(&options, &selector).expect("scopes"));

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let streams = Streams::plain(&mut out, &mut err);
            let mut session = Session::default();
            drive(
                vec![vec![modified(&ignored)]].into_iter(),
                &mut scopes,
                &options,
                &selector,
                &streams,
                &mut session,
            )
            .expect("drive");
        }
        assert_eq!(
            fs::read_to_string(&file).expect("read"),
            "a={b=\"c\"}\n",
            "an unrelated file must not cause a run"
        );
    }

    /// Under `--check` nothing is written, so a path that *would* change must
    /// not be recorded as this run's own work.
    #[test]
    fn a_check_run_records_no_writes() {
        let dir = tree();
        let file = detector::absolutize(&dir.path().join("x.toml"));
        write(&file, "a={b=\"c\"}\n");

        let mut options = toml_run(dir.path());
        options.check = true;
        let selector = options.selector().expect("selector");
        let mut scopes = dedupe(scopes(&options, &selector).expect("scopes"));

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let streams = Streams::plain(&mut out, &mut err);
            let mut session = Session::default();
            drive(
                vec![vec![modified(&file)]].into_iter(),
                &mut scopes,
                &options,
                &selector,
                &streams,
                &mut session,
            )
            .expect("drive");
            assert!(
                session.written.is_empty(),
                "a check run wrote nothing, so it owns nothing"
            );
        }
        assert_eq!(fs::read_to_string(&file).expect("read"), "a={b=\"c\"}\n");
    }

    #[test]
    fn a_note_names_a_few_paths_and_counts_the_rest() {
        let paths: Vec<PathBuf> = (0..5).map(|n| PathBuf::from(format!("/{n}.rs"))).collect();
        let note = triggered(&paths);
        assert!(note.ends_with("and 2 more"), "{note}");
        assert_eq!(triggered(&paths[..1]), "changed: /0.rs");
    }
}
