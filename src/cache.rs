//! What a previous run already proved about a file.
//!
//! An entry says: these exact bytes, under this exact configuration, are
//! already a fixed point. A run that finds one skips the parse and the rustfmt
//! invocation entirely, which is what a converged tree spends nearly all of its
//! time on.
//!
//! The key is the whole answer, so it has to be complete: a setting that
//! escaped it would let a stale entry hide a file that needs formatting, which
//! is the one failure a formatter must not have. Two things guard that -- the
//! configuration part is built by destructuring the options struct without
//! `..`, so a new setting cannot be added without a decision, and the file
//! carries a format version, so a change here is a miss rather than a
//! misparse.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use xxhash_rust::xxh3::xxh3_128;

use crate::cargo_config::{EnvLookup, home_dir};

const CACHE_DIR_ENV: &str = "RUST_FORMATTER_CACHE_DIR";
const DISABLE_ENV: &str = "RUST_FORMATTER_CACHE";

/// A format change is a miss rather than a misparse.
const MAGIC: &[u8] = b"rust-formatter-clean\x01";
/// `path` + `content` + `config` + `seen`.
const RECORD: usize = 16 + 16 + 16 + 4;
/// An entry nothing has looked at in this long is dropped, so a cache does not
/// grow forever with files that were renamed or deleted.
const RETENTION: u64 = 30 * 24 * 60 * 60;

/// Everything that decides whether one file is already formatted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    path: u128,
    content: u128,
    config: u128,
}

pub fn fingerprint(path: &Path, content: &[u8], config: u128) -> Fingerprint {
    Fingerprint {
        path: xxh3_128(path.as_os_str().as_encoded_bytes()),
        content: xxh3_128(content),
        config,
    }
}

/// The configuration half of a key, hashed from whatever the caller decided
/// belongs in it.
pub fn config_key(identity: &str) -> u128 {
    xxh3_128(identity.as_bytes())
}

pub fn blob_key(bytes: &[u8]) -> u128 {
    xxh3_128(bytes)
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    print: Fingerprint,
    seen: u32,
}

#[derive(Debug, Default)]
pub struct Cache {
    /// Sorted by path hash, so a lookup is a binary search rather than a hash
    /// map that would have to be rebuilt on every load.
    entries: Vec<Entry>,
    file: Option<PathBuf>,
}

impl Cache {
    /// The cache in force for one target root, or an inert one when caching is
    /// off or the cache directory is unusable.
    ///
    /// Nothing here is ever an error: a cache that cannot be read is a slower
    /// run, and a cache that cannot be written is a slower next one.
    pub fn load(root: &Path, enabled: bool, env: EnvLookup<'_>) -> Self {
        if !enabled || disabled_by_environment(env) {
            return Self::default();
        }
        let Some(dir) = directory(env) else {
            return Self::default();
        };
        let file = dir.join(format!(
            "{:032x}",
            xxh3_128(root.as_os_str().as_encoded_bytes())
        ));
        Self {
            entries: read(&file).unwrap_or_default(),
            file: Some(file),
        }
    }

    pub fn enabled(&self) -> bool {
        self.file.is_some()
    }

    pub fn contains(&self, print: Fingerprint) -> bool {
        self.entries
            .binary_search_by_key(&print.path, |entry| entry.print.path)
            .is_ok_and(|at| self.entries[at].print == print)
    }

    /// Fold this run's answers in and write the result.
    ///
    /// A concurrent run can only lose entries this way, never invent one: each
    /// process writes what it proved, and the loser of a race is re-proved next
    /// time.
    pub fn store(mut self, fresh: Vec<Fingerprint>) {
        let Some(file) = self.file.take() else { return };
        if fresh.is_empty() && self.entries.is_empty() {
            return;
        }
        let now = now();
        let cutoff = now.saturating_sub(RETENTION.min(u64::from(u32::MAX)));
        self.entries.retain(|entry| u64::from(entry.seen) >= cutoff);
        for print in fresh {
            let entry = Entry {
                print,
                seen: u32::try_from(now).unwrap_or(u32::MAX),
            };
            match self
                .entries
                .binary_search_by_key(&print.path, |held| held.print.path)
            {
                Ok(at) => self.entries[at] = entry,
                Err(at) => self.entries.insert(at, entry),
            }
        }
        write(&file, &self.entries);
    }
}

fn directory(env: EnvLookup<'_>) -> Option<PathBuf> {
    let dir = match env(CACHE_DIR_ENV)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
    {
        Some(dir) => dir,
        None => base_dir(env)?.join("files"),
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// `RUST_FORMATTER_CACHE=0` is the spelling a script reaches for when it cannot
/// add a flag, and it is read the same way every other boolean setting is.
fn disabled_by_environment(env: EnvLookup<'_>) -> bool {
    let Some(value) = env(DISABLE_ENV) else {
        return false;
    };
    let value = value.to_string_lossy().to_ascii_lowercase();
    matches!(value.as_str(), "0" | "false" | "no" | "off")
}

/// Where this tool keeps everything it caches: the registry index, and now the
/// per-file answers.
pub fn base_dir(env: EnvLookup<'_>) -> Option<PathBuf> {
    if cfg!(windows) {
        return env("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|dir| dir.join("rust-formatter").join("cache"));
    }
    if cfg!(target_os = "macos") {
        return home_dir(env).map(|home| home.join("Library/Caches/rust-formatter"));
    }
    env("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| home_dir(env).map(|home| home.join(".cache")))
        .map(|dir| dir.join("rust-formatter"))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn read(file: &Path) -> Option<Vec<Entry>> {
    let bytes = std::fs::read(file).ok()?;
    let body = bytes.strip_prefix(MAGIC)?;
    if body.len() % RECORD != 0 {
        return None;
    }
    let (records, _) = body.as_chunks::<RECORD>();
    let mut entries = Vec::with_capacity(records.len());
    for record in records {
        entries.push(Entry {
            print: Fingerprint {
                path: u128::from_le_bytes(record[0..16].try_into().ok()?),
                content: u128::from_le_bytes(record[16..32].try_into().ok()?),
                config: u128::from_le_bytes(record[32..48].try_into().ok()?),
            },
            seen: u32::from_le_bytes(record[48..52].try_into().ok()?),
        });
    }
    // A file written by this crate is already sorted, but one that is not must
    // not silently break the binary search.
    if entries
        .windows(2)
        .any(|pair| pair[0].print.path > pair[1].print.path)
    {
        entries.sort_by_key(|entry| entry.print.path);
    }
    Some(entries)
}

fn write(file: &Path, entries: &[Entry]) {
    let mut bytes = Vec::with_capacity(MAGIC.len() + entries.len() * RECORD);
    bytes.extend_from_slice(MAGIC);
    for entry in entries {
        bytes.extend_from_slice(&entry.print.path.to_le_bytes());
        bytes.extend_from_slice(&entry.print.content.to_le_bytes());
        bytes.extend_from_slice(&entry.print.config.to_le_bytes());
        bytes.extend_from_slice(&entry.seen.to_le_bytes());
    }
    let _ = crate::runner::atomic_write(file, &bytes);
}

/// What resolving and probing a rustfmt cost, so the next invocation does not
/// pay it again.
///
/// Two spawns -- `rustup which` and `--print-config default` -- are around
/// half of a single-file `--check`, which is the latency a format-on-save
/// keystroke waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    pub rustfmt: PathBuf,
    pub version: String,
    pub unstable_cli: bool,
    pub options: Vec<String>,
}

const TOOLCHAIN_MAGIC: &str = "rust-formatter-toolchain\t1\n";

/// The cached answer for `key`, if the binary it names is still the binary it
/// was.
///
/// Size and modification time are the check: rustup replaces the whole file on
/// an update, so a toolchain that moved on is a miss rather than a wrong answer
/// about which options it has.
pub fn read_toolchain(key: &str, enabled: bool, env: EnvLookup<'_>) -> Option<Toolchain> {
    if !enabled || disabled_by_environment(env) {
        return None;
    }
    let text = std::fs::read_to_string(toolchain_file(key, env)?).ok()?;
    let body = text.strip_prefix(TOOLCHAIN_MAGIC)?;
    let mut lines = body.lines();

    let mut head = lines.next()?.split('\t');
    let rustfmt = PathBuf::from(head.next()?);
    let size: u64 = head.next()?.parse().ok()?;
    let stamp: u64 = head.next()?.parse().ok()?;
    let unstable_cli = head.next()? == "1";
    let version = lines.next()?.to_string();
    if identity(&rustfmt)? != (size, stamp) {
        return None;
    }

    Some(Toolchain {
        rustfmt,
        version,
        unstable_cli,
        options: lines.map(str::to_owned).collect(),
    })
}

/// A cache is never a source of errors: a failed write is a slower next run.
pub fn write_toolchain(key: &str, found: &Toolchain, enabled: bool, env: EnvLookup<'_>) {
    if !enabled || disabled_by_environment(env) {
        return;
    }
    let Some(file) = toolchain_file(key, env) else {
        return;
    };
    let Some((size, stamp)) = identity(&found.rustfmt) else {
        return;
    };
    let mut text = String::with_capacity(4096);
    text.push_str(TOOLCHAIN_MAGIC);
    let unstable = u8::from(found.unstable_cli);
    let path = found.rustfmt.display();
    // A version string with a newline in it would be read back as an option.
    let version = found.version.replace('\n', " ");
    let _ = writeln!(text, "{path}\t{size}\t{stamp}\t{unstable}\n{version}");
    for option in &found.options {
        text.push_str(option);
        text.push('\n');
    }
    let _ = crate::runner::atomic_write(&file, text.as_bytes());
}

fn toolchain_file(key: &str, env: EnvLookup<'_>) -> Option<PathBuf> {
    let dir = directory(env)?.parent()?.join("toolchain");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("{:032x}", xxh3_128(key.as_bytes()))))
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let stamp = meta
        .modified()
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs());
    Some((meta.len(), stamp))
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |key: &str| {
            owned
                .iter()
                .find(|(held, _)| held == key)
                .map(|(_, value)| OsString::from(value))
        }
    }

    fn print(path: &str, content: &[u8], config: u128) -> Fingerprint {
        fingerprint(Path::new(path), content, config)
    }

    #[test]
    fn a_stored_answer_comes_back() {
        let temp = tempfile::tempdir().unwrap();
        let env = env_of(&[(CACHE_DIR_ENV, temp.path().to_str().unwrap())]);
        let one = print("/a.toml", b"a = 1\n", 7);

        let cache = Cache::load(Path::new("/root"), true, &env);
        assert!(!cache.contains(one));
        cache.store(vec![one]);

        assert!(Cache::load(Path::new("/root"), true, &env).contains(one));
    }

    /// The key is the whole answer, so every part of it has to matter.
    #[test]
    fn a_different_content_or_configuration_is_a_miss() {
        let temp = tempfile::tempdir().unwrap();
        let env = env_of(&[(CACHE_DIR_ENV, temp.path().to_str().unwrap())]);
        Cache::load(Path::new("/root"), true, &env).store(vec![print("/a.toml", b"a = 1\n", 7)]);

        let cache = Cache::load(Path::new("/root"), true, &env);
        assert!(cache.contains(print("/a.toml", b"a = 1\n", 7)));
        assert!(!cache.contains(print("/a.toml", b"a = 2\n", 7)));
        assert!(!cache.contains(print("/a.toml", b"a = 1\n", 8)));
        assert!(!cache.contains(print("/b.toml", b"a = 1\n", 7)));
    }

    /// Two roots must not read each other's answers, or moving a checkout
    /// would carry a stale verdict with it.
    #[test]
    fn each_root_has_its_own_file() {
        let temp = tempfile::tempdir().unwrap();
        let env = env_of(&[(CACHE_DIR_ENV, temp.path().to_str().unwrap())]);
        let one = print("/a.toml", b"a = 1\n", 7);
        Cache::load(Path::new("/one"), true, &env).store(vec![one]);
        assert!(!Cache::load(Path::new("/two"), true, &env).contains(one));
    }

    #[test]
    fn the_environment_can_switch_it_off() {
        let temp = tempfile::tempdir().unwrap();
        let env = env_of(&[
            (CACHE_DIR_ENV, temp.path().to_str().unwrap()),
            (DISABLE_ENV, "0"),
        ]);
        let cache = Cache::load(Path::new("/root"), true, &env);
        assert!(!cache.enabled());
        cache.store(vec![print("/a.toml", b"a = 1\n", 7)]);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_disabled_cache_never_reads_or_writes() {
        let temp = tempfile::tempdir().unwrap();
        let env = env_of(&[(CACHE_DIR_ENV, temp.path().to_str().unwrap())]);
        let cache = Cache::load(Path::new("/root"), false, &env);
        assert!(!cache.enabled());
        assert!(!cache.contains(print("/a.toml", b"a = 1\n", 7)));
    }

    /// A truncated or foreign file is a miss, not a panic and not a wrong
    /// answer.
    #[test]
    fn a_damaged_file_reads_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        let env = env_of(&[(CACHE_DIR_ENV, temp.path().to_str().unwrap())]);
        let one = print("/a.toml", b"a = 1\n", 7);
        Cache::load(Path::new("/root"), true, &env).store(vec![one]);

        let file = std::fs::read_dir(temp.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let bytes = std::fs::read(&file).unwrap();
        std::fs::write(&file, &bytes[..bytes.len() - 3]).unwrap();
        assert!(!Cache::load(Path::new("/root"), true, &env).contains(one));

        std::fs::write(&file, b"something else entirely").unwrap();
        assert!(!Cache::load(Path::new("/root"), true, &env).contains(one));
    }

    #[test]
    fn a_later_answer_replaces_an_earlier_one_for_the_same_file() {
        let temp = tempfile::tempdir().unwrap();
        let env = env_of(&[(CACHE_DIR_ENV, temp.path().to_str().unwrap())]);
        Cache::load(Path::new("/root"), true, &env).store(vec![print("/a.toml", b"a = 1\n", 7)]);
        Cache::load(Path::new("/root"), true, &env).store(vec![print("/a.toml", b"a = 2\n", 7)]);

        let cache = Cache::load(Path::new("/root"), true, &env);
        assert!(cache.contains(print("/a.toml", b"a = 2\n", 7)));
        assert!(!cache.contains(print("/a.toml", b"a = 1\n", 7)));
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn a_probed_toolchain_comes_back_until_the_binary_changes() {
        let temp = tempfile::tempdir().unwrap();
        let env = env_of(&[(CACHE_DIR_ENV, temp.path().join("files").to_str().unwrap())]);
        let rustfmt = temp.path().join("rustfmt");
        std::fs::write(&rustfmt, b"binary").unwrap();

        let found = Toolchain {
            rustfmt: rustfmt.clone(),
            version: "rustfmt 1.0.0-nightly".to_string(),
            unstable_cli: true,
            options: vec!["group_imports".to_string(), "max_width".to_string()],
        };
        write_toolchain("key", &found, true, &env);
        assert_eq!(read_toolchain("key", true, &env).as_ref(), Some(&found));
        assert_eq!(read_toolchain("other", true, &env), None);
        assert_eq!(read_toolchain("key", false, &env), None);

        // An upgraded toolchain is a different binary, and its option list may
        // differ; reporting the old one would be a wrong answer, not a stale
        // one.
        std::fs::write(&rustfmt, b"a different binary entirely").unwrap();
        assert_eq!(read_toolchain("key", true, &env), None);

        std::fs::remove_file(&rustfmt).unwrap();
        assert_eq!(read_toolchain("key", true, &env), None);
    }

    #[test]
    fn the_base_directory_follows_the_platform() {
        let env = env_of(&[("XDG_CACHE_HOME", "/xdg"), ("HOME", "/home/someone")]);
        let found = base_dir(&env).unwrap();
        assert!(found.ends_with("rust-formatter"), "{}", found.display());
        assert!(OsStr::new("x").is_empty() || found.is_absolute());
    }
}
