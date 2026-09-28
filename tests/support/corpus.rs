use std::{env, ffi::OsStr, path::PathBuf};

/// Every `.toml` file in the local crate registry, used as a differential
/// corpus. Override the location with `RUST_FORMATTER_CORPUS_DIR`.
pub fn corpus_files() -> Vec<PathBuf> {
    let Some(root) = corpus_root() else {
        return Vec::new();
    };

    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| path.extension() == Some(OsStr::new("toml")))
        .collect();

    files.sort();
    files
}

pub fn corpus_root() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("RUST_FORMATTER_CORPUS_DIR") {
        let path = PathBuf::from(dir);
        return path.is_dir().then_some(path);
    }

    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .or_else(|| env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".cargo")))?;

    let registry = cargo_home.join("registry").join("src");
    registry.is_dir().then_some(registry)
}

/// Run `body` over `items` on every core, collecting whatever it returns.
/// Mirrors the atomic-cursor pattern in `src/runner.rs` rather than pulling in
/// another dependency.
pub fn map_parallel<T, R, F>(items: &[T], body: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> Option<R> + Sync,
{
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        thread,
    };

    if items.is_empty() {
        return Vec::new();
    }

    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(items.len());

    thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let next = &next;
                let body = &body;
                scope.spawn(move || {
                    let mut out = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            return out;
                        };
                        if let Some(result) = body(item) {
                            out.push(result);
                        }
                    }
                })
            })
            .collect();

        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("corpus worker panicked"))
            .collect()
    })
}

/// Copy `files` into `dir` under flat `NNNNNN.toml` names.
///
/// Flattening is load-bearing: `detect_target` promotes any directory with an
/// ancestor `Cargo.toml` to a `CargoProject`, and the walker skips `Cargo.lock`,
/// `clippy.toml` and `rustfmt.toml` by name. Renaming sidesteps both, so the
/// tree stays a plain `LooseDirectory` and every file is actually visited.
pub fn flat_copy(files: &[PathBuf], dir: &std::path::Path) -> Vec<(PathBuf, PathBuf)> {
    let mut mapping = Vec::with_capacity(files.len());
    for (index, source) in files.iter().enumerate() {
        let dest = dir.join(format!("{index:06}.toml"));
        if std::fs::copy(source, &dest).is_ok() {
            mapping.push((source.clone(), dest));
        }
    }
    mapping
}

pub fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

/// Where a corpus sweep writes the inputs it failed on.
///
/// A sweep reads whatever the developer's registry happens to hold, so a
/// finding on one machine may never reproduce on another. Writing the offending
/// text out gives it an existence independent of that registry; promoting the
/// file into `tests/fixtures/` then pins it for good.
pub fn failure_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus-failures")
}

/// Writes `source` under a name derived from its own bytes, so the same input
/// failing under several profiles is written once per profile and never twice
/// for the same one.
pub fn record_failure(source: &str, profile: &str, kind: &str) -> std::io::Result<PathBuf> {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let dir = failure_dir();
    std::fs::create_dir_all(&dir)?;

    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    let path = dir.join(format!(
        "{}-{}-{:016x}.toml.in",
        slug(kind),
        slug(profile),
        hasher.finish()
    ));
    std::fs::write(&path, source)?;
    Ok(path)
}

fn slug(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    out.truncate(48);
    out.trim_matches('-').to_owned()
}
