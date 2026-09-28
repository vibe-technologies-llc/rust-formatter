use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// Cargo stamps every build directory it creates with this tag. It is the only
/// signal that separates a build directory from a source directory that happens
/// to be called `target`, and it survives `--target-dir`, which this tool never
/// sees.
const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// Cargo reads `config.toml` and, for compatibility, the extensionless `config`.
const CONFIG_NAMES: [&str; 2] = ["config.toml", "config"];

pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<OsString>;

pub fn process_env(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

/// One `.cargo/config.toml` that was read, with the directory a relative path in
/// it resolves against -- the directory holding `.cargo`, not the process
/// working directory.
struct Layer {
    base: PathBuf,
    doc: toml_edit::DocumentMut,
}

/// The cargo configuration in force for a tree, as the merge of every layer
/// cargo would read, nearest first. Lookups are per key path rather than per
/// table, which is how cargo resolves a key defined in more than one layer.
pub struct CargoConfig {
    layers: Vec<Layer>,
    unreadable: Vec<PathBuf>,
}

impl CargoConfig {
    pub fn load(start: &Path, env: EnvLookup<'_>) -> Self {
        let mut config = Self {
            layers: Vec::new(),
            unreadable: Vec::new(),
        };

        // Every ancestor, as cargo reads them. A depth cap here would stop
        // silently at exactly the layer that mattered, and the walk is two
        // `stat`s per directory against a path length the filesystem already
        // bounds.
        for dir in start.ancestors() {
            let cargo_dir = dir.join(".cargo");
            for name in CONFIG_NAMES {
                config.push_layer(&cargo_dir.join(name), dir);
            }
        }

        if let Some(home) = cargo_home(env) {
            for name in CONFIG_NAMES {
                config.push_layer(&home.join(name), &home);
            }
        }

        config
    }

    pub fn empty() -> Self {
        Self {
            layers: Vec::new(),
            unreadable: Vec::new(),
        }
    }

    /// Configuration files that exist but did not parse. Cargo would refuse to
    /// run at all; this tool keeps formatting, so the caller reports them.
    pub fn unreadable(&self) -> &[PathBuf] {
        &self.unreadable
    }

    pub fn get(&self, path: &[&str]) -> Option<&toml_edit::Item> {
        self.get_with_base(path).map(|(item, _)| item)
    }

    pub fn str(&self, path: &[&str]) -> Option<&str> {
        self.get(path)?.as_str()
    }

    pub fn bool(&self, path: &[&str]) -> Option<bool> {
        self.get(path)?.as_bool()
    }

    pub fn u64(&self, path: &[&str]) -> Option<u64> {
        self.get(path)?.as_integer()?.try_into().ok()
    }

    /// A path-valued key, resolved against the layer that defined it.
    pub fn path(&self, path: &[&str]) -> Option<PathBuf> {
        let (item, base) = self.get_with_base(path)?;
        Some(absolutize(Path::new(item.as_str()?), base))
    }

    /// The keys of a table, taken from the nearest layer that defines it.
    pub fn table_keys(&self, path: &[&str]) -> Vec<String> {
        self.layers
            .iter()
            .filter_map(|layer| lookup(layer.doc.as_item(), path))
            .filter_map(toml_edit::Item::as_table_like)
            .flat_map(|table| table.iter().map(|(key, _)| key.to_owned()))
            .collect()
    }

    fn get_with_base(&self, path: &[&str]) -> Option<(&toml_edit::Item, &Path)> {
        self.layers.iter().find_map(|layer| {
            lookup(layer.doc.as_item(), path).map(|item| (item, layer.base.as_path()))
        })
    }

    fn push_layer(&mut self, file: &Path, base: &Path) {
        let Ok(source) = std::fs::read_to_string(file) else {
            return;
        };
        match source.parse::<toml_edit::DocumentMut>() {
            Ok(doc) => self.layers.push(Layer {
                base: base.to_path_buf(),
                doc,
            }),
            Err(_) => self.unreadable.push(file.to_path_buf()),
        }
    }
}

fn lookup<'a>(item: &'a toml_edit::Item, path: &[&str]) -> Option<&'a toml_edit::Item> {
    path.iter()
        .try_fold(item, |item, segment| item.get(*segment))
}

/// Where cargo would build a tree rooted at `start`, and the configuration files
/// that could not be read while working it out.
pub struct BuildDir {
    pub path: Option<PathBuf>,
    /// Files that exist but did not parse. One of them may be the file that
    /// names the build directory, so a caller that pruned nothing and said
    /// nothing would go on to format generated code.
    pub unreadable: Vec<PathBuf>,
}

/// Resolve the build directory cargo would use for a tree rooted at `start`, in
/// cargo's own precedence order.
///
/// `cwd` is the process working directory, which is what cargo resolves a
/// relative `CARGO_TARGET_DIR` against -- not the directory being walked. A
/// relative `build.target-dir` is different: it belongs to the configuration
/// file that declared it, and [`CargoConfig::path`] resolves it there.
pub fn target_dir(start: &Path, cwd: &Path, env: EnvLookup<'_>) -> BuildDir {
    for key in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if let Some(value) = env(key).filter(|value| !value.is_empty()) {
            return BuildDir {
                path: Some(absolutize(Path::new(&value), cwd)),
                unreadable: Vec::new(),
            };
        }
    }

    let config = CargoConfig::load(start, env);
    BuildDir {
        path: config.path(&["build", "target-dir"]),
        unreadable: config.unreadable().to_vec(),
    }
}

fn cargo_home(env: EnvLookup<'_>) -> Option<PathBuf> {
    if let Some(home) = env("CARGO_HOME").filter(|home| !home.is_empty()) {
        return Some(PathBuf::from(home));
    }
    home_dir(env).map(|home| home.join(".cargo"))
}

pub fn home_dir(env: EnvLookup<'_>) -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env(key)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

fn absolutize(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Whether `dir` holds a cargo build directory, judged by evidence rather than
/// by name: a directory called `target` in a source tree is ordinary code.
pub fn is_build_dir(dir: &Path, name: &std::ffi::OsStr, resolved: &[PathBuf]) -> bool {
    is_named_build_dir(dir, name, resolved) || has_cachedir_tag(dir)
}

/// The half of the question a name answers.
///
/// Split out from `has_cachedir_tag` because that one reads a file in every
/// directory the walk enters, and a directory about to be pruned by its name
/// or by the filter should not pay for it.
pub fn is_named_build_dir(dir: &Path, name: &std::ffi::OsStr, resolved: &[PathBuf]) -> bool {
    if resolved.iter().any(|known| known == dir) {
        return true;
    }
    name == "target"
        && (dir.join(".rustc_info.json").is_file()
            || dir
                .parent()
                .is_some_and(|parent| parent.join("Cargo.toml").is_file()))
}

pub fn has_cachedir_tag(dir: &Path) -> bool {
    let Ok(bytes) = std::fs::read(dir.join("CACHEDIR.TAG")) else {
        return false;
    };
    bytes.starts_with(CACHEDIR_SIGNATURE)
}

/// A vendored dependency tree: cargo writes `.cargo-checksum.json` into every
/// crate it vendors, so a directory of such directories is not ours to rewrite.
pub fn is_vendor_dir(dir: &Path, name: &std::ffi::OsStr) -> bool {
    name == "vendor"
        && dir
            .parent()
            .is_some_and(|parent| parent.join("Cargo.toml").is_file())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs};

    use tempfile::tempdir;

    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> HashMap<String, OsString> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(*value)))
            .collect()
    }

    fn lookup(map: &HashMap<String, OsString>) -> impl Fn(&str) -> Option<OsString> + '_ {
        move |key| map.get(key).cloned()
    }

    fn write_config(dir: &Path, name: &str, body: &str) {
        let cargo = dir.join(".cargo");
        fs::create_dir_all(&cargo).unwrap();
        fs::write(cargo.join(name), body).unwrap();
    }

    /// Cargo resolves a relative `CARGO_TARGET_DIR` against the directory it was
    /// invoked from, not the tree it is building.
    #[test]
    fn a_relative_env_target_dir_resolves_against_the_working_directory() {
        let temp = tempdir().unwrap();
        let tree = temp.path().join("tree");
        let cwd = temp.path().join("cwd");
        fs::create_dir_all(&tree).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let map = env_from(&[("CARGO_TARGET_DIR", "build")]);
        assert_eq!(
            target_dir(&tree, &cwd, &lookup(&map)).path,
            Some(cwd.join("build"))
        );
    }

    #[test]
    fn an_unreadable_config_is_named_by_the_build_directory_lookup() {
        let temp = tempdir().unwrap();
        write_config(temp.path(), "config.toml", "[build\ntarget-dir = ");
        let map = env_from(&[]);
        let found = target_dir(temp.path(), temp.path(), &lookup(&map));
        assert_eq!(found.path, None);
        assert_eq!(found.unreadable.len(), 1);
    }

    #[test]
    fn env_target_dir_wins_over_config() {
        let temp = tempdir().unwrap();
        write_config(
            temp.path(),
            "config.toml",
            "[build]\ntarget-dir = \"from-config\"\n",
        );
        let map = env_from(&[("CARGO_TARGET_DIR", "/from/env")]);
        assert_eq!(
            target_dir(temp.path(), temp.path(), &lookup(&map)).path,
            Some(PathBuf::from("/from/env"))
        );
    }

    #[test]
    fn nearest_config_wins() {
        let temp = tempdir().unwrap();
        let nested = temp.path().join("crates").join("foo");
        fs::create_dir_all(&nested).unwrap();
        write_config(
            temp.path(),
            "config.toml",
            "[build]\ntarget-dir = \"outer\"\n",
        );
        write_config(&nested, "config.toml", "[build]\ntarget-dir = \"inner\"\n");
        let map = env_from(&[]);
        assert_eq!(
            target_dir(&nested, &nested, &lookup(&map)).path,
            Some(nested.join("inner"))
        );
    }

    #[test]
    fn extensionless_config_is_read() {
        let temp = tempdir().unwrap();
        write_config(temp.path(), "config", "[build]\ntarget-dir = \"legacy\"\n");
        let map = env_from(&[]);
        assert_eq!(
            target_dir(temp.path(), temp.path(), &lookup(&map)).path,
            Some(temp.path().join("legacy"))
        );
    }

    #[test]
    fn cargo_home_config_is_the_last_layer() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("cargo-home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            "[build]\ntarget-dir = \"/shared/target\"\n",
        )
        .unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let map = env_from(&[("CARGO_HOME", home.to_str().unwrap())]);
        assert_eq!(
            target_dir(&project, &project, &lookup(&map)).path,
            Some(PathBuf::from("/shared/target"))
        );
    }

    #[test]
    fn cargo_home_reads_the_extensionless_name_too() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("cargo-home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("config"), "[net]\nretry = 9\n").unwrap();
        let map = env_from(&[("CARGO_HOME", home.to_str().unwrap())]);

        let config = CargoConfig::load(temp.path(), &lookup(&map));
        assert_eq!(config.u64(&["net", "retry"]), Some(9));
    }

    #[test]
    fn the_nearest_layer_wins_per_key_rather_than_per_table() {
        let temp = tempdir().unwrap();
        let nested = temp.path().join("crates").join("foo");
        fs::create_dir_all(&nested).unwrap();
        write_config(
            temp.path(),
            "config.toml",
            "[http]\ntimeout = 5\nproxy = \"http://outer:3128\"\n",
        );
        write_config(&nested, "config.toml", "[http]\ntimeout = 11\n");
        let map = env_from(&[]);

        let config = CargoConfig::load(&nested, &lookup(&map));
        assert_eq!(config.u64(&["http", "timeout"]), Some(11));
        assert_eq!(config.str(&["http", "proxy"]), Some("http://outer:3128"));
        assert_eq!(config.bool(&["net", "offline"]), None);
    }

    #[test]
    fn a_config_that_does_not_parse_is_named_rather_than_swallowed() {
        let temp = tempdir().unwrap();
        write_config(temp.path(), "config.toml", "[http\ntimeout = ");
        let map = env_from(&[]);

        let config = CargoConfig::load(temp.path(), &lookup(&map));
        assert_eq!(config.unreadable().len(), 1);
        assert!(config.unreadable()[0].ends_with("config.toml"));
        assert_eq!(config.u64(&["http", "timeout"]), None);
    }

    #[test]
    fn a_source_table_is_readable_by_name() {
        let temp = tempdir().unwrap();
        write_config(
            temp.path(),
            "config.toml",
            "[source.crates-io]\nreplace-with = \"mirror\"\n\
             [source.mirror]\nregistry = \"sparse+https://mirror.example/index/\"\n",
        );
        let map = env_from(&[]);

        let config = CargoConfig::load(temp.path(), &lookup(&map));
        assert_eq!(
            config.str(&["source", "crates-io", "replace-with"]),
            Some("mirror")
        );
        assert_eq!(
            config.str(&["source", "mirror", "registry"]),
            Some("sparse+https://mirror.example/index/")
        );
        assert_eq!(config.table_keys(&["source"]), vec!["crates-io", "mirror"]);
    }

    #[test]
    fn a_relative_config_path_resolves_against_the_layer_that_declared_it() {
        let temp = tempdir().unwrap();
        write_config(
            temp.path(),
            "config.toml",
            "[http]\ncainfo = \"roots.pem\"\n",
        );
        let map = env_from(&[]);

        let config = CargoConfig::load(temp.path(), &lookup(&map));
        assert_eq!(
            config.path(&["http", "cainfo"]),
            Some(temp.path().join("roots.pem"))
        );
    }

    #[test]
    fn a_source_dir_named_target_is_not_a_build_dir() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("src").join("target");
        fs::create_dir_all(&source).unwrap();
        assert!(!is_build_dir(&source, "target".as_ref(), &[]));
    }

    #[test]
    fn a_target_dir_beside_a_manifest_is_a_build_dir() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let build = temp.path().join("target");
        fs::create_dir_all(&build).unwrap();
        assert!(is_build_dir(&build, "target".as_ref(), &[]));
    }

    #[test]
    fn a_cachedir_tag_marks_a_build_dir_under_any_name() {
        let temp = tempdir().unwrap();
        let build = temp.path().join("build-output");
        fs::create_dir_all(&build).unwrap();
        fs::write(
            build.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        assert!(has_cachedir_tag(&build));
        assert!(is_build_dir(&build, "build-output".as_ref(), &[]));
    }
}
