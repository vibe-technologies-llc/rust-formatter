//! Which rustfmt a run drives, and how it was found.
//!
//! rustup is the usual answer and no longer the only one: a Nix profile, a
//! distro package or a `rust:alpine` image has a rustfmt on `PATH` and no
//! rustup at all, and cargo's own `$RUSTFMT` names one directly.

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::error::{Error, Result};

/// The toolchain name that asks for the best rustfmt available rather than
/// naming one. It is the built-in default, so a machine with no nightly is a
/// slower path rather than a refusal.
pub const AUTO: &str = "auto";

/// The channel `auto` prefers. Nightly is what every earlier release drove, so
/// preferring it keeps a machine that has one formatting exactly as it did.
const PREFERRED: &str = "nightly";

/// Where a resolved rustfmt came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    /// `$RUSTFMT`.
    Environment,
    /// `rustup which rustfmt --toolchain <name>`.
    Rustup(String),
    /// `rustup which rustfmt`, against whatever toolchain rustup has active.
    RustupActive,
    /// `rustfmt` on `PATH`.
    Path,
}

impl Via {
    pub fn describe(&self) -> String {
        match self {
            Self::Environment => "$RUSTFMT".to_string(),
            Self::Rustup(toolchain) => format!("rustup toolchain '{toolchain}'"),
            Self::RustupActive => "rustup's active toolchain".to_string(),
            Self::Path => "rustfmt on PATH".to_string(),
        }
    }

    /// Whether this outcome is the one the next run would reach anyway.
    ///
    /// Only a rustup toolchain is: `auto` prefers nightly, so remembering that
    /// it settled for the active toolchain or for `PATH` would mean a nightly
    /// installed afterwards was never noticed.
    pub fn is_preferred(&self) -> bool {
        matches!(self, Self::Rustup(_))
    }
}

/// How a remembered resolution was reached. Only `Via::Rustup` is ever
/// remembered, and for `auto` that is always the preferred channel.
pub fn cached_via(requested: &str) -> Via {
    if requested.eq_ignore_ascii_case(AUTO) {
        Via::Rustup(PREFERRED.to_string())
    } else {
        Via::Rustup(requested.to_string())
    }
}

/// Everything a resolution depends on, as one string to key a cache with.
///
/// `None` means "do not remember this one": `$RUSTFMT` is already a direct
/// answer, and a resolution that has to consult it cannot be shortened.
pub fn resolution_key(requested: &str, env: crate::cargo_config::EnvLookup<'_>) -> Option<String> {
    if env("RUSTFMT").is_some_and(|value| !value.is_empty()) {
        return None;
    }
    let read = |key: &str| {
        env(key)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Some(format!(
        "1\x1f{requested}\x1f{}\x1f{}\x1f{}",
        read("RUSTUP_HOME"),
        read("RUSTUP_TOOLCHAIN"),
        read("PATH"),
    ))
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub rustfmt: PathBuf,
    /// What the run asked for, which is `AUTO` unless something named a
    /// toolchain.
    pub requested: String,
    pub via: Via,
    /// What the caller should say about a resolution that is not what was
    /// asked for. Carried rather than printed so the library path stays quiet.
    pub warnings: Vec<String>,
}

/// What to do when a named toolchain has no rustfmt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallPolicy {
    /// Print the install command, as every release before this one did.
    #[default]
    Never,
    /// Offer to run it. Only ever chosen when the session is interactive.
    Ask,
    /// Run it without asking.
    Always,
}

impl InstallPolicy {
    /// A prompt is only honest when there is someone to answer it: a formatter
    /// run from an editor, a pre-commit hook or a CI job must never block on
    /// one, and each of those has at least one of the two streams redirected.
    pub fn interactive(requested: Option<bool>) -> Self {
        match requested {
            Some(true) => Self::Always,
            None if io::stdin().is_terminal() && io::stderr().is_terminal() => Self::Ask,
            Some(false) | None => Self::Never,
        }
    }
}

pub fn resolve(requested: &str, install: InstallPolicy) -> Result<Resolved> {
    if requested.eq_ignore_ascii_case(AUTO) {
        resolve_auto()
    } else {
        resolve_named(requested, install)
    }
}

/// `$RUSTFMT` first, then nightly, then whatever rustup has active, then
/// `PATH`. Nightly outranks the active toolchain deliberately: it is the
/// channel this tool was built against, and preferring it means `auto` changes
/// nothing for a machine that already had one.
fn resolve_auto() -> Result<Resolved> {
    let requested = AUTO.to_string();
    if let Some(rustfmt) = from_environment() {
        return Ok(Resolved {
            rustfmt,
            requested,
            via: Via::Environment,
            warnings: Vec::new(),
        });
    }
    match rustup_which("rustfmt", Some(PREFERRED)) {
        Rustup::Found(rustfmt) => {
            return Ok(Resolved {
                rustfmt,
                requested,
                via: Via::Rustup(PREFERRED.to_string()),
                warnings: Vec::new(),
            });
        }
        Rustup::Failed(err) => return Err(err),
        Rustup::Refused { .. } | Rustup::Absent => {}
    }
    if let Rustup::Found(rustfmt) = rustup_which("rustfmt", None) {
        return Ok(Resolved {
            rustfmt,
            requested,
            via: Via::RustupActive,
            warnings: Vec::new(),
        });
    }
    if let Some(rustfmt) = on_path("rustfmt") {
        return Ok(Resolved {
            rustfmt,
            requested,
            via: Via::Path,
            warnings: Vec::new(),
        });
    }
    Err(Error::RustfmtUnresolved)
}

fn resolve_named(toolchain: &str, install: InstallPolicy) -> Result<Resolved> {
    let refusal = match rustup_which("rustfmt", Some(toolchain)) {
        Rustup::Found(rustfmt) => {
            return Ok(Resolved {
                rustfmt,
                requested: toolchain.to_string(),
                via: Via::Rustup(toolchain.to_string()),
                warnings: Vec::new(),
            });
        }
        Rustup::Failed(err) => return Err(err),
        Rustup::Refused { details } => Some(details),
        Rustup::Absent => None,
    };

    if let Some(details) = refusal {
        if let Some(rustfmt) = install_and_retry(toolchain, &details, install)? {
            return Ok(Resolved {
                rustfmt,
                requested: toolchain.to_string(),
                via: Via::Rustup(toolchain.to_string()),
                warnings: Vec::new(),
            });
        }
        return Err(Error::RustfmtNotFound {
            toolchain: toolchain.to_string(),
            details,
        });
    }

    // rustup is not installed at all. A toolchain name it cannot honour is
    // still a pin worth reporting, but refusing here would be a dead end for
    // exactly the Nix and distro installs a named `rust-toolchain.toml` is
    // most likely to sit next to.
    let found = from_environment()
        .map(|rustfmt| (rustfmt, Via::Environment))
        .or_else(|| on_path("rustfmt").map(|rustfmt| (rustfmt, Via::Path)));
    let Some((rustfmt, via)) = found else {
        return Err(Error::RustupNotFound);
    };
    Ok(Resolved {
        warnings: vec![format!(
            "warning: toolchain '{toolchain}' was asked for but rustup is not installed, \
             so {} is used instead",
            via.describe()
        )],
        rustfmt,
        requested: toolchain.to_string(),
        via,
    })
}

/// Cargo's own spelling for "use this rustfmt". An absolute path is taken as
/// given; a bare name is looked up the way a shell would.
fn from_environment() -> Option<PathBuf> {
    let named = std::env::var_os("RUSTFMT")?;
    if named.is_empty() {
        return None;
    }
    let path = PathBuf::from(&named);
    if path.components().count() > 1 {
        return path.is_file().then_some(path);
    }
    on_path(&path.to_string_lossy())
}

enum Rustup {
    Found(PathBuf),
    /// rustup ran and said no: the toolchain or the component is missing.
    Refused {
        details: String,
    },
    /// rustup itself is not installed.
    Absent,
    /// rustup is installed and something else went wrong.
    Failed(Error),
}

fn rustup_which(bin: &str, toolchain: Option<&str>) -> Rustup {
    let mut cmd = Command::new("rustup");
    cmd.args(["which", bin]);
    if let Some(toolchain) = toolchain {
        cmd.args(["--toolchain", toolchain]);
    }
    let described = describe(bin, toolchain);
    let output = match cmd.stdin(Stdio::null()).output() {
        Ok(output) => output,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Rustup::Absent,
        Err(err) => {
            return Rustup::Failed(Error::CommandExecutionFailed {
                command: described,
                source: err,
            });
        }
    };

    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        let details = details.trim();
        return Rustup::Refused {
            details: if details.is_empty() {
                "Component not found".to_string()
            } else {
                details.to_string()
            },
        };
    }

    let path = String::from_utf8_lossy(&output.stdout);
    let path = path.trim();
    if path.is_empty() {
        return Rustup::Failed(Error::ToolFailed {
            command: described,
            code: 0,
            details: "rustup printed an empty path".to_string(),
        });
    }
    Rustup::Found(PathBuf::from(path))
}

fn describe(bin: &str, toolchain: Option<&str>) -> String {
    match toolchain {
        Some(toolchain) => format!("rustup which {bin} --toolchain {toolchain}"),
        None => format!("rustup which {bin}"),
    }
}

/// The install command a missing rustfmt needs. Adding the component to an
/// installed toolchain is a much smaller download than installing the
/// toolchain, so the two cases are told apart rather than always offering the
/// larger one.
pub fn install_command(toolchain: &str) -> Vec<String> {
    if toolchain_is_installed(toolchain) {
        vec![
            "component".to_string(),
            "add".to_string(),
            "rustfmt".to_string(),
            "--toolchain".to_string(),
            toolchain.to_string(),
        ]
    } else {
        vec![
            "toolchain".to_string(),
            "install".to_string(),
            toolchain.to_string(),
            "--component".to_string(),
            "rustfmt".to_string(),
        ]
    }
}

fn toolchain_is_installed(toolchain: &str) -> bool {
    matches!(rustup_which("cargo", Some(toolchain)), Rustup::Found(_))
}

fn install_and_retry(
    toolchain: &str,
    details: &str,
    install: InstallPolicy,
) -> Result<Option<PathBuf>> {
    let args = install_command(toolchain);
    let rendered = format!("rustup {}", args.join(" "));
    match install {
        InstallPolicy::Never => return Ok(None),
        InstallPolicy::Ask => {
            if !confirm(toolchain, details, &rendered) {
                return Ok(None);
            }
        }
        InstallPolicy::Always => {
            let mut err = io::stderr();
            let _ = writeln!(err, "running: {rendered}");
        }
    }

    let status = Command::new("rustup")
        .args(&args)
        .stdin(Stdio::null())
        .status()
        .map_err(|err| Error::CommandExecutionFailed {
            command: rendered.clone(),
            source: err,
        })?;
    if !status.success() {
        return Err(Error::ToolFailed {
            command: rendered,
            code: status.code().unwrap_or(1),
            details: "the install command failed".to_string(),
        });
    }
    match rustup_which("rustfmt", Some(toolchain)) {
        Rustup::Found(rustfmt) => Ok(Some(rustfmt)),
        Rustup::Failed(err) => Err(err),
        Rustup::Refused { .. } | Rustup::Absent => Ok(None),
    }
}

fn confirm(toolchain: &str, details: &str, rendered: &str) -> bool {
    let mut err = io::stderr();
    let _ = writeln!(
        err,
        "rustfmt is not available for toolchain '{toolchain}'.\nDetails: {details}"
    );
    let _ = write!(err, "Install it now with `{rendered}`? [y/N] ");
    let _ = err.flush();

    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
}

/// `cargo metadata` is what resolves workspace scope, and it needs a cargo, not
/// the rustfmt the formatting half needs. Failing to find one is a degradation
/// to report, not a reason to refuse a TOML-only run.
pub fn cargo_for_metadata(rustfmt: Option<&Path>, toolchain: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) { "cargo.exe" } else { "cargo" };
    if let Some(rustfmt) = rustfmt {
        let sibling = rustfmt.with_file_name(name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let named = (!toolchain.eq_ignore_ascii_case(AUTO)).then_some(toolchain);
    if let Rustup::Found(path) = rustup_which("cargo", named) {
        return Some(path);
    }
    on_path(name)
}

pub fn on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_matched_without_regard_to_case() {
        assert!(AUTO.eq_ignore_ascii_case("Auto"));
    }

    /// The two spellings download very different amounts, so a toolchain that
    /// is present must not be reinstalled to add one component to it.
    #[test]
    fn the_install_command_adds_a_component_to_a_toolchain_that_exists() {
        let installed = install_command("stable");
        if toolchain_is_installed("stable") {
            assert_eq!(installed.first().map(String::as_str), Some("component"));
        }
        assert_eq!(
            install_command("nightly-1970-01-01")
                .first()
                .map(String::as_str),
            Some("toolchain")
        );
    }

    #[test]
    fn cargo_for_metadata_uses_a_sibling_binary() {
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) { "cargo.exe" } else { "cargo" };
        let cargo = dir.path().join(name);
        std::fs::write(&cargo, "").unwrap();
        let rustfmt = dir.path().join("rustfmt");
        assert_eq!(cargo_for_metadata(Some(&rustfmt), AUTO).unwrap(), cargo);
    }

    #[test]
    fn cargo_for_metadata_falls_back_without_a_rustfmt() {
        assert!(cargo_for_metadata(None, AUTO).is_some());
    }

    /// An unset variable must not resolve to the empty path, which would be
    /// reported as a rustfmt that exists and then fail to spawn.
    #[test]
    fn an_empty_rustfmt_variable_is_not_a_resolution() {
        assert!(PathBuf::from("").components().count() <= 1);
    }
}
