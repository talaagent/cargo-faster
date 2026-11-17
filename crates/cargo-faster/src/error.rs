//! Why a command failed, with a stable code for each kind of failure.

use bpaf_kit::{cmd, suggest};

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A subprocess couldn't start, timed out or failed.
    #[error(transparent)]
    Cmd(#[from] cmd::Error),
    /// A crate, package or target name that isn't in the workspace.
    #[error(transparent)]
    Unknown(#[from] suggest::UnknownValue),
    /// `cargo metadata` failed, with the end of its stderr.
    #[error("cargo metadata failed (is this a Cargo workspace?):\n{0}")]
    NoWorkspace(String),
    /// There is no `--timings` report to read.
    #[error(
        "no cargo-timing.html in target/cargo-timings; run `cargo build --timings` or `cargo faster timings --run`"
    )]
    NoTimings,
    /// `--dry-run` stopped before a step that needs a real build's output.
    #[error("{0}")]
    DryRun(String),
    /// Anything else: a file that can't be read or parsed, a profile with
    /// no data.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Stable `snake_case` identifier, printed as `error[code]`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Cmd(e) => e.code(),
            Self::Unknown(e) => e.code(),
            Self::NoWorkspace(_) => "no_workspace",
            Self::NoTimings => "no_timings",
            Self::DryRun(_) => "dry_run",
            Self::Other(_) => "failed",
        }
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self::Other(message.to_owned())
    }
}
