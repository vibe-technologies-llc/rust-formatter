use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    cargo_config::{EnvLookup, home_dir, process_env},
    error::{Error, Result},
};

/// The two names rustfmt looks for, in the order it looks for them.
const CONFIG_NAMES: [&str; 2] = ["rustfmt.toml", ".rustfmt.toml"];

pub fn discover(dir: &Path) -> Option<PathBuf> {
    discover_with(dir, &process_env)
}

pub fn discover_with(dir: &Path, env: EnvLookup<'_>) -> Option<PathBuf> {
    dir.ancestors().find_map(config_in).or_else(|| {
        user_config_dirs(env)
            .iter()
            .find_map(|user_dir| config_in(user_dir))
    })
}

fn config_in(dir: &Path) -> Option<PathBuf> {
    CONFIG_NAMES
        .iter()
        .map(|name| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn user_config_dirs(env: EnvLookup<'_>) -> Vec<PathBuf> {
    home_dir(env)
        .into_iter()
        .chain(platform_config_dir(env).map(|dir| dir.join("rustfmt")))
        .collect()
}

fn platform_config_dir(env: EnvLookup<'_>) -> Option<PathBuf> {
    if cfg!(windows) {
        return env("APPDATA")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute());
    }
    if cfg!(target_os = "macos") {
        return home_dir(env).map(|home| home.join("Library").join("Application Support"));
    }
    env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| home_dir(env).map(|home| home.join(".config")))
}

/// A string-valued top-level setting of a `rustfmt.toml`.
pub fn setting(config_file: &Path, key: &str) -> Option<String> {
    let source = fs::read_to_string(config_file).ok()?;
    let doc = source.parse::<toml_edit::DocumentMut>().ok()?;
    let item = doc.get(key)?;
    item.as_str()
        .map(str::to_owned)
        .or_else(|| item.as_integer().map(|value| value.to_string()))
}

/// `discover` and `setting` memoized per directory. One run asks the same
/// question for every file of a package, and the answer is a walk of syscalls.
#[derive(Debug, Default)]
pub struct Discovery {
    by_dir: HashMap<PathBuf, Option<PathBuf>>,
    settings: HashMap<(PathBuf, String), Option<String>>,
}

impl Discovery {
    pub fn new() -> Self {
        Self::default()
    }

    /// The configuration file rustfmt will read when it formats `file`.
    pub fn for_file(&mut self, file: &Path) -> Option<PathBuf> {
        let dir = file.parent().unwrap_or(Path::new("."));
        if let Some(found) = self.by_dir.get(dir) {
            return found.clone();
        }
        let found = discover(dir);
        self.by_dir.insert(dir.to_path_buf(), found.clone());
        found
    }

    /// What that file says about `key`, if anything.
    pub fn setting_for_file(&mut self, file: &Path, key: &str) -> Option<String> {
        let config = self.for_file(file)?;
        let cache_key = (config.clone(), key.to_owned());
        if let Some(found) = self.settings.get(&cache_key) {
            return found.clone();
        }
        let found = setting(&config, key);
        self.settings.insert(cache_key, found.clone());
        found
    }
}

/// Write `project` plus `overrides` into `dir` as one document, and return the
/// path to pass as `--config-path`.
///
/// The project's own file has to be carried across: `--config-path` replaces
/// rustfmt's discovery rather than adding to it, so a temporary file holding
/// only the overrides would silently drop every setting the repository made.
pub fn materialize(
    dir: &Path,
    project: Option<&Path>,
    overrides: impl Iterator<Item = (String, String)>,
) -> Result<PathBuf> {
    let overrides: Vec<(String, String)> = overrides.collect();
    let mut doc = match project {
        Some(path) => {
            let source = fs::read_to_string(path).map_err(|err| Error::io(path, err))?;
            source
                .parse::<toml_edit::DocumentMut>()
                .map_err(|err| Error::toml_parse(path, &source, &err))?
        }
        None => toml_edit::DocumentMut::new(),
    };

    // rustfmt roots `ignore` at the directory of the file it was read from and
    // panics on an input outside that root, so the entries of a project file
    // cannot be moved into a temporary one. Refusing says so; carrying them
    // across would abort rustfmt with an internal error instead.
    if let Some(path) = project
        && doc.get("ignore").is_some()
    {
        return Err(Error::ConflictingIgnore {
            path: path.to_path_buf(),
            option: overrides
                .first()
                .map_or_else(|| "…".to_string(), |(key, _)| key.clone()),
        });
    }

    for (key, value) in overrides {
        doc[key.as_str()] = toml_edit::value(config_value(&value));
    }

    let path = dir.join("rustfmt.toml");
    fs::write(&path, doc.to_string()).map_err(|err| Error::io(&path, err))?;
    Ok(path)
}

/// `--config` values are written in rustfmt's command-line spelling, where a
/// variant name is bare. Anything that is already a TOML value keeps its type;
/// everything else is the string it looks like.
fn config_value(raw: &str) -> toml_edit::Value {
    raw.parse::<toml_edit::Value>()
        .unwrap_or_else(|_| toml_edit::Value::from(raw))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs};

    use tempfile::tempdir;

    use super::*;

    fn env_of(pairs: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let owned: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.as_os_str().to_owned()))
            .collect();
        move |key: &str| {
            owned
                .iter()
                .find(|(held, _)| held == key)
                .map(|(_, value)| value.clone())
        }
    }

    fn home_key() -> &'static str {
        if cfg!(windows) { "USERPROFILE" } else { "HOME" }
    }

    fn platform_config_key() -> &'static str {
        if cfg!(windows) {
            "APPDATA"
        } else {
            "XDG_CONFIG_HOME"
        }
    }

    #[test]
    fn with_no_project_file_the_home_directory_is_asked() {
        let tree = tempdir().unwrap();
        let home = tempdir().unwrap();
        let config = tempdir().unwrap();
        fs::write(home.path().join(".rustfmt.toml"), "max_width = 40\n").unwrap();
        fs::create_dir_all(config.path().join("rustfmt")).unwrap();
        fs::write(
            config.path().join("rustfmt").join("rustfmt.toml"),
            "max_width = 50\n",
        )
        .unwrap();
        let env = env_of(&[
            (home_key(), home.path()),
            (platform_config_key(), config.path()),
        ]);

        assert_eq!(
            discover_with(tree.path(), &env),
            Some(home.path().join(".rustfmt.toml"))
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn with_nothing_at_home_the_platform_config_directory_is_asked() {
        let tree = tempdir().unwrap();
        let home = tempdir().unwrap();
        let config = tempdir().unwrap();
        let global = config.path().join("rustfmt").join("rustfmt.toml");
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::write(&global, "max_width = 40\n").unwrap();
        let xdg = env_of(&[("HOME", home.path()), ("XDG_CONFIG_HOME", config.path())]);

        fs::create_dir_all(home.path().join(".config").join("rustfmt")).unwrap();
        let dotted = home
            .path()
            .join(".config")
            .join("rustfmt")
            .join(".rustfmt.toml");
        fs::write(&dotted, "max_width = 60\n").unwrap();
        let without_xdg = env_of(&[("HOME", home.path())]);

        assert_eq!(discover_with(tree.path(), &xdg), Some(global));
        assert_eq!(discover_with(tree.path(), &without_xdg), Some(dotted));
    }

    #[test]
    fn a_project_file_outranks_every_user_level_one() {
        let tree = tempdir().unwrap();
        let home = tempdir().unwrap();
        fs::write(tree.path().join("rustfmt.toml"), "max_width = 80\n").unwrap();
        fs::write(home.path().join("rustfmt.toml"), "max_width = 40\n").unwrap();
        let env = env_of(&[(home_key(), home.path())]);

        assert_eq!(
            discover_with(tree.path(), &env),
            Some(tree.path().join("rustfmt.toml"))
        );
    }

    #[test]
    fn discovery_prefers_the_nearest_file_and_the_undotted_name() {
        let temp = tempdir().unwrap();
        let deep = temp.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(temp.path().join("rustfmt.toml"), "max_width = 80\n").unwrap();
        fs::write(deep.join(".rustfmt.toml"), "max_width = 70\n").unwrap();
        fs::write(deep.join("rustfmt.toml"), "max_width = 60\n").unwrap();

        assert_eq!(discover(&deep), Some(deep.join("rustfmt.toml")));
        assert_eq!(
            discover(&temp.path().join("a")),
            Some(temp.path().join("rustfmt.toml"))
        );
    }

    #[test]
    fn a_dotted_name_is_found_when_it_is_the_only_one() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join(".rustfmt.toml"), "edition = \"2018\"\n").unwrap();
        let found = discover(temp.path()).unwrap();
        assert_eq!(setting(&found, "edition").as_deref(), Some("2018"));
        assert_eq!(setting(&found, "max_width"), None);
    }

    #[test]
    fn an_integer_setting_reads_back_as_its_digits() {
        let temp = tempdir().unwrap();
        let config = temp.path().join("rustfmt.toml");
        fs::write(&config, "style_edition = 2024\n").unwrap();
        assert_eq!(setting(&config, "style_edition").as_deref(), Some("2024"));
    }

    #[test]
    fn discovery_answers_the_second_file_of_a_directory_without_walking_again() {
        let temp = tempdir().unwrap();
        let config = temp.path().join("rustfmt.toml");
        fs::write(&config, "max_width = 80\n").unwrap();
        let mut discovery = Discovery::new();

        assert_eq!(discovery.for_file(&temp.path().join("a.rs")), Some(config));
        fs::remove_file(temp.path().join("rustfmt.toml")).unwrap();
        assert!(discovery.for_file(&temp.path().join("b.rs")).is_some());
    }

    #[test]
    fn materializing_carries_the_project_settings_across() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("rustfmt.toml");
        fs::write(&project, "max_width = 80\nedition = \"2018\"\n").unwrap();
        let out = tempdir().unwrap();

        let merged = materialize(
            out.path(),
            Some(&project),
            [
                ("ignore".to_string(), "[\"a\", \"b\"]".to_string()),
                ("max_width".to_string(), "120".to_string()),
                ("group_imports".to_string(), "StdExternalCrate".to_string()),
            ]
            .into_iter(),
        )
        .unwrap();

        let text = fs::read_to_string(&merged).unwrap();
        assert!(text.contains("edition = \"2018\""), "{text}");
        assert!(text.contains("max_width = 120"), "{text}");
        assert!(text.contains("ignore = [\"a\", \"b\"]"), "{text}");
        assert!(
            text.contains("group_imports = \"StdExternalCrate\""),
            "{text}"
        );
    }

    #[test]
    fn materializing_without_a_project_file_writes_only_the_overrides() {
        let out = tempdir().unwrap();
        let merged = materialize(
            out.path(),
            None,
            [("ignore".to_string(), "[\"vendor\"]".to_string())].into_iter(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&merged).unwrap(),
            "ignore = [\"vendor\"]\n"
        );
    }

    #[test]
    fn a_project_ignore_list_cannot_be_carried_into_a_temporary_file() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("rustfmt.toml");
        fs::write(&project, "ignore = [\"vendor\"]\n").unwrap();
        let out = tempdir().unwrap();
        assert!(matches!(
            materialize(
                out.path(),
                Some(&project),
                [(
                    "skip_macro_invocations".to_string(),
                    "[\"a\", \"b\"]".to_string()
                )]
                .into_iter()
            ),
            Err(Error::ConflictingIgnore { .. })
        ));
    }

    #[test]
    fn an_unparseable_project_file_is_reported_rather_than_dropped() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("rustfmt.toml");
        fs::write(&project, "max_width = \n").unwrap();
        let out = tempdir().unwrap();
        assert!(materialize(out.path(), Some(&project), std::iter::empty()).is_err());
    }
}
