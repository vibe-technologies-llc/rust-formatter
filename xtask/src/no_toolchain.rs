use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use crate::{
    cargo,
    error::{Error, Result},
};

const FAILING_TOOLS: [&str; 2] = ["rustup", "rustfmt"];
const SYSTEM_PATH: [&str; 2] = ["/usr/bin", "/bin"];
const TEST_THREADS: &str = "8";
const FAILURES_HEADING: &str = "failures:";
const SHOWN_FAILURE_LINES: usize = 20;

pub fn run() -> Result<ExitCode> {
    let root = cargo::repo_root();
    let artifacts = cargo::artifacts(
        cargo::cargo()
            .current_dir(root)
            .args(["test", "--locked", "--no-run"]),
    )?;

    let mut tests: Vec<&Path> = artifacts
        .iter()
        .filter(|artifact| artifact.test)
        .map(|artifact| artifact.executable.as_path())
        .collect();

    tests.sort_unstable();
    tests.dedup();
    if tests.is_empty() {
        return Err(Error::NoTestBinaries);
    }

    let binaries: Vec<(String, &Path)> = artifacts
        .iter()
        .filter(|artifact| artifact.bin && !artifact.test)
        .map(|artifact| {
            (
                format!("CARGO_BIN_EXE_{}", artifact.name),
                artifact.executable.as_path(),
            )
        })
        .collect();

    let shim = tempfile::tempdir().map_err(Error::io(&env::temp_dir()))?;

    for tool in FAILING_TOOLS {
        write_failing_tool(&shim.path().join(tool))?;
    }

    let search_path = env::join_paths(
        std::iter::once(shim.path().to_owned()).chain(SYSTEM_PATH.map(PathBuf::from)),
    )
    .map_err(Error::SearchPath)?;

    let mut failed = false;

    for test in &tests {
        let output = cargo::output(
            Command::new(test)
                .args(["--test-threads", TEST_THREADS])
                .current_dir(root)
                .env_remove("RUSTFMT")
                .env("PATH", &search_path)
                .envs(binaries.iter().map(|(name, path)| (name, path))),
        )?;
        let name = test
            .file_name()
            .unwrap_or(test.as_os_str())
            .to_string_lossy();

        if output.status.success() {
            println!("    {name} skipped cleanly");
            continue;
        }

        failed = true;
        eprintln!("FAIL: {name} does not skip without a toolchain");

        let report = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for line in report
            .lines()
            .skip_while(|line| *line != FAILURES_HEADING)
            .take(SHOWN_FAILURE_LINES)
        {
            eprintln!("{line}");
        }
    }

    if failed {
        eprintln!("\nGATE INCOMPLETE ({} binaries)", tests.len());
        return Ok(ExitCode::FAILURE);
    }
    println!(
        "\nevery one of {} test binaries skips without a toolchain",
        tests.len()
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(unix)]
fn write_failing_tool(path: &Path) -> Result<()> {
    use std::{fs, os::unix::fs::PermissionsExt};

    const SCRIPT: &str = "#!/bin/sh\nexit 127\n";
    const EXECUTABLE: u32 = 0o755;

    fs::write(path, SCRIPT).map_err(Error::io(path))?;
    fs::set_permissions(path, fs::Permissions::from_mode(EXECUTABLE)).map_err(Error::io(path))
}

#[cfg(not(unix))]
fn write_failing_tool(_: &Path) -> Result<()> {
    Err(Error::NotUnix)
}
