//! The `rust-toolchain.toml` a repository pins its compiler with.
//!
//! rustup and cargo both read it, so a checkout that carries one has already
//! said which toolchain its tooling should use. Reading it here makes that
//! answer reach rustfmt as well, which is the difference between a pinned
//! repository formatting reproducibly and formatting with whatever nightly the
//! machine happens to have.

use std::path::{Path, PathBuf};

use crate::detector;

/// Both spellings rustup accepts, nearest first. The bare name is the legacy
/// file, whose whole contents may be a channel rather than TOML.
const NAMES: [&str; 2] = ["rust-toolchain.toml", "rust-toolchain"];

/// The channel a repository pins, and the file that pinned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub channel: String,
    pub path: PathBuf,
}

/// The pin in force for `start`.
///
/// The search stops where the settings file search stops -- at the workspace
/// root, else at the repository boundary -- rather than at the filesystem root
/// rustup would walk to. A checkout must not inherit the toolchain of whatever
/// directory it happens to have been unpacked into.
pub fn discover(start: &Path) -> Option<Pin> {
    let stop =
        detector::find_cargo_manifest(start).map(|manifest| detector::workspace_root(&manifest));
    for dir in detector::project_ancestors(start) {
        for name in NAMES {
            let candidate = dir.join(name);
            if let Some(channel) = read(&candidate) {
                return Some(Pin {
                    channel,
                    path: candidate,
                });
            }
        }
        if stop.as_deref() == Some(dir) {
            break;
        }
    }
    None
}

/// A file that does not parse, names no channel, or names a `path` toolchain
/// contributes nothing: rustup would report it, and a formatter that refused to
/// run over it would be reporting someone else's problem.
fn read(path: &Path) -> Option<String> {
    let source = std::fs::read_to_string(path).ok()?;
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(document) = trimmed.parse::<toml_edit::DocumentMut>()
        && let Some(table) = document.get("toolchain")
    {
        return table
            .get("channel")
            .and_then(|item| item.as_str())
            .map(str::trim)
            .filter(|channel| !channel.is_empty())
            .map(str::to_owned);
    }

    if is_legacy_spelling(path) {
        bare_channel(trimmed)
    } else {
        None
    }
}

fn is_legacy_spelling(path: &Path) -> bool {
    path.extension().is_none()
}

fn bare_channel(source: &str) -> Option<String> {
    let channel = source.lines().next()?.trim();
    (!channel.is_empty() && !channel.starts_with('#') && !channel.contains(char::is_whitespace))
        .then(|| channel.to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn at(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn the_channel_is_read_out_of_the_toolchain_table() {
        let temp = tempfile::tempdir().unwrap();
        at(
            temp.path(),
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"nightly-2026-06-01\"\ncomponents = [\"rustfmt\"]\n",
        );
        let found = discover(temp.path()).unwrap();
        assert_eq!(found.channel, "nightly-2026-06-01");
        assert_eq!(found.path, temp.path().join("rust-toolchain.toml"));
    }

    /// The file predates the TOML one and is still what many repositories
    /// carry, so a bare channel has to be understood as one.
    #[test]
    fn a_legacy_file_is_a_bare_channel_name() {
        let temp = tempfile::tempdir().unwrap();
        at(temp.path(), "rust-toolchain", "stable\n");
        assert_eq!(discover(temp.path()).unwrap().channel, "stable");
    }

    #[test]
    fn the_toml_spelling_is_preferred_over_the_legacy_one() {
        let temp = tempfile::tempdir().unwrap();
        at(temp.path(), "rust-toolchain", "stable\n");
        at(
            temp.path(),
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"beta\"\n",
        );
        assert_eq!(discover(temp.path()).unwrap().channel, "beta");
    }

    /// rustup reports its own malformed file; a formatter that refused to run
    /// over one would be reporting someone else's problem twice.
    #[test]
    fn a_file_with_no_channel_contributes_nothing() {
        let temp = tempfile::tempdir().unwrap();
        at(
            temp.path(),
            "rust-toolchain.toml",
            "[toolchain]\ncomponents = [\"rustfmt\"]\n",
        );
        assert!(discover(temp.path()).is_none());

        at(temp.path(), "rust-toolchain.toml", "this is not = = toml\n");
        assert!(discover(temp.path()).is_none());

        at(temp.path(), "rust-toolchain.toml", "   \n");
        assert!(discover(temp.path()).is_none());
    }

    #[test]
    fn an_unparseable_toml_file_is_not_read_as_a_bare_channel() {
        let temp = tempfile::tempdir().unwrap();
        at(
            temp.path(),
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"nightly\n",
        );
        assert!(discover(temp.path()).is_none());

        at(temp.path(), "rust-toolchain.toml", "nightly\n");
        assert!(discover(temp.path()).is_none());
    }

    #[test]
    fn a_file_above_the_start_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("crates").join("member");
        fs::create_dir_all(&nested).unwrap();
        at(
            temp.path(),
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"nightly\"\n",
        );
        assert_eq!(discover(&nested).unwrap().channel, "nightly");
    }
}
