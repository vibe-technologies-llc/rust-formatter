use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode, ExitStatus, Output, Stdio},
};

use serde::Deserialize;

use crate::error::{Error, Result};

pub const BINARY: &str = "rust-formatter";

const ARTIFACT: &str = "compiler-artifact";
const BIN_KIND: &str = "bin";

pub struct Artifact {
    pub name: String,
    pub executable: PathBuf,
    pub test: bool,
    pub bin: bool,
}

#[derive(Deserialize)]
struct Message {
    reason: String,
    executable: Option<PathBuf>,
    profile: Option<Profile>,
    target: Option<Target>,
}

#[derive(Deserialize)]
struct Profile {
    test: bool,
}

#[derive(Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
}

pub fn repo_root() -> &'static Path {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.parent().unwrap_or(manifest_dir)
}

pub fn cargo() -> Command {
    Command::new("cargo")
}

pub fn describe(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn status(command: &mut Command) -> Result<ExitStatus> {
    command.status().map_err(|source| Error::Spawn {
        command: describe(command),
        source,
    })
}

pub fn succeed(command: &mut Command) -> Result<()> {
    let status = status(command)?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Failed {
            command: describe(command),
            status,
        })
    }
}

pub fn output(command: &mut Command) -> Result<Output> {
    command.output().map_err(|source| Error::Spawn {
        command: describe(command),
        source,
    })
}

pub fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}

pub fn artifacts(command: &mut Command) -> Result<Vec<Artifact>> {
    command
        .arg("--message-format=json-render-diagnostics")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let output = output(command)?;
    if !output.status.success() {
        return Err(Error::Failed {
            command: describe(command),
            status: output.status,
        });
    }

    let mut artifacts = Vec::new();
    for line in output.stdout.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let message: Message =
            serde_json::from_slice(line).map_err(|source| Error::CargoMessage {
                command: describe(command),
                source,
            })?;
        let (Some(executable), Some(profile), Some(target)) =
            (message.executable, message.profile, message.target)
        else {
            continue;
        };
        if message.reason != ARTIFACT {
            continue;
        }
        artifacts.push(Artifact {
            bin: target.kind.iter().any(|kind| kind == BIN_KIND),
            name: target.name,
            executable,
            test: profile.test,
        });
    }
    Ok(artifacts)
}

pub fn release_binary(manifest: &Path, target_dir: Option<&Path>) -> Result<PathBuf> {
    let mut command = cargo();
    command
        .args([
            "build",
            "--release",
            "--quiet",
            "--bin",
            BINARY,
            "--manifest-path",
        ])
        .arg(manifest);
    if let Some(dir) = target_dir {
        command.arg("--target-dir").arg(dir);
    }

    artifacts(&mut command)?
        .into_iter()
        .find(|artifact| artifact.bin && !artifact.test && artifact.name == BINARY)
        .map(|artifact| artifact.executable)
        .ok_or_else(|| Error::BinaryMissing {
            name: BINARY.to_owned(),
        })
}
