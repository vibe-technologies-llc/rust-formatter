//! The pre-commit hook, for a repository that does not use the pre-commit
//! framework.
//!
//! The hook is a shell script rather than anything cleverer because git runs it
//! with `sh`, and it has to keep working when the binary that wrote it has been
//! replaced by a newer one -- so it resolves `rust-formatter` on `PATH` first
//! and only falls back to the absolute path it was installed from.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use crate::{Result, error::Error, git};

/// The hook git runs before a commit is written.
const NAME: &str = "pre-commit";

/// Where a hook that was not ours is moved to by `--force`.
pub const BACKUP: &str = "pre-commit.rust-formatter.bak";

/// The line that identifies a hook as ours, and says what it was written to do.
///
/// The prefix must never change: a hook written by an older build is recognised
/// by it, which is what makes a re-install an upgrade rather than a refusal, and
/// what stops `uninstall` from deleting somebody else's script. The `version`
/// beside it is what tells an old hook from a current one when neither the mode
/// nor the marker changed but the body did.
const MARKER: &str = "# rust-formatter-hook:";

/// The body this build writes. Raise it whenever the script changes, so an
/// install over an older hook rewrites it instead of reporting it as current.
const VERSION: u32 = 1;

/// What the hook asks of the formatter.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Format what is staged and stage the result, so the commit carries it.
    Restage,
    /// Leave the commit alone and refuse it, so the author fixes it themselves.
    Check,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Restage => "restage",
            Self::Check => "check",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "restage" => Some(Self::Restage),
            "check" => Some(Self::Check),
            _ => None,
        }
    }
}

/// What is sitting at the hook path already.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing is installed.
    Absent,
    /// Ours: the body version it was written by, and what it does.
    Ours { version: u32, mode: Mode },
    /// Somebody else's hook, or one edited past recognition.
    Foreign,
}

/// The hook path, and what is there now.
#[derive(Clone, Debug)]
pub struct Located {
    pub directory: PathBuf,
    pub path: PathBuf,
    pub state: State,
}

/// What an install did, so the caller can say so precisely.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    /// There was nothing there.
    Fresh,
    /// Ours was there, already saying exactly this.
    Unchanged,
    /// Ours was there in another mode, or from an older build.
    Upgraded { version: u32, mode: Mode },
    /// A foreign hook was moved aside first.
    Forced,
}

/// What an uninstall did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Uninstalled {
    Removed(Mode),
    /// Removed, and the hook `--force` had moved aside was put back.
    Restored(Mode),
    /// Nothing of ours was there.
    Absent,
}

/// Why a hook was left alone. Refusals are values rather than errors: none of
/// them can reach the `--message-format json` envelope, and a permanent public
/// diagnostic code is the wrong price for "that hook is somebody else's".
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A hook is there that this tool did not write.
    Foreign,
    /// `--force` would have to overwrite a backup that is already there.
    Backup,
}

pub fn locate(cwd: &Path) -> Result<Located> {
    let directory = git::hooks_dir(cwd)?;
    let path = directory.join(NAME);
    let state = state(&path)?;
    Ok(Located {
        directory,
        path,
        state,
    })
}

fn state(path: &Path) -> Result<State> {
    let text = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(State::Absent),
        Err(err) => return Err(Error::io(path, err)),
    };
    // A hook is a script, but nothing guarantees it is UTF-8, and a script we
    // cannot read is certainly not one we wrote.
    let Ok(text) = std::str::from_utf8(&text) else {
        return Ok(State::Foreign);
    };
    Ok(
        header(text).map_or(State::Foreign, |(version, mode)| State::Ours {
            version,
            mode,
        }),
    )
}

/// The marker line of a hook of ours, read back, or `None` if this is not one.
///
/// Recognition keys off the marker rather than off the `exec` line, so a hook
/// somebody edited is still upgraded rather than refused -- and a field this
/// build cannot read falls back rather than making the hook a stranger.
fn header(text: &str) -> Option<(u32, Mode)> {
    let line = text.lines().find(|line| line.starts_with(MARKER))?;
    let fields = line[MARKER.len()..].split_whitespace().filter_map(|field| {
        let (key, value) = field.split_once('=')?;
        Some((key, value))
    });
    let mut version = 0;
    let mut mode = Mode::Restage;
    for (key, value) in fields {
        match key {
            "version" => version = value.parse().unwrap_or(0),
            "mode" => mode = Mode::parse(value).unwrap_or(mode),
            _ => {}
        }
    }
    Some((version, mode))
}

/// The hook script.
///
/// `fallback` is the binary that installed the hook, used only when nothing
/// answers to `rust-formatter` on `PATH` -- so an upgrade is picked up, and a
/// hook run from a bare `git commit` in a login-less environment still works.
/// A missing formatter *fails* the commit: a check that has quietly stopped
/// checking is worse than one that says it cannot run.
fn script(mode: Mode, fallback: &Path) -> String {
    let preamble = format!(
        "#!/bin/sh\n\
         {MARKER} version={VERSION} mode={mode}\n\
         #\n\
         # Written by `rust-formatter hook install`; remove it with\n\
         # `rust-formatter hook uninstall`. Editing this file is fine, but the\n\
         # line above is how it is recognised and upgraded -- keep it.\n\
         set -eu\n\
         \n\
         if command -v rust-formatter >/dev/null 2>&1; then\n\
         \tformatter=rust-formatter\n\
         elif [ -x {fallback} ]; then\n\
         \tformatter={fallback}\n\
         else\n\
         \tprintf 'pre-commit: rust-formatter is not on PATH, and %s is gone.\\n' {fallback} >&2\n\
         \tprintf 'pre-commit: install it again, or run `rust-formatter hook uninstall`.\\n' >&2\n\
         \texit 1\n\
         fi\n\
         \n",
        mode = mode.name(),
        fallback = quote(fallback),
    );
    // `set -e` does not apply to the condition of an `if`, which is what lets
    // the checking hook add its own advice instead of exec'ing and vanishing.
    let action = match mode {
        Mode::Restage => "exec \"$formatter\" --staged --restage\n".to_string(),
        Mode::Check => "if \"$formatter\" --staged --check; then\n\
                        \texit 0\n\
                        fi\n\
                        printf 'pre-commit: run `rust-formatter --staged --restage` and stage the result.\\n' >&2\n\
                        exit 1\n"
            .to_string(),
    };
    preamble + &action
}

/// A path as one `sh` word. Single quotes take everything literally, so only a
/// single quote itself has to be broken out of them.
fn quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Write the hook, refusing to destroy one this tool did not write.
pub fn install(
    cwd: &Path,
    mode: Mode,
    force: bool,
    fallback: &Path,
) -> Result<(Located, std::result::Result<Installed, Refused>)> {
    let located = locate(cwd)?;
    let body = script(mode, fallback);
    let outcome = match located.state {
        State::Foreign if !force => return Ok((located, Err(Refused::Foreign))),
        State::Foreign => {
            let backup = located.directory.join(BACKUP);
            // Never overwrite a backup: the one already there is somebody's
            // only copy of a hook this tool moved aside once before.
            if backup.exists() {
                return Ok((located, Err(Refused::Backup)));
            }
            // A copy rather than a rename, so a failed write leaves the hook
            // that is there working.
            fs::copy(&located.path, &backup).map_err(|err| Error::io(&backup, err))?;
            Installed::Forced
        }
        State::Ours { version, mode: had } => {
            let existing = fs::read(&located.path).map_err(|err| Error::io(&located.path, err))?;
            if existing == body.as_bytes() {
                Installed::Unchanged
            } else {
                Installed::Upgraded { version, mode: had }
            }
        }
        State::Absent => Installed::Fresh,
    };

    if outcome != Installed::Unchanged {
        write(&located.directory, &located.path, &body)?;
    }
    Ok((located, Ok(outcome)))
}

/// Write `contents` to `path` through a temporary file in the same directory, so
/// git can never find a half-written hook -- and set the executable bit before
/// the rename rather than after, so it can never find one it refuses to run.
fn write(directory: &Path, path: &Path, contents: &str) -> Result<()> {
    fs::create_dir_all(directory).map_err(|err| Error::io(directory, err))?;
    let mut file =
        tempfile::NamedTempFile::new_in(directory).map_err(|err| Error::io(directory, err))?;
    file.write_all(contents.as_bytes())
        .map_err(|err| Error::io(path, err))?;
    file.flush().map_err(|err| Error::io(path, err))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|err| Error::io(path, err))?;
    }

    file.persist(path)
        .map_err(|err| Error::io(path, err.error))?;
    Ok(())
}

/// Remove the hook, if it is ours.
pub fn uninstall(cwd: &Path) -> Result<(Located, std::result::Result<Uninstalled, Refused>)> {
    let located = locate(cwd)?;
    let outcome = match located.state {
        State::Foreign => return Ok((located, Err(Refused::Foreign))),
        State::Absent => Uninstalled::Absent,
        State::Ours { mode, .. } => {
            fs::remove_file(&located.path).map_err(|err| Error::io(&located.path, err))?;
            let backup = located.directory.join(BACKUP);
            if backup.exists() {
                fs::rename(&backup, &located.path).map_err(|err| Error::io(&backup, err))?;
                Uninstalled::Restored(mode)
            } else {
                Uninstalled::Removed(mode)
            }
        }
    };
    Ok((located, Ok(outcome)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hook_we_wrote_is_recognised_with_its_mode() {
        for mode in [Mode::Restage, Mode::Check] {
            let text = script(mode, Path::new("/usr/bin/rust-formatter"));
            assert_eq!(header(&text), Some((VERSION, mode)), "{mode:?}");
        }
    }

    /// A field this build cannot read must not turn a hook of ours into a
    /// stranger: an upgrade has to be able to replace what an older build left.
    #[test]
    fn an_unreadable_field_still_leaves_the_hook_ours() {
        let bare = format!("#!/bin/sh\n{MARKER}\nexec rust-formatter --staged --restage\n");
        assert_eq!(header(&bare), Some((0, Mode::Restage)));
        let future = format!("#!/bin/sh\n{MARKER} version=99 mode=elsewhere shape=round\n");
        assert_eq!(header(&future), Some((99, Mode::Restage)));
    }

    #[test]
    fn somebody_elses_hook_is_not_ours() {
        assert_eq!(header("#!/bin/sh\nexec cargo fmt --check\n"), None);
        assert_eq!(header(""), None);
    }

    #[test]
    fn the_script_does_what_the_mode_it_names_says() {
        let restage = script(Mode::Restage, Path::new("/opt/rf"));
        assert!(restage.contains("mode=restage"), "{restage}");
        assert!(
            restage.contains("exec \"$formatter\" --staged --restage\n"),
            "{restage}"
        );
        assert!(!restage.contains("--check"), "{restage}");

        let check = script(Mode::Check, Path::new("/opt/rf"));
        assert!(check.contains("mode=check"), "{check}");
        assert!(
            check.contains("if \"$formatter\" --staged --check; then"),
            "{check}"
        );
        // The advice it prints names `--restage`; what it *runs* must not.
        assert!(!check.contains("exec"), "{check}");
    }

    /// The script is assembled from a Rust string, where a line continuation
    /// eats the indentation that follows it -- so the bytes are checked rather
    /// than assumed.
    #[test]
    fn the_script_is_indented_the_way_a_shell_script_is() {
        for mode in [Mode::Restage, Mode::Check] {
            let text = script(mode, Path::new("/opt/rf"));
            assert!(text.starts_with("#!/bin/sh\n"), "{text}");
            assert!(text.ends_with('\n'), "{text}");
            for line in text.lines() {
                assert!(
                    !line.starts_with("    ") && !line.starts_with(" \t"),
                    "{mode:?}: {line:?} is indented with spaces:\n{text}"
                );
            }
        }
    }

    /// A formatter that has been uninstalled must fail the commit rather than
    /// let it through unchecked.
    #[test]
    fn a_missing_formatter_fails_the_commit() {
        let text = script(Mode::Restage, Path::new("/opt/rf"));
        assert!(text.contains("elif [ -x '/opt/rf' ]; then"), "{text}");
        assert!(text.contains("exit 1"), "{text}");
    }

    /// A path with a quote or a space in it has to survive being pasted into a
    /// shell script, or the hook fails for everyone whose home directory is
    /// spelled unusually.
    #[test]
    fn the_fallback_path_is_one_shell_word() {
        assert_eq!(quote(Path::new("/home/a b/rf")), "'/home/a b/rf'");
        assert_eq!(quote(Path::new("/home/it's/rf")), r"'/home/it'\''s/rf'");
        let text = script(Mode::Restage, Path::new("/home/it's/rf"));
        assert!(text.contains(r"'/home/it'\''s/rf'"), "{text}");
    }
}
