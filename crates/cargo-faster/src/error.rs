//! Why a command failed, with a stable code for each kind of failure.

use bpaf_kit::{cmd, suggest};

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A subprocess couldn't start, timed out or failed.
    #[error(transparent)]
    Cmd(#[from] cmd::Error),
    /// A choice that isn't one of the valid values.
    #[error(transparent)]
    Unknown(#[from] suggest::UnknownValue),
    /// A crate, package or target name that isn't in the workspace.
    #[error(transparent)]
    UnknownName(#[from] UnknownName),
    /// The command line doesn't parse, or asks for something contradictory.
    #[error("{0}")]
    Usage(String),
    /// `tests --run` needs cargo-nextest, and it isn't on `PATH`.
    #[error("tests --run needs cargo-nextest: cargo install --locked cargo-nextest")]
    NoNextest,
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
            Self::UnknownName(_) => "unknown_value",
            Self::Usage(_) => "usage",
            Self::NoNextest => "no_nextest",
            Self::NoWorkspace(_) => "no_workspace",
            Self::NoTimings => "no_timings",
            Self::DryRun(_) => "dry_run",
            Self::Other(_) => "failed",
        }
    }
}

impl bpaf_kit::report::Coded for Error {
    fn code(&self) -> &str {
        Self::code(self)
    }

    /// 2 for usage errors (the cargo and clap convention), else 1.
    fn exit_code(&self) -> u8 {
        if matches!(self, Self::Usage(_)) { 2 } else { 1 }
    }
}

/// A name that matched nothing, with the closest few valid ones.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} '{input}'{}", self.hint())]
pub struct UnknownName {
    /// What was looked up (`workspace crate`, `cf-scratch binary`, ...).
    pub kind: String,
    /// What the user typed.
    pub input: String,
    /// The closest valid names, best first (at most 3).
    pub closest: Vec<String>,
    /// How many valid names there are.
    pub total: usize,
    /// The command that lists all of them, when there are more than shown.
    pub list_with: Option<&'static str>,
}

impl UnknownName {
    /// Pick the 3 names closest to `input` from `valid`.
    pub fn new<'a>(
        kind: impl Into<String>,
        input: &str,
        valid: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let valid: Vec<&str> = valid.into_iter().collect();
        Self {
            kind: kind.into(),
            input: input.to_owned(),
            closest: closest(input, &valid, 3),
            total: valid.len(),
            list_with: None,
        }
    }

    /// Name the command that lists every valid name.
    #[must_use]
    pub const fn list_with(mut self, cmd: &'static str) -> Self {
        self.list_with = Some(cmd);
        self
    }

    fn hint(&self) -> String {
        if self.total == 0 {
            return " (there are none)".into();
        }
        let list = self.closest.join(", ");
        if self.closest.len() == self.total {
            return format!(" (valid: {list})");
        }
        let more = self
            .list_with
            .map_or_else(String::new, |c| format!("; `{c}` lists them"));
        format!(", closest: {list} ({} in all{more})", self.total)
    }
}

/// The `n` candidates closest to `input`: case-insensitive, `_` and `-`
/// alike, a candidate containing `input` first, then by edit distance.
#[must_use]
pub fn closest(input: &str, candidates: &[&str], n: usize) -> Vec<String> {
    let norm = |s: &str| s.to_lowercase().replace('_', "-");
    let want = norm(input);
    let mut scored: Vec<(usize, &str)> = candidates
        .iter()
        .map(|c| {
            let c_norm = norm(c);
            let d = if c_norm.contains(&want) {
                0
            } else {
                1 + edit_distance(&want, &c_norm)
            };
            (d, *c)
        })
        .collect();
    scored.sort_unstable();
    scored
        .into_iter()
        .take(n)
        .map(|(_, c)| c.to_owned())
        .collect()
}

/// Levenshtein distance over chars.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if ca == *cb {
                prev
            } else {
                1 + prev.min(cur).min(row[j])
            };
            prev = cur;
        }
    }
    row[b.len()]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_names_show_the_closest_three() {
        let names = [
            "app-config",
            "app-core",
            "app-spool",
            "serde-thing",
            "other",
            "configure",
        ];
        let e =
            UnknownName::new("workspace crate", "app_confg", names).list_with("cargo faster rdeps");
        assert_eq!(e.closest, ["app-config", "app-core", "app-spool"]);
        assert_eq!(
            e.to_string(),
            "unknown workspace crate 'app_confg', closest: app-config, app-core, app-spool (6 in all; `cargo faster rdeps` lists them)"
        );
        // A substring match wins over a short edit distance.
        assert_eq!(closest("spool", &names, 1), ["app-spool"]);
        // Few names: list them all.
        let e = UnknownName::new("binary", "x", ["a", "b"]);
        assert_eq!(e.to_string(), "unknown binary 'x' (valid: a, b)");
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn usage_errors_exit_2() {
        use bpaf_kit::report::Coded;
        assert_eq!(Error::Usage("x".into()).exit_code(), 2);
        assert_eq!(Error::NoTimings.exit_code(), 1);
    }
}
