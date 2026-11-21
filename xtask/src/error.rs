use std::{
    env::JoinPathsError,
    io,
    path::{Path, PathBuf},
    process::ExitStatus,
};

pub type Result<T> = std::result::Result<T, Error>;

const BROKEN: u8 = 2;
const UNAVAILABLE: u8 = 77;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not run `{command}`: {source}")]
    Spawn { command: String, source: io::Error },
    #[error("`{command}` failed with {status}")]
    Failed { command: String, status: ExitStatus },
    #[error("`{command}` printed a line that is not a cargo message: {source}")]
    CargoMessage {
        command: String,
        source: serde_json::Error,
    },
    #[error("cargo built no `{name}` binary")]
    BinaryMissing { name: String },
    #[error("cargo built no test binaries")]
    NoTestBinaries,
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("walking {}: {source}", root.display())]
    Walk {
        root: PathBuf,
        source: walkdir::Error,
    },
    #[error("cannot build a PATH from the shim directory: {0}")]
    SearchPath(JoinPathsError),
    #[error(
        "no home directory to find the registry or the cache under; pass --corpus and --work-dir"
    )]
    HomeUnknown,
    #[error("no corpus at {} (pass --corpus or set RUST_FORMATTER_CORPUS_DIR)", path.display())]
    CorpusMissing { path: PathBuf },
    #[error("no .toml files sampled from {}", corpus.display())]
    NothingSampled { corpus: PathBuf },
    #[error(
        "the work tree at {} is not formatted as a loose directory; it is probably inside a \
         cargo project, so pass a --work-dir outside one",
        work.display()
    )]
    NotLoose { work: PathBuf },
    #[cfg(not(unix))]
    #[error("the toolchain gate shims rustup with a shell script, so it runs on Unix only")]
    NotUnix,
}

impl Error {
    pub fn io(path: &Path) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_owned();
        move |source| Self::Io { path, source }
    }

    pub fn walk(root: &Path) -> impl FnOnce(walkdir::Error) -> Self {
        let root = root.to_owned();
        move |source| Self::Walk { root, source }
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            Self::CorpusMissing { .. } | Self::NothingSampled { .. } | Self::NotLoose { .. } => {
                UNAVAILABLE
            }
            _ => BROKEN,
        }
    }
}
