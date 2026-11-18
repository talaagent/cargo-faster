//! What `timings --run` builds, and the probe edit `timings --edit` makes
//! to measure what an edit to one crate costs.

use crate::context::Metadata;
use crate::error::Error;
use crate::error::UnknownName;
use alloc::collections::BTreeSet;
use core::fmt;
use core::str::FromStr;
use std::path::{Path, PathBuf};

/// The build `timings --run` times.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Build {
    /// `cargo check`.
    #[default]
    Check,
    /// `cargo build`: a dev build, with codegen and links.
    Build,
    /// `cargo clippy`.
    Clippy,
    /// The test build: `cargo nextest run --no-run`, or `cargo test
    /// --no-run` without nextest.
    Test,
}

impl FromStr for Build {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "check" => Ok(Self::Check),
            "build" => Ok(Self::Build),
            "clippy" => Ok(Self::Clippy),
            "test" => Ok(Self::Test),
            _ => Err(format!(
                "`{s}` isn't a build cargo faster times: use check, build, clippy or test"
            )),
        }
    }
}

impl fmt::Display for Build {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Check => "check",
            Self::Build => "build",
            Self::Clippy => "clippy",
            Self::Test => "test",
        })
    }
}

impl Build {
    /// The cargo arguments, without `--timings` and the user's extra
    /// arguments. Always `--workspace`, never `-p`.
    #[must_use]
    pub fn args(self, nextest: bool) -> Vec<&'static str> {
        match self {
            Self::Check => vec!["check", "--workspace"],
            Self::Build => vec!["build", "--workspace"],
            Self::Clippy => vec!["clippy", "--workspace"],
            Self::Test if nextest => vec!["nextest", "run", "--workspace", "--no-run"],
            Self::Test => vec!["test", "--workspace", "--no-run"],
        }
    }
}

/// What the probe edit changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// A new public function each run: the crate's API changes, as when
    /// you add or change a public item.
    Api,
    /// A private function whose body changes each run: an edit inside a
    /// function body.
    Body,
}

impl Probe {
    /// Describes the edit, for the report.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Api => "a new public function (the crate's API changed)",
            Self::Body => "a private function's body (the API didn't change)",
        }
    }
}

/// The marker in every probe, so a probe left behind is easy to find.
pub const MARKER: &str = "cargo-faster probe";

/// The probe appended to the crate's root file; `n` makes each run's edit
/// different.
#[must_use]
pub fn probe_text(probe: Probe, n: u64) -> String {
    let allow = "#[allow(dead_code, missing_docs, unreachable_pub, clippy::all, clippy::pedantic, clippy::nursery)]";
    match probe {
        Probe::Api => format!(
            "\n// {MARKER} (`cargo faster timings --edit`): delete this if it was left behind\n#[doc(hidden)]\n{allow}\npub fn __cargo_faster_probe_{n}() -> u64 {{\n    {n}\n}}\n"
        ),
        Probe::Body => format!(
            "\n// {MARKER} (`cargo faster timings --edit`): delete this if it was left behind\n{allow}\nfn __cargo_faster_probe() -> u64 {{\n    {n}\n}}\n"
        ),
    }
}

/// The crate an edit goes into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTarget {
    /// Package name.
    pub krate: String,
    /// Its library's root file (or its first binary's).
    pub file: PathBuf,
    /// A proc-macro crate can only export macros, so it gets a body edit.
    pub proc_macro: bool,
}

/// Find the workspace member `name` and the file to edit.
///
/// # Errors
///
/// When no member has that name (with a suggestion), or it has no library
/// or binary target.
pub fn edit_target(m: &Metadata, name: &str) -> Result<EditTarget, Error> {
    let want = name.replace('_', "-");
    let Some(p) = m
        .members()
        .find(|p| p.name == name || p.name.replace('_', "-") == want)
    else {
        return Err(UnknownName::new(
            "workspace crate",
            name,
            m.members().map(|p| p.name.as_str()),
        )
        .list_with("cargo faster rdeps")
        .into());
    };
    let has = |k: &str| {
        p.targets
            .iter()
            .find(|t| t.kind.iter().any(|x| x == k))
            .map(|t| t.src_path.clone())
    };
    let proc_macro = has("proc-macro");
    let file = has("lib")
        .or_else(|| proc_macro.clone())
        .or_else(|| has("bin"))
        .ok_or_else(|| format!("{} has no library or binary to edit", p.name))?;
    Ok(EditTarget {
        krate: p.name.clone(),
        file,
        proc_macro: proc_macro.is_some(),
    })
}

/// The cargo fingerprint directories (`<package>-<hash>`) of `members`
/// (package names) in the target directory, in any profile
/// (`debug/.fingerprint`) or target
/// (`x86_64-unknown-linux-gnu/debug/.fingerprint`). Each is one unit: a
/// package's target in one mode, profile and feature set, so a check, a
/// test build and a release build of a crate each have their own.
#[must_use]
pub fn fingerprints(target_dir: &Path, members: &[&str]) -> BTreeSet<PathBuf> {
    let subdirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p)
            .map(|d| {
                d.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut dirs = Vec::new();
    for profile in subdirs(target_dir) {
        dirs.push(profile.join(".fingerprint"));
        dirs.extend(
            subdirs(&profile)
                .into_iter()
                .map(|p| p.join(".fingerprint")),
        );
    }
    dirs.iter()
        .flat_map(|d| subdirs(d))
        .filter(|d| package(d).is_some_and(|pkg| members.contains(&pkg)))
        .collect()
}

/// The package of a fingerprint directory, `app-core` for `app-core-9f8e`.
fn package(dir: &Path) -> Option<&str> {
    dir.file_name()?
        .to_str()?
        .rsplit_once('-')
        .map(|(pkg, _)| pkg)
}

/// What a run knows about whether its build was a first build, to decide
/// once the report says which workspace crates compiled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FirstBuildHint {
    /// Nothing: tell from the report.
    #[default]
    Unknown,
    /// The run made an edit, so it's not a first build.
    Edit,
    /// The packages that got a fingerprint directory they didn't have
    /// before the build: units built for the first time in this mode and
    /// profile.
    New(BTreeSet<String>),
}

impl FirstBuildHint {
    /// From the fingerprint directories before and after the build.
    #[must_use]
    pub fn new(before: &BTreeSet<PathBuf>, after: &BTreeSet<PathBuf>) -> Self {
        Self::New(
            after
                .difference(before)
                .filter_map(|d| package(d))
                .map(str::to_owned)
                .collect(),
        )
    }

    /// For [`crate::timings::Extra::first_build`]: `Some(true)` when every
    /// workspace crate that compiled did so for the first time in this mode
    /// and profile (a first test build after checks, a first release
    /// build), `Some(false)` after an edit, otherwise `None`. With no new
    /// fingerprints at all they live elsewhere (a build-dir), so the report
    /// tells.
    #[must_use]
    pub fn resolve<'a>(&self, compiled: impl IntoIterator<Item = &'a str>) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::Edit => Some(false),
            Self::New(new) => {
                let mut compiled = compiled.into_iter().peekable();
                (compiled.peek().is_some() && compiled.all(|c| new.contains(c))).then_some(true)
            }
        }
    }
}

/// A probe edit in place: the file's original bytes, written back by
/// [`Edit::restore`], or on drop or Ctrl-C if that never runs.
#[derive(Debug)]
pub struct Edit {
    path: PathBuf,
    original: Vec<u8>,
    restored: bool,
    _on_interrupt: bpaf_kit::interrupt::Cleanup,
}

impl Edit {
    /// Append `text` to `path`.
    ///
    /// # Errors
    ///
    /// When the file can't be read or written, or already has a probe (an
    /// earlier run was killed before it could restore the file).
    pub fn apply(path: &Path, text: &str) -> Result<Self, Error> {
        let original =
            std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        if String::from_utf8_lossy(&original).contains(MARKER) {
            return Err(format!(
                "{} already has a {MARKER} from an earlier run that didn't finish; delete it and try again",
                path.display()
            )
            .into());
        }
        let mut edited = original.clone();
        if !edited.is_empty() && !edited.ends_with(b"\n") {
            edited.push(b'\n');
        }
        edited.extend_from_slice(text.as_bytes());
        let on_interrupt = bpaf_kit::interrupt::on_interrupt({
            let (path, original) = (path.to_path_buf(), original.clone());
            move || drop(std::fs::write(path, original))
        });
        std::fs::write(path, &edited).map_err(|e| format!("editing {}: {e}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            original,
            restored: false,
            _on_interrupt: on_interrupt,
        })
    }

    /// Write the original contents back.
    ///
    /// # Errors
    ///
    /// When the file can't be written.
    pub fn restore(mut self) -> Result<(), Error> {
        self.restored = true;
        std::fs::write(&self.path, &self.original)
            .map_err(|e| format!("restoring {}: {e}", self.path.display()).into())
    }
}

impl Drop for Edit {
    fn drop(&mut self) {
        if !self.restored {
            let _ = std::fs::write(&self.path, &self.original);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args() {
        assert_eq!("build".parse::<Build>(), Ok(Build::Build));
        assert!("bench".parse::<Build>().is_err());
        assert_eq!(Build::Check.args(true), ["check", "--workspace"]);
        assert_eq!(Build::Clippy.args(false), ["clippy", "--workspace"]);
        assert_eq!(
            Build::Test.args(true),
            ["nextest", "run", "--workspace", "--no-run"]
        );
        assert_eq!(Build::Test.args(false), ["test", "--workspace", "--no-run"]);
        for b in [Build::Check, Build::Build, Build::Clippy, Build::Test] {
            assert!(!b.args(true).contains(&"-p"));
            assert_eq!(b.to_string().parse::<Build>(), Ok(b));
        }
    }

    const META: &str = r#"{
      "workspace_root": "/w", "target_directory": "/w/target",
      "workspace_members": ["core 0.1", "app 0.1", "mac 0.1"],
      "packages": [
        {"id": "core 0.1", "name": "app-core", "version": "0.1.0", "manifest_path": "/w/core/Cargo.toml",
         "targets": [{"name": "app_core", "kind": ["lib"], "src_path": "/w/core/src/lib.rs"}]},
        {"id": "app 0.1", "name": "app", "version": "0.1.0", "manifest_path": "/w/app/Cargo.toml",
         "targets": [{"name": "app", "kind": ["bin"], "src_path": "/w/app/src/main.rs"}]},
        {"id": "mac 0.1", "name": "mac", "version": "0.1.0", "manifest_path": "/w/mac/Cargo.toml",
         "targets": [{"name": "mac", "kind": ["proc-macro"], "src_path": "/w/mac/src/lib.rs"}]},
        {"id": "serde 1", "name": "serde", "version": "1.0.0", "manifest_path": "/r/serde/Cargo.toml",
         "targets": [{"name": "serde", "kind": ["lib"], "src_path": "/r/serde/src/lib.rs"}]}
      ]
    }"#;

    #[test]
    fn edit_targets() {
        let m = Metadata::parse(META).unwrap();
        let t = edit_target(&m, "app_core").unwrap();
        assert_eq!(t.krate, "app-core");
        assert_eq!(t.file, Path::new("/w/core/src/lib.rs"));
        assert!(!t.proc_macro);
        assert_eq!(
            edit_target(&m, "app").unwrap().file,
            Path::new("/w/app/src/main.rs")
        );
        assert!(edit_target(&m, "mac").unwrap().proc_macro);
        // Not a member.
        let e = edit_target(&m, "serde").unwrap_err();
        assert_eq!(e.code(), "unknown_value");
        assert!(
            edit_target(&m, "app-cor")
                .unwrap_err()
                .to_string()
                .contains("app-core")
        );
    }

    #[test]
    fn probes_differ_per_run() {
        let a = probe_text(Probe::Api, 1);
        assert!(a.contains("pub fn __cargo_faster_probe_1()"));
        assert_ne!(a, probe_text(Probe::Api, 2));
        let b = probe_text(Probe::Body, 1);
        assert!(!b.contains("pub fn"));
        assert!(b.contains("fn __cargo_faster_probe() -> u64 {\n    1\n}"));
        assert!(a.contains(MARKER) && b.contains(MARKER));
    }

    #[test]
    fn first_build_per_mode() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path();
        let fp = |d: &str| std::fs::create_dir_all(t.join(d)).unwrap();
        assert!(fingerprints(t, &["app-core"]).is_empty());
        fp("debug/.fingerprint/serde-0123abcd");
        assert!(fingerprints(t, &["app-core"]).is_empty());
        fp("x86_64-unknown-linux-gnu/gate/.fingerprint/app-core-9f8e");
        let members = ["app-core", "app"];
        // A check of both crates.
        fp("debug/.fingerprint/app-1111");
        let checked = fingerprints(t, &members);
        assert_eq!(checked.len(), 2);
        // Then a first test build: new units for both.
        fp("debug/.fingerprint/app-2222");
        fp("debug/.fingerprint/app-core-3333");
        let hint = FirstBuildHint::new(&checked, &fingerprints(t, &members));
        assert_eq!(hint.resolve(["app", "app-core"]), Some(true));
        // A build that also recompiled a crate it had built before.
        assert_eq!(hint.resolve(["app", "app-core", "other"]), None);
        assert_eq!(hint.resolve([]), None);
        // Nothing new: an ordinary rebuild.
        let same = FirstBuildHint::new(&checked, &checked);
        assert_eq!(same.resolve(["app"]), None);
        assert_eq!(FirstBuildHint::Edit.resolve(["app"]), Some(false));
        assert_eq!(FirstBuildHint::Unknown.resolve(["app"]), None);
    }

    #[test]
    fn edit_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("lib.rs");
        std::fs::write(&f, "pub fn x() {}").unwrap();
        let e = Edit::apply(&f, &probe_text(Probe::Api, 7)).unwrap();
        let edited = std::fs::read_to_string(&f).unwrap();
        assert!(edited.starts_with("pub fn x() {}\n\n// cargo-faster probe"));
        // A second probe on top of a leftover one is refused.
        assert!(Edit::apply(&f, &probe_text(Probe::Api, 8)).is_err());
        e.restore().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "pub fn x() {}");
        // Dropping without restore() restores too (an error mid-build).
        drop(Edit::apply(&f, &probe_text(Probe::Body, 9)).unwrap());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "pub fn x() {}");
    }
}
