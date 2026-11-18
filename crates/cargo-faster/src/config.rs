//! The project's `cargo-faster.toml` (or `[workspace.metadata.cargo-faster]`
//! in the root `Cargo.toml`): findings it accepts, the crates people edit
//! most, time and dependency budgets, layering rules and the build
//! environment the numbers are measured in.
//!
//! Every table rejects unknown keys, so a typo is an error rather than a
//! rule that silently never applies. docs/cargo-faster.md has the
//! reference, docs/cargo-faster.schema.json the JSON schema.

use crate::error::Error;
use crate::finding::{Finding, Report};
use alloc::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The file name looked for at the workspace root.
pub const FILE_NAME: &str = "cargo-faster.toml";

/// The whole configuration. Every key is optional.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Findings that are there by design, each with a reason.
    #[serde(default)]
    pub accept: Vec<Accept>,
    /// The workspace crates people edit most.
    #[serde(default)]
    pub hot: Vec<String>,
    /// Limits `cargo faster check` enforces.
    #[serde(default)]
    pub budgets: Budgets,
    /// Dependency rules between groups of crates.
    #[serde(default, rename = "layer")]
    pub layers: Vec<Layer>,
    /// Dependencies to flag or allow.
    #[serde(default)]
    pub deps: Deps,
    /// The environment builds run in.
    #[serde(default)]
    pub env: Env,
}

/// `[[accept]]`: a finding that is there by design.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Accept {
    /// The finding id, e.g. `big-crate` (a glob: `*` and `?`).
    pub id: String,
    /// Only when the finding is about this crate (a glob).
    #[serde(default, rename = "crate", skip_serializing_if = "Option::is_none")]
    pub krate: Option<String>,
    /// Only when the finding is about this target: a unit or target name
    /// such as `app (test)` or `integration` (a glob).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Why it is accepted. Required, and shown in the report.
    pub reason: String,
}

/// `[budgets]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    /// Seconds a cold `cargo check --workspace` may take (`check --timings`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_check_secs: Option<f64>,
    /// Seconds the build after an edit to a hot crate may take: one number
    /// for every hot crate, or a table of crate = seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_rebuild_secs: Option<EditBudget>,
    /// Workspace crates the cold check's critical path may hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical_path_max_crates: Option<usize>,
    /// The workspace crates allowed on the cold check's critical path; any
    /// other one there is a violation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_new_critical_crates: Option<Vec<String>>,
    /// Crates that may be built in more than one version, not counting
    /// `deps.allow_duplicates`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duplicate_versions: Option<usize>,
}

/// `edit_rebuild_secs`: one budget for every hot crate, or one per crate.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum EditBudget {
    /// The same budget for every hot crate.
    All(f64),
    /// A budget per crate.
    PerCrate(BTreeMap<String, f64>),
}

impl EditBudget {
    /// The budget for an edit to `krate`, if it has one.
    #[must_use]
    pub fn for_crate(&self, krate: &str) -> Option<f64> {
        match self {
            Self::All(s) => Some(*s),
            Self::PerCrate(m) => m.get(krate).copied(),
        }
    }
}

/// `[[layer]]`: crates matching `crates` must not depend on crates
/// matching `must_not_depend_on`, directly or through other crates.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    /// The workspace crates the rule is about (globs).
    pub crates: Globs,
    /// Crates they must not depend on (globs; any package in the graph).
    pub must_not_depend_on: Vec<String>,
    /// Also follow their dev-dependencies (first hop only: a dependency's
    /// dev-dependencies are never built).
    #[serde(default)]
    pub dev: bool,
    /// Why the rule exists, shown with a violation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One glob or a list of them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Globs {
    /// One pattern.
    One(String),
    /// Several patterns.
    Many(Vec<String>),
}

impl Globs {
    /// The patterns.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        match self {
            Self::One(p) => core::slice::from_ref(p),
            Self::Many(v) => v,
        }
    }

    /// Whether `name` matches any pattern.
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        any_glob(self.patterns(), name)
    }
}

/// `[deps]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Deps {
    /// Crates that must not be in the normal or build graph (globs), e.g.
    /// heavy ones the project moved off.
    #[serde(default)]
    pub flag: Vec<String>,
    /// Crates allowed in more than one version (globs).
    #[serde(default)]
    pub allow_duplicates: Vec<String>,
}

/// `[env]`: the environment the project builds in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Env {
    /// Whether builds share a jobserver with other builds on the machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jobserver: Option<Jobserver>,
    /// The compiler cache builds are expected to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<Cache>,
    /// The cargo profile builds are measured with (`--profile`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The nextest profile `tests --run` uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nextest_profile: Option<String>,
    /// Features builds are measured with (`--features`).
    #[serde(default)]
    pub features: Vec<String>,
}

/// `env.jobserver`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Jobserver {
    /// Builds take tokens from a jobserver shared with other builds.
    Shared,
    /// Builds have the machine's jobs to themselves.
    Own,
}

/// `env.cache`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cache {
    /// kache as `RUSTC_WRAPPER`.
    Kache,
    /// sccache as `RUSTC_WRAPPER`.
    Sccache,
    /// No compiler cache.
    None,
}

impl Cache {
    /// The wrapper's program name (`None` for no cache).
    #[must_use]
    pub const fn program(self) -> Option<&'static str> {
        match self {
            Self::Kache => Some("kache"),
            Self::Sccache => Some("sccache"),
            Self::None => None,
        }
    }
}

/// A loaded configuration and where it came from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Loaded {
    /// The configuration.
    pub config: Config,
    /// The file it was read from (`None`: there is no configuration).
    pub source: Option<PathBuf>,
}

impl Loaded {
    /// The source as it reads in a report: the file, relative to `root`.
    #[must_use]
    pub fn describe(&self, root: &Path) -> String {
        self.source.as_ref().map_or_else(
            || "none".to_owned(),
            |p| p.strip_prefix(root).unwrap_or(p).display().to_string(),
        )
    }
}

impl Config {
    /// Parse a `cargo-faster.toml`.
    ///
    /// # Errors
    ///
    /// On invalid TOML, an unknown key or a value of the wrong type.
    pub fn parse(text: &str) -> Result<Self, String> {
        let c: Self = toml::from_str(text).map_err(|e| e.to_string().trim_end().to_owned())?;
        c.validate()?;
        Ok(c)
    }

    /// Read `[workspace.metadata.cargo-faster]` from a parsed root
    /// manifest, if it has one.
    ///
    /// # Errors
    ///
    /// On an unknown key or a value of the wrong type.
    pub fn from_manifest(manifest: &toml::Table) -> Result<Option<Self>, String> {
        let Some(v) = crate::context::lookup(manifest, "workspace.metadata.cargo-faster") else {
            return Ok(None);
        };
        let c: Self = v.clone().try_into().map_err(|e: toml::de::Error| {
            e.to_string().trim_end().to_owned()
        })?;
        c.validate()?;
        Ok(Some(c))
    }

    /// Load the workspace's configuration: `cargo-faster.toml` at `root`,
    /// else `[workspace.metadata.cargo-faster]` in `manifest`, else the
    /// defaults.
    ///
    /// # Errors
    ///
    /// When both exist, or the one that does is invalid.
    pub fn load(root: &Path, manifest: &toml::Table) -> Result<Loaded, Error> {
        let file = root.join(FILE_NAME);
        let from_manifest = Self::from_manifest(manifest)
            .map_err(|e| Error::Config(format!("{}: [workspace.metadata.cargo-faster]: {e}", root.join("Cargo.toml").display())))?;
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::Config(format!("{}: {e}", file.display()))),
        };
        match (text, from_manifest) {
            (Some(_), Some(_)) => Err(Error::Config(format!(
                "both {} and [workspace.metadata.cargo-faster] in Cargo.toml exist; keep one",
                file.display()
            ))),
            (Some(t), None) => Ok(Loaded {
                config: Self::parse(&t).map_err(|e| Error::Config(format!("{}: {e}", file.display())))?,
                source: Some(file),
            }),
            (None, Some(c)) => Ok(Loaded {
                config: c,
                source: Some(root.join("Cargo.toml")),
            }),
            (None, None) => Ok(Loaded::default()),
        }
    }

    /// Checks serde can't express.
    fn validate(&self) -> Result<(), String> {
        for (i, a) in self.accept.iter().enumerate() {
            if a.reason.trim().is_empty() {
                return Err(format!(
                    "accept[{i}] (id = \"{}\"): `reason` is empty; say why the finding is by design",
                    a.id
                ));
            }
        }
        for (i, l) in self.layers.iter().enumerate() {
            if l.crates.patterns().is_empty() || l.must_not_depend_on.is_empty() {
                return Err(format!(
                    "layer[{i}]: `crates` and `must_not_depend_on` need at least one pattern"
                ));
            }
        }
        let budgets = [
            ("cold_check_secs", self.budgets.cold_check_secs),
            (
                "edit_rebuild_secs",
                match &self.budgets.edit_rebuild_secs {
                    Some(EditBudget::All(s)) => Some(*s),
                    Some(EditBudget::PerCrate(m)) => m.values().copied().reduce(f64::min),
                    None => None,
                },
            ),
        ];
        for (k, v) in budgets {
            if v.is_some_and(|s| !s.is_finite() || s <= 0.0) {
                return Err(format!("budgets.{k}: seconds must be above 0"));
            }
        }
        Ok(())
    }

    /// Check that `hot`, `no_new_critical_crates` and per-crate edit
    /// budgets name workspace crates.
    ///
    /// # Errors
    ///
    /// Naming the first unknown crate and the closest valid ones.
    pub fn check_names<'a>(&self, members: impl IntoIterator<Item = &'a str> + Clone) -> Result<(), Error> {
        let known = |n: &str| {
            members
                .clone()
                .into_iter()
                .any(|m| m == n || m == n.replace('_', "-"))
        };
        let mut names: Vec<(&str, &str)> = self.hot.iter().map(|h| ("hot", h.as_str())).collect();
        if let Some(EditBudget::PerCrate(m)) = &self.budgets.edit_rebuild_secs {
            names.extend(m.keys().map(|k| ("budgets.edit_rebuild_secs", k.as_str())));
        }
        if let Some(c) = &self.budgets.no_new_critical_crates {
            names.extend(c.iter().map(|k| ("budgets.no_new_critical_crates", k.as_str())));
        }
        for (key, n) in names {
            if !known(n) {
                let e = crate::error::UnknownName::new("workspace crate", n, members.clone());
                return Err(Error::Config(format!("{key}: {e}")));
            }
        }
        Ok(())
    }

    /// Whether any crate matching `name` is allowed duplicate versions.
    #[must_use]
    pub fn duplicate_allowed(&self, name: &str) -> bool {
        any_glob(&self.deps.allow_duplicates, name)
    }
}

/// A finding a `[[accept]]` entry suppressed, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Accepted {
    /// The section it came from.
    pub section: String,
    /// The finding.
    pub finding: Finding,
    /// The entry's reason.
    pub reason: String,
}

impl Accept {
    /// Whether this entry accepts `f`.
    #[must_use]
    pub fn matches(&self, f: &Finding) -> bool {
        glob(&self.id, f.id)
            && self
                .krate
                .as_deref()
                .is_none_or(|p| subject_matches(p, f.krate.as_deref(), f))
            && self
                .target
                .as_deref()
                .is_none_or(|p| subject_matches(p, f.target.as_deref(), f))
    }
}

/// A `crate` or `target` pattern against a finding: the finding's own
/// field when it sets one, else any word of its title or `saw` (crate
/// names, unit labels).
fn subject_matches(pattern: &str, field: Option<&str>, f: &Finding) -> bool {
    if let Some(v) = field {
        return glob(pattern, v);
    }
    let text = format!("{} {}", f.title, f.saw);
    // A unit label like `app (test)` spans words: match it as a substring.
    if !pattern.contains(['*', '?']) && pattern.contains(' ') {
        return text.contains(pattern);
    }
    text.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .filter(|w| !w.is_empty())
        .any(|w| glob(pattern, w))
}

/// Move the findings `accept` matches out of `report` into
/// `report.accepted`.
pub fn apply_accepts(accept: &[Accept], report: &mut Report) {
    if accept.is_empty() {
        return;
    }
    for s in &mut report.sections {
        let mut kept = Vec::with_capacity(s.findings.len());
        for f in s.findings.drain(..) {
            match accept.iter().find(|a| a.matches(&f)) {
                Some(a) => report.accepted.push(Accepted {
                    section: s.name.clone(),
                    finding: f,
                    reason: a.reason.clone(),
                }),
                None => kept.push(f),
            }
        }
        s.findings = kept;
    }
}

/// Whether `name` matches any of `patterns`.
#[must_use]
pub fn any_glob(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| glob(p, name))
}

/// Shell-style glob over the whole string: `*` any run of characters,
/// `?` one character. `_` and `-` match each other, as in crate names.
#[must_use]
pub fn glob(pattern: &str, name: &str) -> bool {
    let norm = |c: char| if c == '_' { '-' } else { c };
    let p: Vec<char> = pattern.chars().map(norm).collect();
    let n: Vec<char> = name.chars().map(norm).collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ni));
                pi += 1;
            }
            Some(&c) if c == '?' || c == n[ni] => {
                pi += 1;
                ni += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    pi = sp + 1;
                    ni = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Section, Severity};

    const FULL: &str = r#"
hot = ["app-core", "app-web"]

[[accept]]
id = "big-crate"
crate = "app-gen"
reason = "generated protobuf code"

[budgets]
cold_check_secs = 90
edit_rebuild_secs = { app-core = 20, app-web = 8.5 }
critical_path_max_crates = 4
no_new_critical_crates = ["app-core"]
max_duplicate_versions = 3

[[layer]]
crates = "valet-*"
must_not_depend_on = ["tala-*"]
reason = "valet is published on its own"

[[layer]]
crates = ["app-core"]
must_not_depend_on = ["hyper", "app-web"]
dev = true

[deps]
flag = ["openssl-sys"]
allow_duplicates = ["windows-*"]

[env]
jobserver = "shared"
cache = "kache"
profile = "gate"
nextest_profile = "ci"
features = ["test-utils"]
"#;

    #[test]
    fn parses_every_section() {
        let c = Config::parse(FULL).unwrap();
        assert_eq!(c.hot, ["app-core", "app-web"]);
        assert_eq!(c.accept[0].krate.as_deref(), Some("app-gen"));
        assert_eq!(c.budgets.cold_check_secs, Some(90.0));
        let e = c.budgets.edit_rebuild_secs.as_ref().unwrap();
        assert_eq!(e.for_crate("app-web"), Some(8.5));
        assert_eq!(e.for_crate("other"), None);
        assert_eq!(EditBudget::All(5.0).for_crate("x"), Some(5.0));
        assert!(c.layers[0].crates.matches("valet-core"));
        assert!(c.layers[1].dev);
        assert_eq!(c.env.jobserver, Some(Jobserver::Shared));
        assert_eq!(c.env.cache, Some(Cache::Kache));
        assert!(c.duplicate_allowed("windows-sys"));
        assert!(!c.duplicate_allowed("syn"));
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn rejects_unknown_keys_and_empty_reasons() {
        let e = Config::parse("hott = []").unwrap_err();
        assert!(e.contains("unknown field `hott`"), "{e}");
        let e = Config::parse("[budgets]\ncold_check = 3").unwrap_err();
        assert!(e.contains("unknown field `cold_check`"), "{e}");
        assert!(e.contains("cold_check_secs"), "{e}");
        let e = Config::parse("[[accept]]\nid = \"x\"\nreason = \" \"").unwrap_err();
        assert!(e.contains("`reason` is empty"), "{e}");
        let e = Config::parse("[[accept]]\nid = \"x\"").unwrap_err();
        assert!(e.contains("missing field `reason`"), "{e}");
        let e = Config::parse("[env]\ncache = \"ccache\"").unwrap_err();
        assert!(e.contains("unknown variant `ccache`"), "{e}");
        let e = Config::parse("[budgets]\ncold_check_secs = 0").unwrap_err();
        assert!(e.contains("above 0"), "{e}");
    }

    #[test]
    fn loads_from_the_file_or_the_manifest_but_not_both() {
        let dir = tempfile::tempdir().unwrap();
        let manifest: toml::Table =
            "[workspace.metadata.cargo-faster]\nhot = [\"a\"]\n".parse().unwrap();
        let l = Config::load(dir.path(), &manifest).unwrap();
        assert_eq!(l.config.hot, ["a"]);
        assert_eq!(l.describe(dir.path()), "Cargo.toml");
        let bad: toml::Table = "[workspace.metadata.cargo-faster]\nbogus = 1\n".parse().unwrap();
        let e = Config::load(dir.path(), &bad).unwrap_err().to_string();
        assert!(e.contains("[workspace.metadata.cargo-faster]: unknown field `bogus`"), "{e}");
        std::fs::write(dir.path().join(FILE_NAME), "hot = [\"b\"]\n").unwrap();
        let e = Config::load(dir.path(), &manifest).unwrap_err();
        assert_eq!(e.code(), "config");
        let l = Config::load(dir.path(), &toml::Table::new()).unwrap();
        assert_eq!(l.config.hot, ["b"]);
        assert_eq!(l.describe(dir.path()), FILE_NAME);
        std::fs::write(dir.path().join(FILE_NAME), "hot = 3\n").unwrap();
        let e = Config::load(dir.path(), &toml::Table::new()).unwrap_err().to_string();
        assert!(e.contains("cargo-faster.toml") && e.contains("invalid type"), "{e}");
        assert_eq!(
            Config::load(&dir.path().join("none"), &toml::Table::new()).unwrap(),
            Loaded::default()
        );
    }

    #[test]
    fn names_must_be_workspace_crates() {
        let c = Config::parse(FULL).unwrap();
        c.check_names(["app-core", "app-web"]).unwrap();
        let e = c.check_names(["app-core", "app-wb"]).unwrap_err().to_string();
        assert!(e.starts_with("hot: unknown workspace crate 'app-web'"), "{e}");
    }

    #[test]
    fn globs() {
        assert!(glob("tala-*", "tala-core"));
        assert!(glob("tala_*", "tala-core"));
        assert!(!glob("tala-*", "valet-tala"));
        assert!(glob("*-core", "tala-core"));
        assert!(glob("a?c", "abc"));
        assert!(glob("*", ""));
        assert!(glob("a*b*c", "axxbyyc"));
        assert!(!glob("a*b*c", "axxbyy"));
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exactly"));
    }

    #[test]
    fn accepts_move_findings_out_of_the_report() {
        let f = |id, title: &str| Finding::new(id, Severity::Medium, title, "debug-info");
        let mut s = Section::new("rdeps");
        s.extend([
            f("big-crate", "app-gen has 40000 lines of Rust"),
            f("big-crate", "app-core has 30000 lines of Rust"),
            f("slow-unit", "app (test) took 30s").saw("x"),
            f("layer-violation", "t").krate("valet-core"),
        ]);
        let mut r = Report {
            sections: vec![s],
            ..Report::default()
        };
        let accept = [
            Accept {
                id: "big-crate".into(),
                krate: Some("app-gen".into()),
                target: None,
                reason: "generated".into(),
            },
            Accept {
                id: "slow-*".into(),
                krate: None,
                target: Some("app (test)".into()),
                reason: "one big integration test".into(),
            },
            Accept {
                id: "layer-violation".into(),
                krate: Some("valet-*".into()),
                target: None,
                reason: "moving".into(),
            },
        ];
        apply_accepts(&accept, &mut r);
        let left: Vec<&str> = r.sections[0].findings.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(left, ["app-core has 30000 lines of Rust"]);
        assert_eq!(r.accepted.len(), 3);
        assert_eq!(r.accepted[0].reason, "generated");
        let text = r.render_text(false);
        assert!(text.contains("3 accepted"), "{text}");
    }
}
