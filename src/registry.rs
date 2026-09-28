use std::{
    fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use semver::Version;
use serde::Deserialize;

use crate::{
    cargo_config::{CargoConfig, EnvLookup, home_dir},
    registry_http::{Fetched, HttpClient, HttpOptions, is_loopback_http},
    semver::PartialVersion,
};

/// The crates.io sparse index. The data-access policy lists it first and asks for
/// no rate limit, no user agent and no pagination, unlike the API this once used.
pub const DEFAULT_INDEX_URL: &str = "https://index.crates.io";

/// Kept from before `--registry-url` existed so the test suite can point the
/// whole path at a loopback server. Plain HTTP is accepted only for loopback.
const BASE_URL_ENV: &str = "RUST_FORMATTER_CRATES_IO_URL";
const CACHE_DIR_ENV: &str = "RUST_FORMATTER_REGISTRY_CACHE_DIR";

const USER_AGENT: &str = concat!("rust-formatter/", env!("CARGO_PKG_VERSION"));

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_RETRIES: u32 = 3;

/// Cargo's own `.cache` header, from `cargo::sources::registry::index::cache`.
/// A mismatch means a cargo old or new enough to disagree, which is read as
/// "no local copy" rather than as an error.
const CARGO_CACHE_VERSION: u8 = 3;
const CARGO_INDEX_FORMAT_MAX: u32 = 2;

/// Our own cache header, so a format change is a miss rather than a misparse.
const CACHE_MAGIC: &str = "rust-formatter-index\t1\t";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub version: Version,
    pub yanked: bool,
    pub rust_version: Option<PartialVersion>,
}

#[derive(Debug)]
pub enum LookupError {
    /// The registry has no such crate: a typo, or a dependency of a private
    /// registry that reached this path anyway. A skip, never a failure.
    NotFound,
    InvalidName,
    Offline,
    SourceReplaced(String),
    Transport(String),
    Status(u16),
    Malformed(String),
}

impl LookupError {
    /// Whether the run should stop. A missing crate is the user's manifest
    /// saying something this tool cannot act on; a 500 is this tool failing.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::Status(_) | Self::Malformed(_)
        )
    }
}

impl fmt::Display for LookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("no such crate on the registry"),
            Self::InvalidName => f.write_str("not a legal crate name"),
            Self::Offline => f.write_str("offline and not in the local registry index"),
            Self::SourceReplaced(source) => {
                write!(f, "crates.io is replaced by source `{source}`")
            }
            Self::Transport(details) => f.write_str(details),
            Self::Status(code) => write!(f, "http status {code}"),
            Self::Malformed(details) => write!(f, "the index entry did not parse: {details}"),
        }
    }
}

/// Where crates.io resolves to for this tree, which cargo's `[source]` tables
/// can move out from under a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrySource {
    Sparse(String),
    /// Replaced by a source whose versions this tool cannot enumerate, so no
    /// requirement in the tree may be pinned against crates.io.
    Replaced(String),
}

#[derive(Debug, Clone)]
pub struct RegistryCli {
    pub registry_url: Option<String>,
    pub offline: bool,
    pub concurrency: usize,
}

pub struct RegistryOptions {
    pub source: RegistrySource,
    pub offline: bool,
    pub http: HttpOptions,
    pub local_index: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub warnings: Vec<String>,
}

impl RegistryOptions {
    /// Everything cargo would consult, in cargo's precedence order: the command
    /// line, then the environment, then the merged `.cargo/config.toml` layers.
    pub fn resolve(start: &Path, env: EnvLookup<'_>, cli: &RegistryCli) -> Self {
        let config = CargoConfig::load(start, env);
        let mut warnings: Vec<String> = config
            .unreadable()
            .iter()
            .map(|path| {
                format!(
                    "warning: {} did not parse; its cargo settings were not applied",
                    path.display()
                )
            })
            .collect();

        let source = resolve_source(&config, cli.registry_url.as_deref(), env);
        let base = match &source {
            RegistrySource::Sparse(url) => url.clone(),
            RegistrySource::Replaced(_) => DEFAULT_INDEX_URL.to_owned(),
        };

        let offline = cli.offline
            || env_bool(env, "CARGO_NET_OFFLINE").unwrap_or_default()
            || config.bool(&["net", "offline"]).unwrap_or_default();

        let cainfo = env_path(env, "CARGO_HTTP_CAINFO")
            .or_else(|| config.path(&["http", "cainfo"]))
            .or_else(|| env_path(env, "SSL_CERT_FILE"));

        let http = HttpOptions {
            timeout: env_u64(env, "CARGO_HTTP_TIMEOUT")
                .or_else(|| config.u64(&["http", "timeout"]))
                .map_or(DEFAULT_TIMEOUT, Duration::from_secs),
            retries: env_u64(env, "CARGO_NET_RETRY")
                .or_else(|| config.u64(&["net", "retry"]))
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(DEFAULT_RETRIES),
            proxy: env_string(env, "CARGO_HTTP_PROXY")
                .or_else(|| config.str(&["http", "proxy"]).map(str::to_owned)),
            cainfo,
            concurrency: cli.concurrency.max(1),
            allow_cleartext: is_loopback_http(&base),
            user_agent: USER_AGENT.to_owned(),
        };

        let host = index_host(&base);
        let local_index = host.and_then(|host| local_index_dir(env, host));
        let cache_dir = cache_dir(env).map(|dir| dir.join(host.unwrap_or("index")));

        if let RegistrySource::Replaced(name) = &source {
            warnings.push(format!(
                "warning: crates.io is replaced by source `{name}`, so dependency versions were \
                 not looked up; pass --registry-url to name an index explicitly"
            ));
        }

        Self {
            source,
            offline,
            http,
            local_index,
            cache_dir,
            warnings,
        }
    }
}

pub struct Registry {
    base: String,
    source: RegistrySource,
    http: Option<HttpClient>,
    local_index: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
}

impl Registry {
    pub fn new(options: RegistryOptions) -> Result<Self, LookupError> {
        let base = match &options.source {
            RegistrySource::Sparse(url) => url.trim_end_matches('/').to_owned(),
            RegistrySource::Replaced(_) => String::new(),
        };
        let http = match (options.offline, &options.source) {
            (false, RegistrySource::Sparse(_)) => Some(
                HttpClient::new(&options.http)
                    .map_err(|err| LookupError::Transport(err.to_string()))?,
            ),
            _ => None,
        };

        Ok(Self {
            base,
            source: options.source,
            http,
            local_index: options.local_index,
            cache_dir: options.cache_dir,
        })
    }

    pub fn lookup(&self, name: &str) -> Result<Vec<IndexEntry>, LookupError> {
        if let RegistrySource::Replaced(source) = &self.source {
            return Err(LookupError::SourceReplaced(source.clone()));
        }
        if !is_valid_crate_name(name) {
            return Err(LookupError::InvalidName);
        }

        let path = index_path(name);
        let cached = self
            .cache_dir
            .as_ref()
            .and_then(|dir| read_our_cache(&dir.join(&path)))
            .or_else(|| {
                self.local_index
                    .as_ref()
                    .and_then(|dir| read_cargo_cache(&dir.join(&path)))
            });

        // A client is built for every source but a replaced one, so no client
        // here means the run is offline and the local index is all there is.
        let Some(http) = &self.http else {
            return cached.map_or(Err(LookupError::Offline), |entry| {
                parse_entries(&entry.body)
            });
        };

        let url = format!("{}/{path}", self.base);
        let etag = cached.as_ref().and_then(|entry| entry.etag.as_deref());
        match http.get(&url, etag) {
            Ok(Fetched::Body { bytes, etag }) => {
                let entries = parse_entries(&bytes)?;
                if let Some(dir) = &self.cache_dir {
                    write_our_cache(&dir.join(&path), etag.as_deref(), &bytes);
                }
                Ok(entries)
            }
            Ok(Fetched::NotModified) => match cached {
                Some(entry) => parse_entries(&entry.body),
                None => Err(LookupError::Transport(
                    "the registry answered 304 without a cached copy to reuse".to_owned(),
                )),
            },
            Ok(Fetched::NotFound) => Err(LookupError::NotFound),
            Err(err) => Err(match err.status_code() {
                Some(code) => LookupError::Status(code),
                None => LookupError::Transport(err.to_string()),
            }),
        }
    }
}

/// Cargo's rule, not crates.io's: a package name starts with a letter or `_` and
/// carries only ASCII alphanumerics, `-` and `_`. It is also the guard that keeps
/// a name out of the URL path it is interpolated into.
fn is_valid_crate_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if !(1..=64).contains(&bytes.len()) {
        return false;
    }
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return false;
    }
    bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The index path cargo uses, from the lowercased name.
fn index_path(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.len() {
        1 => format!("1/{lower}"),
        2 => format!("2/{lower}"),
        3 => format!("3/{}/{lower}", &lower[..1]),
        _ => format!("{}/{}/{lower}", &lower[..2], &lower[2..4]),
    }
}

#[derive(Deserialize)]
struct IndexLine {
    vers: String,
    #[serde(default)]
    yanked: bool,
    #[serde(default)]
    rust_version: Option<String>,
}

/// The index body is JSON Lines in **publish order, not semver order**, so a
/// caller must take the maximum rather than the last line.
fn parse_entries(bytes: &[u8]) -> Result<Vec<IndexEntry>, LookupError> {
    let mut entries = Vec::new();
    let mut lines = 0usize;

    for line in bytes.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        lines += 1;
        let Ok(parsed) = serde_json::from_slice::<IndexLine>(line) else {
            continue;
        };
        // A version this tool cannot order is one it must not select; the rest
        // of the crate's history is still usable.
        let Ok(version) = Version::parse(&parsed.vers) else {
            continue;
        };
        entries.push(IndexEntry {
            version,
            yanked: parsed.yanked,
            rust_version: parsed
                .rust_version
                .as_deref()
                .and_then(PartialVersion::parse),
        });
    }

    if entries.is_empty() && lines > 0 {
        return Err(LookupError::Malformed(format!(
            "{lines} line(s), none of them a version"
        )));
    }
    Ok(entries)
}

struct CachedIndex {
    etag: Option<String>,
    body: Vec<u8>,
}

fn read_our_cache(path: &Path) -> Option<CachedIndex> {
    let bytes = fs::read(path).ok()?;
    let rest = bytes.strip_prefix(CACHE_MAGIC.as_bytes())?;
    let split = rest.iter().position(|&b| b == b'\n')?;
    let etag = String::from_utf8(rest[..split].to_vec()).ok()?;
    Some(CachedIndex {
        etag: (!etag.is_empty()).then_some(etag),
        body: rest[split + 1..].to_vec(),
    })
}

fn write_our_cache(path: &Path, etag: Option<&str>, body: &[u8]) {
    let Some(parent) = path.parent() else { return };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let mut bytes = Vec::with_capacity(body.len() + 64);
    bytes.extend_from_slice(CACHE_MAGIC.as_bytes());
    bytes.extend_from_slice(etag.unwrap_or_default().as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(body);
    // A cache is never a source of errors: a failed write is a slower next run.
    let _ = crate::runner::atomic_write(path, &bytes);
}

/// Cargo's own index cache, read but never written. Layout: a version byte, a
/// little-endian index-format version, a NUL-terminated index version
/// (`etag: "…"` or `revision: …`), then `version NUL json NUL` pairs.
fn read_cargo_cache(path: &Path) -> Option<CachedIndex> {
    let bytes = fs::read(path).ok()?;
    let (&version, rest) = bytes.split_first()?;
    if version != CARGO_CACHE_VERSION || rest.len() < 4 {
        return None;
    }
    let (format, rest) = rest.split_at(4);
    if u32::from_le_bytes(format.try_into().ok()?) > CARGO_INDEX_FORMAT_MAX {
        return None;
    }

    let mut fields = rest.split(|&b| b == 0);
    let index_version = std::str::from_utf8(fields.next()?).ok()?;
    let etag = index_version
        .strip_prefix("etag: ")
        .map(|value| value.trim().to_owned());

    let mut body = Vec::with_capacity(bytes.len());
    // Fields alternate version, json; only the json half is the index line.
    while let (Some(_version), Some(json)) = (fields.next(), fields.next()) {
        if json.is_empty() {
            continue;
        }
        body.extend_from_slice(json);
        body.push(b'\n');
    }
    if body.is_empty() {
        return None;
    }
    Some(CachedIndex { etag, body })
}

fn resolve_source(
    config: &CargoConfig,
    explicit: Option<&str>,
    env: EnvLookup<'_>,
) -> RegistrySource {
    if let Some(url) = explicit.filter(|url| !url.trim().is_empty()) {
        return RegistrySource::Sparse(url.trim().to_owned());
    }
    if let Some(url) = env_string(env, BASE_URL_ENV).filter(|url| !url.trim().is_empty()) {
        return RegistrySource::Sparse(url.trim().to_owned());
    }

    let mut name = "crates-io".to_owned();
    for _ in 0..8 {
        let Some(next) = config.str(&["source", &name, "replace-with"]) else {
            break;
        };
        let next = next.to_owned();
        if next == name {
            break;
        }
        name = next;
    }

    if name == "crates-io" {
        return RegistrySource::Sparse(DEFAULT_INDEX_URL.to_owned());
    }
    match config.str(&["source", &name, "registry"]) {
        Some(url) => match url.strip_prefix("sparse+") {
            Some(sparse) => RegistrySource::Sparse(sparse.trim_end_matches('/').to_owned()),
            // A git-protocol registry holds the same data behind a protocol this
            // tool does not speak.
            None => RegistrySource::Replaced(name),
        },
        None => RegistrySource::Replaced(name),
    }
}

fn index_host(base: &str) -> Option<&str> {
    let (_, rest) = base.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => host.split(':').next().unwrap_or(host),
    };
    (!host.is_empty()).then_some(host)
}

/// Cargo names its index directory `<host>-<hash>`, where the hash is cargo's
/// own and not reproducible here, so the directory is found by its host prefix.
fn local_index_dir(env: EnvLookup<'_>, host: &str) -> Option<PathBuf> {
    let root = cargo_home(env)?.join("registry").join("index");
    let prefix = format!("{host}-");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;

    for entry in fs::read_dir(root).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) {
            continue;
        }
        let cache = entry.path().join(".cache");
        if !cache.is_dir() {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(seen, _)| modified > *seen) {
            best = Some((modified, cache));
        }
    }
    best.map(|(_, path)| path)
}

fn cargo_home(env: EnvLookup<'_>) -> Option<PathBuf> {
    if let Some(home) = env("CARGO_HOME").filter(|home| !home.is_empty()) {
        return Some(PathBuf::from(home));
    }
    home_dir(env).map(|home| home.join(".cargo"))
}

fn cache_dir(env: EnvLookup<'_>) -> Option<PathBuf> {
    env_path(env, CACHE_DIR_ENV).or_else(|| crate::cache::base_dir(env))
}

fn env_string(env: EnvLookup<'_>, key: &str) -> Option<String> {
    env(key)?.into_string().ok().filter(|v| !v.is_empty())
}

fn env_path(env: EnvLookup<'_>, key: &str) -> Option<PathBuf> {
    env(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_u64(env: EnvLookup<'_>, key: &str) -> Option<u64> {
    env_string(env, key)?.trim().parse().ok()
}

fn env_bool(env: EnvLookup<'_>, key: &str) -> Option<bool> {
    match env_string(env, key)?.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::OsString};

    use tempfile::tempdir;

    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> HashMap<String, OsString> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(*value)))
            .collect()
    }

    fn env_lookup(map: &HashMap<String, OsString>) -> impl Fn(&str) -> Option<OsString> + '_ {
        move |key| map.get(key).cloned()
    }

    fn write_config(dir: &Path, body: &str) {
        let cargo = dir.join(".cargo");
        fs::create_dir_all(&cargo).unwrap();
        fs::write(cargo.join("config.toml"), body).unwrap();
    }

    #[test]
    fn index_paths_follow_the_length_rules() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("so"), "2/so");
        assert_eq!(index_path("gcc"), "3/g/gcc");
        assert_eq!(index_path("serde"), "se/rd/serde");
        assert_eq!(index_path("Serde_JSON"), "se/rd/serde_json");
        assert_eq!(index_path("raw-window-handle"), "ra/w-/raw-window-handle");
    }

    #[test]
    fn cargo_permits_a_leading_underscore() {
        for name in ["serde", "serde_json", "pin-project-lite", "_foo", "a"] {
            assert!(is_valid_crate_name(name), "{name}");
        }
        for name in ["", "1abc", "-abc", "foo/bar", "../x", "foo bar", "foo?x"] {
            assert!(!is_valid_crate_name(name), "{name}");
        }
        assert!(!is_valid_crate_name(&"a".repeat(65)));
    }

    #[test]
    fn entries_are_read_in_publish_order_not_semver_order() {
        let body = br#"{"name":"x","vers":"1.10.0","yanked":false}
{"name":"x","vers":"1.9.0","yanked":true}
{"name":"x","vers":"1.2.0","yanked":false,"rust_version":"1.63"}

"#;
        let entries = parse_entries(body).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].version, Version::parse("1.10.0").unwrap());
        assert!(entries[1].yanked);
        assert_eq!(entries[2].rust_version, PartialVersion::parse("1.63"));
        assert_eq!(entries[0].rust_version, None);
    }

    #[test]
    fn an_unreadable_version_is_skipped_but_a_body_of_them_is_an_error() {
        let mixed = br#"{"name":"x","vers":"not-a-version"}
{"name":"x","vers":"1.0.0"}
"#;
        assert_eq!(parse_entries(mixed).unwrap().len(), 1);

        assert!(parse_entries(b"garbage\n").is_err());
        assert!(parse_entries(b"").unwrap().is_empty());
    }

    #[test]
    fn our_cache_round_trips_body_and_etag() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("se/rd/serde");

        write_our_cache(&path, Some("\"abc\""), b"{\"vers\":\"1.0.0\"}\n");
        let cached = read_our_cache(&path).unwrap();
        assert_eq!(cached.etag.as_deref(), Some("\"abc\""));
        assert_eq!(cached.body, b"{\"vers\":\"1.0.0\"}\n");

        write_our_cache(&path, None, b"body\n");
        let cached = read_our_cache(&path).unwrap();
        assert_eq!(cached.etag, None);
        assert_eq!(cached.body, b"body\n");
    }

    #[test]
    fn a_cache_file_from_another_format_is_a_miss_not_an_error() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("x");
        fs::write(&path, b"some other tool's file").unwrap();
        assert!(read_our_cache(&path).is_none());
        assert!(read_cargo_cache(&path).is_none());
        assert!(read_our_cache(&temp.path().join("missing")).is_none());
    }

    fn cargo_cache_bytes(index_version: &str, rows: &[(&str, &str)]) -> Vec<u8> {
        let mut bytes = vec![CARGO_CACHE_VERSION];
        bytes.extend_from_slice(&CARGO_INDEX_FORMAT_MAX.to_le_bytes());
        bytes.extend_from_slice(index_version.as_bytes());
        bytes.push(0);
        for (version, json) in rows {
            bytes.extend_from_slice(version.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(json.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn cargos_own_index_cache_is_readable() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("se/rd/serde");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            cargo_cache_bytes(
                "etag: \"69ee5a\"",
                &[
                    ("1.0.0", r#"{"name":"serde","vers":"1.0.0","yanked":false}"#),
                    ("1.1.0", r#"{"name":"serde","vers":"1.1.0","yanked":false}"#),
                ],
            ),
        )
        .unwrap();

        let cached = read_cargo_cache(&path).unwrap();
        assert_eq!(cached.etag.as_deref(), Some("\"69ee5a\""));
        let entries = parse_entries(&cached.body).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].version, Version::parse("1.1.0").unwrap());
    }

    #[test]
    fn a_git_revision_is_not_offered_as_an_etag() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("x");
        fs::write(
            &path,
            cargo_cache_bytes("revision: deadbeef", &[("1.0.0", r#"{"vers":"1.0.0"}"#)]),
        )
        .unwrap();
        assert_eq!(read_cargo_cache(&path).unwrap().etag, None);
    }

    #[test]
    fn a_newer_cargo_cache_version_is_ignored() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("x");
        let mut bytes = cargo_cache_bytes("etag: \"a\"", &[("1.0.0", r#"{"vers":"1.0.0"}"#)]);
        bytes[0] = CARGO_CACHE_VERSION + 1;
        fs::write(&path, &bytes).unwrap();
        assert!(read_cargo_cache(&path).is_none());
    }

    #[test]
    fn the_default_source_is_the_sparse_index() {
        let map = env_from(&[]);
        let config = CargoConfig::empty();
        assert_eq!(
            resolve_source(&config, None, &env_lookup(&map)),
            RegistrySource::Sparse(DEFAULT_INDEX_URL.to_owned())
        );
    }

    #[test]
    fn an_explicit_url_outranks_the_environment_and_the_config() {
        let temp = tempdir().unwrap();
        write_config(
            temp.path(),
            "[source.crates-io]\nreplace-with = \"vendored\"\n[source.vendored]\ndirectory = \"vendor\"\n",
        );
        let map = env_from(&[(BASE_URL_ENV, "http://127.0.0.1:1/env")]);
        let config = CargoConfig::load(temp.path(), &env_lookup(&map));

        assert_eq!(
            resolve_source(
                &config,
                Some("https://mirror.example/index"),
                &env_lookup(&map)
            ),
            RegistrySource::Sparse("https://mirror.example/index".to_owned())
        );
        assert_eq!(
            resolve_source(&config, None, &env_lookup(&map)),
            RegistrySource::Sparse("http://127.0.0.1:1/env".to_owned())
        );
    }

    #[test]
    fn a_vendored_source_refuses_to_pin() {
        let temp = tempdir().unwrap();
        write_config(
            temp.path(),
            "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\
             [source.vendored-sources]\ndirectory = \"vendor\"\n",
        );
        let map = env_from(&[]);
        let config = CargoConfig::load(temp.path(), &env_lookup(&map));
        assert_eq!(
            resolve_source(&config, None, &env_lookup(&map)),
            RegistrySource::Replaced("vendored-sources".to_owned())
        );
    }

    #[test]
    fn a_sparse_mirror_is_followed_through_the_replacement_chain() {
        let temp = tempdir().unwrap();
        write_config(
            temp.path(),
            "[source.crates-io]\nreplace-with = \"first\"\n\
             [source.first]\nreplace-with = \"mirror\"\n\
             [source.mirror]\nregistry = \"sparse+https://mirror.example/index/\"\n",
        );
        let map = env_from(&[]);
        let config = CargoConfig::load(temp.path(), &env_lookup(&map));
        assert_eq!(
            resolve_source(&config, None, &env_lookup(&map)),
            RegistrySource::Sparse("https://mirror.example/index".to_owned())
        );
    }

    #[test]
    fn a_replacement_cycle_terminates() {
        let temp = tempdir().unwrap();
        write_config(
            temp.path(),
            "[source.crates-io]\nreplace-with = \"a\"\n\
             [source.a]\nreplace-with = \"b\"\n[source.b]\nreplace-with = \"a\"\n",
        );
        let map = env_from(&[]);
        let config = CargoConfig::load(temp.path(), &env_lookup(&map));
        assert!(matches!(
            resolve_source(&config, None, &env_lookup(&map)),
            RegistrySource::Replaced(_)
        ));
    }

    #[test]
    fn cargo_network_settings_are_honoured_with_the_environment_on_top() {
        let temp = tempdir().unwrap();
        write_config(
            temp.path(),
            "[http]\ntimeout = 5\nproxy = \"http://config.example:3128\"\n[net]\nretry = 7\noffline = true\n",
        );
        let map = env_from(&[
            ("HOME", temp.path().to_str().unwrap()),
            ("CARGO_HTTP_TIMEOUT", "11"),
        ]);
        let options = RegistryOptions::resolve(
            temp.path(),
            &env_lookup(&map),
            &RegistryCli {
                registry_url: None,
                offline: false,
                concurrency: 4,
            },
        );

        assert_eq!(options.http.timeout, Duration::from_secs(11));
        assert_eq!(options.http.retries, 7);
        assert_eq!(
            options.http.proxy.as_deref(),
            Some("http://config.example:3128")
        );
        assert!(options.offline);
    }

    #[test]
    fn an_unreadable_cargo_config_is_reported_rather_than_swallowed() {
        let temp = tempdir().unwrap();
        write_config(temp.path(), "[http\ntimeout = ");
        let map = env_from(&[("HOME", temp.path().to_str().unwrap())]);
        let options = RegistryOptions::resolve(
            temp.path(),
            &env_lookup(&map),
            &RegistryCli {
                registry_url: None,
                offline: false,
                concurrency: 1,
            },
        );
        assert!(
            options
                .warnings
                .iter()
                .any(|warning| warning.contains("did not parse")),
            "{:?}",
            options.warnings
        );
    }

    #[test]
    fn a_replaced_source_refuses_every_lookup() {
        let registry = Registry::new(RegistryOptions {
            source: RegistrySource::Replaced("vendored".to_owned()),
            offline: false,
            http: HttpOptions {
                timeout: Duration::from_secs(1),
                retries: 0,
                proxy: None,
                cainfo: None,
                concurrency: 1,
                allow_cleartext: false,
                user_agent: USER_AGENT.to_owned(),
            },
            local_index: None,
            cache_dir: None,
            warnings: Vec::new(),
        })
        .unwrap();

        let err = registry.lookup("serde").unwrap_err();
        assert!(matches!(err, LookupError::SourceReplaced(_)));
        assert!(!err.is_fatal());
    }

    #[test]
    fn offline_reads_the_local_index_and_skips_what_is_missing() {
        let temp = tempdir().unwrap();
        let index = temp.path().join("cache");
        let path = index.join(index_path("serde"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            cargo_cache_bytes(
                "etag: \"a\"",
                &[("1.0.0", r#"{"name":"serde","vers":"1.0.0","yanked":false}"#)],
            ),
        )
        .unwrap();

        let registry = Registry::new(RegistryOptions {
            source: RegistrySource::Sparse(DEFAULT_INDEX_URL.to_owned()),
            offline: true,
            http: HttpOptions {
                timeout: Duration::from_secs(1),
                retries: 0,
                proxy: None,
                cainfo: None,
                concurrency: 1,
                allow_cleartext: false,
                user_agent: USER_AGENT.to_owned(),
            },
            local_index: Some(index),
            cache_dir: None,
            warnings: Vec::new(),
        })
        .unwrap();

        assert_eq!(registry.lookup("serde").unwrap().len(), 1);
        let missing = registry.lookup("nothing-here").unwrap_err();
        assert!(matches!(missing, LookupError::Offline));
        assert!(!missing.is_fatal());
    }

    #[test]
    fn only_a_tool_failure_is_fatal() {
        assert!(!LookupError::NotFound.is_fatal());
        assert!(!LookupError::InvalidName.is_fatal());
        assert!(!LookupError::Offline.is_fatal());
        assert!(!LookupError::SourceReplaced("x".into()).is_fatal());
        assert!(LookupError::Transport("reset".into()).is_fatal());
        assert!(LookupError::Status(500).is_fatal());
        assert!(LookupError::Malformed("x".into()).is_fatal());
    }

    #[test]
    fn a_host_is_taken_from_the_index_url() {
        assert_eq!(
            index_host("https://index.crates.io"),
            Some("index.crates.io")
        );
        assert_eq!(index_host("http://127.0.0.1:8080/index"), Some("127.0.0.1"));
        assert_eq!(index_host("nonsense"), None);
    }
}
