//! Everything the checks read: the environment, cargo configuration,
//! `cargo metadata`, the root manifest and `rustc -vV`.
//!
//! Gathering is separate from checking so the checks can run on fixtures.

use crate::error::Error;
use alloc::collections::BTreeMap;
use bpaf_kit::cmd::Cmd;
use core::time::Duration;
use serde::Deserialize;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The subset of `cargo metadata` the checks use.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Metadata {
    /// Workspace root directory.
    pub workspace_root: PathBuf,
    /// Target directory.
    pub target_directory: PathBuf,
    /// Package ids of workspace members.
    pub workspace_members: Vec<String>,
    /// All packages in the dependency graph.
    pub packages: Vec<Package>,
    /// The resolved graph.
    pub resolve: Option<Resolve>,
}

/// A package.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Package {
    /// Package id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Version.
    pub version: String,
    /// Path to Cargo.toml.
    pub manifest_path: PathBuf,
    /// Targets (lib, bin, test, ...).
    #[serde(default)]
    pub targets: Vec<Target>,
    /// Declared dependencies.
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

/// A build target.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Target {
    /// Target name.
    pub name: String,
    /// Kinds, e.g. `lib`, `bin`, `proc-macro`, `test`.
    pub kind: Vec<String>,
    /// Root source file.
    pub src_path: PathBuf,
}

/// A declared dependency.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Dependency {
    /// Crate name.
    pub name: String,
    /// `null` (normal), `dev` or `build`.
    pub kind: Option<String>,
    /// Requested features.
    #[serde(default)]
    pub features: Vec<String>,
    /// Whether `default-features` is on.
    #[serde(default = "yes")]
    pub uses_default_features: bool,
    /// Path dependencies have a path.
    pub path: Option<PathBuf>,
    /// Whether the dependency is optional (enabled through a feature).
    #[serde(default)]
    pub optional: bool,
}

fn yes() -> bool {
    true
}

/// The resolved dependency graph.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Resolve {
    /// One node per package.
    pub nodes: Vec<Node>,
}

/// A resolved package.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Node {
    /// Package id.
    pub id: String,
    /// Its dependencies.
    #[serde(default)]
    pub deps: Vec<NodeDep>,
    /// Enabled features.
    #[serde(default)]
    pub features: Vec<String>,
}

/// An edge in the resolved graph.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NodeDep {
    /// The name the dependent's code uses for it (its lib name, or the
    /// rename in `Cargo.toml`).
    #[serde(default)]
    pub name: String,
    /// The dependency's package id.
    pub pkg: String,
    /// The edge kinds.
    #[serde(default)]
    pub dep_kinds: Vec<DepKind>,
}

/// One kind of edge.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DepKind {
    /// `null` (normal), `dev` or `build`.
    pub kind: Option<String>,
}

impl Metadata {
    /// Parse `cargo metadata --format-version 1` output.
    ///
    /// # Errors
    ///
    /// On malformed JSON.
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("parsing cargo metadata: {e}"))
    }

    /// Workspace member packages.
    pub fn members(&self) -> impl Iterator<Item = &Package> {
        self.packages
            .iter()
            .filter(|p| self.workspace_members.contains(&p.id))
    }

    /// A package by id.
    #[must_use]
    pub fn package(&self, id: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.id == id)
    }
}

/// Merged cargo configuration, nearest file first.
#[derive(Debug, Clone, Default)]
pub struct CargoConfig {
    /// `(file, parsed)` in priority order.
    pub files: Vec<(PathBuf, toml::Table)>,
}

impl CargoConfig {
    /// Read `.cargo/config.toml` (or `.cargo/config`) from `dir` up to the
    /// filesystem root, then `$CARGO_HOME/config.toml`.
    #[must_use]
    pub fn discover(dir: &Path, cargo_home: Option<&Path>) -> Self {
        let mut files = Vec::new();
        let mut seen = Vec::new();
        let mut candidates: Vec<PathBuf> = dir.ancestors().map(|d| d.join(".cargo")).collect();
        if let Some(home) = cargo_home {
            candidates.push(home.to_path_buf());
        }
        for d in candidates {
            for name in ["config.toml", "config"] {
                let p = d.join(name);
                let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
                if seen.contains(&canon) {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&p)
                    && let Ok(t) = text.parse::<toml::Table>()
                {
                    seen.push(canon);
                    files.push((p, t));
                    break;
                }
            }
        }
        Self { files }
    }

    /// Build from in-memory files (for tests).
    ///
    /// # Panics
    ///
    /// On invalid TOML.
    #[must_use]
    pub fn from_strs(files: &[(&str, &str)]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(p, t)| {
                    (
                        PathBuf::from(p),
                        t.parse::<toml::Table>().expect("valid toml"),
                    )
                })
                .collect(),
        }
    }

    /// The value at a dotted path, e.g. `build.rustc-wrapper`, and the
    /// file it came from.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<(&toml::Value, &Path)> {
        self.files
            .iter()
            .find_map(|(f, t)| lookup(t, path).map(|v| (v, f.as_path())))
    }

    /// A string value.
    #[must_use]
    pub fn get_str(&self, path: &str) -> Option<(&str, &Path)> {
        self.get(path).and_then(|(v, f)| v.as_str().map(|s| (s, f)))
    }

    /// The rustflags cargo takes from config: `target.<triple>.rustflags`
    /// and matching `[target.'cfg(..)']` rustflags if any are set, else
    /// `build.rustflags` (cargo ignores `build.rustflags` when a target
    /// sets rustflags). String or array form, with the file each came from.
    #[must_use]
    pub fn rustflags(&self, host: &str) -> Vec<(String, PathBuf)> {
        let target = self.target_rustflags(host);
        if target.is_empty() {
            self.build_rustflags()
        } else {
            target
        }
    }

    /// `target.<triple>.rustflags` and `[target.'cfg(..)'] rustflags`.
    #[must_use]
    pub fn target_rustflags(&self, host: &str) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        let path = format!("target.{host}.rustflags");
        for (f, t) in &self.files {
            if let Some(v) = lookup(t, &path) {
                out.extend(flags(v).into_iter().map(|fl| (fl, f.clone())));
            }
            // `[target.'cfg(..)'] rustflags` applies when the cfg matches;
            // include them all (we can't evaluate cfgs cheaply).
            if let Some(toml::Value::Table(targets)) = t.get("target") {
                for (k, v) in targets {
                    if k.starts_with("cfg(")
                        && let Some(v) = v.get("rustflags")
                    {
                        out.extend(flags(v).into_iter().map(|fl| (fl, f.clone())));
                    }
                }
            }
        }
        out
    }

    /// `build.rustflags` from every config file.
    #[must_use]
    pub fn build_rustflags(&self) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for (f, t) in &self.files {
            if let Some(v) = lookup(t, "build.rustflags") {
                out.extend(flags(v).into_iter().map(|fl| (fl, f.clone())));
            }
        }
        out
    }
}

fn flags(v: &toml::Value) -> Vec<String> {
    match v {
        toml::Value::String(s) => s.split_whitespace().map(str::to_owned).collect(),
        toml::Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Look up a dotted path in a TOML table.
#[must_use]
pub fn lookup<'a>(t: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
    let mut parts = path.split('.');
    let mut v = t.get(parts.next()?)?;
    for p in parts {
        v = v.as_table()?.get(p)?;
    }
    Some(v)
}

/// `rustc -vV`.
#[derive(Debug, Clone, Default)]
pub struct RustcInfo {
    /// e.g. `1.98.1`.
    pub release: String,
    /// Host triple.
    pub host: String,
    /// Whether this is a nightly (or dev) compiler.
    pub nightly: bool,
}

impl RustcInfo {
    /// Parse `rustc -vV` output.
    #[must_use]
    pub fn parse(out: &str) -> Self {
        let field = |k: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_owned()))
                .unwrap_or_default()
        };
        let release = field("release:");
        Self {
            nightly: release.contains("nightly") || release.contains("dev"),
            host: field("host:"),
            release,
        }
    }

    /// `(major, minor)`.
    #[must_use]
    pub fn version(&self) -> (u32, u32) {
        let mut it = self
            .release
            .split(['.', '-'])
            .map(|p| p.parse().unwrap_or(0));
        (it.next().unwrap_or(0), it.next().unwrap_or(0))
    }
}

/// Everything gathered about a workspace.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Where we ran.
    pub cwd: PathBuf,
    /// The process environment.
    pub env: BTreeMap<String, String>,
    /// `cargo metadata`, if it ran.
    pub metadata: Option<Metadata>,
    /// Cargo configuration.
    pub config: CargoConfig,
    /// The workspace root `Cargo.toml`.
    pub manifest: toml::Table,
    /// `rustc -vV`.
    pub rustc: RustcInfo,
}

impl Context {
    /// Gather from the real system.
    ///
    /// # Errors
    ///
    /// When `cargo metadata` fails (not in a workspace).
    pub fn gather(manifest_path: Option<&Path>) -> Result<Self, Error> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let env: BTreeMap<String, String> = std::env::vars().collect();
        let cargo = cargo_bin(&env);
        let mut metadata = probe(&cargo);
        metadata.args(["metadata", "--format-version", "1"]);
        if let Some(m) = manifest_path {
            metadata.arg("--manifest-path").arg(m);
        }
        let out = metadata.output()?;
        if !out.success() {
            return Err(Error::NoWorkspace(out.stderr_lossy().trim().to_owned()));
        }
        let metadata = Metadata::parse(&String::from_utf8_lossy(&out.stdout))?;
        let root = metadata.workspace_root.clone();
        let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or_default();
        let cargo_home = env
            .get("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| env.get("HOME").map(|h| Path::new(h).join(".cargo")));
        let config = CargoConfig::discover(&root, cargo_home.as_deref());
        let rustc_bin = env
            .get("RUSTC")
            .cloned()
            .unwrap_or_else(|| "rustc".to_owned());
        let rustc = probe(&rustc_bin)
            .arg("-vV")
            .current_dir(&root)
            .output()
            .map(|o| RustcInfo::parse(&o.stdout_lossy()))
            .unwrap_or_default();
        Ok(Self {
            cwd,
            env,
            metadata: Some(metadata),
            config,
            manifest,
            rustc,
        })
    }

    /// The workspace root (or the current directory).
    #[must_use]
    pub fn root(&self) -> &Path {
        self.metadata
            .as_ref()
            .map_or(&self.cwd, |m| &m.workspace_root)
    }

    /// The target directory.
    #[must_use]
    pub fn target_dir(&self) -> PathBuf {
        self.metadata
            .as_ref()
            .map_or_else(|| self.cwd.join("target"), |m| m.target_directory.clone())
    }

    /// An environment variable.
    #[must_use]
    pub fn var(&self, k: &str) -> Option<&str> {
        self.env
            .get(k)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// `profile.<name>.<key>` from the manifest, else from config, else
    /// from `CARGO_PROFILE_<NAME>_<KEY>`; with where it came from.
    #[must_use]
    pub fn profile_value(&self, profile: &str, key: &str) -> Option<(toml::Value, String)> {
        let env_key = format!(
            "CARGO_PROFILE_{}_{}",
            profile.to_uppercase().replace('-', "_"),
            key.to_uppercase().replace('-', "_")
        );
        if let Some(v) = self.var(&env_key) {
            let parsed = v
                .parse::<i64>()
                .map(toml::Value::Integer)
                .or_else(|_| v.parse::<bool>().map(toml::Value::Boolean))
                .unwrap_or_else(|_| toml::Value::String(v.to_owned()));
            return Some((parsed, env_key));
        }
        let path = format!("profile.{profile}.{key}");
        if let Some((v, f)) = self.config.get(&path) {
            return Some((v.clone(), f.display().to_string()));
        }
        lookup(&self.manifest, &path).map(|v| (v.clone(), "Cargo.toml".to_owned()))
    }

    /// The number of CPUs.
    #[must_use]
    pub fn cpus(&self) -> usize {
        std::thread::available_parallelism().map_or(1, core::num::NonZero::get)
    }
}

/// A read-only probe (`cargo metadata`, `rustc -vV`, `git`, `kache`,
/// `df`): not echoed, run even under `--dry-run`, output captured, a
/// five-minute timeout. A non-zero exit is a [`bpaf_kit::cmd::Output`], not an error.
#[must_use]
pub fn probe(program: impl AsRef<OsStr>) -> Cmd {
    let mut cmd = Cmd::new(program);
    cmd.echo(false)
        .dry_run(false)
        .allow_failure()
        .timeout(Duration::from_secs(300))
        .max_output(1 << 30);
    cmd
}

/// A build (`cargo check`, a profiled rustc): echoed `set -x` style,
/// skipped under `--dry-run`, output captured, a four-hour timeout, under
/// `nice -n N` when `nice` is positive and on `PATH`. A non-zero exit is an
/// [`bpaf_kit::cmd::Output`], not an error.
#[must_use]
pub fn build(ctx: &Context, nice: i32, program: impl AsRef<OsStr>) -> Cmd {
    let mut cmd = if nice > 0 && on_path(&ctx.env, "nice").is_some() {
        let mut c = Cmd::new("nice");
        c.arg("-n").arg(nice.to_string()).arg(program);
        c
    } else {
        Cmd::new(program)
    };
    cmd.allow_failure()
        .timeout(Duration::from_secs(4 * 3600))
        .max_output(1 << 30);
    cmd
}

/// The cargo binary to run (`$CARGO` when we're a subcommand).
#[must_use]
pub fn cargo_bin(env: &BTreeMap<String, String>) -> String {
    env.get("CARGO")
        .cloned()
        .unwrap_or_else(|| "cargo".to_owned())
}

/// Whether a program is on `PATH`.
#[must_use]
pub fn on_path(env: &BTreeMap<String, String>, prog: &str) -> Option<PathBuf> {
    let path = env.get("PATH")?;
    std::env::split_paths(path)
        .map(|d| d.join(prog))
        .find(|p| p.is_file())
}

/// Whether cargo can find the subcommand `cargo-{name}`: on `PATH`, or in
/// `$CARGO_HOME/bin` (`~/.cargo/bin`), where cargo also looks.
pub fn has_subcommand(env: &BTreeMap<String, String>, name: &str) -> bool {
    let prog = format!("cargo-{name}");
    let home_bin = env
        .get("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env.get("HOME").map(|h| Path::new(h).join(".cargo")))
        .map(|h| h.join("bin").join(&prog));
    on_path(env, &prog).is_some() || home_bin.is_some_and(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_lookup_prefers_nearest() {
        let c = CargoConfig::from_strs(&[
            (
                "/w/.cargo/config.toml",
                "[build]\nrustc-wrapper = \"kache\"\n",
            ),
            (
                "/home/.cargo/config.toml",
                "[build]\nrustc-wrapper = \"sccache\"\njobs = 8\n",
            ),
        ]);
        assert_eq!(c.get_str("build.rustc-wrapper").unwrap().0, "kache");
        assert_eq!(c.get("build.jobs").unwrap().0.as_integer(), Some(8));
    }

    #[test]
    fn rustflags_both_forms() {
        let c = CargoConfig::from_strs(&[(
            "/w/.cargo/config.toml",
            "[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]\n[build]\nrustflags = \"-C target-cpu=native\"\n",
        )]);
        let f = |host| -> Vec<String> { c.rustflags(host).into_iter().map(|x| x.0).collect() };
        // Target rustflags win; build.rustflags is ignored then.
        assert_eq!(
            f("x86_64-unknown-linux-gnu"),
            ["-C", "link-arg=-fuse-ld=mold"]
        );
        assert_eq!(f("aarch64-unknown-linux-gnu"), ["-C", "target-cpu=native"]);
    }

    #[test]
    fn rustc_info() {
        let r = RustcInfo::parse(
            "rustc 1.98.1 (abc 2026-10-01)\nbinary: rustc\nhost: x86_64-unknown-linux-gnu\nrelease: 1.98.1\n",
        );
        assert_eq!(r.version(), (1, 98));
        assert!(!r.nightly);
        assert_eq!(r.host, "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn profile_value_sources() {
        let mut ctx = Context {
            manifest: "[profile.dev]\ndebug = 1\n".parse().unwrap(),
            ..Context::default()
        };
        assert_eq!(
            ctx.profile_value("dev", "debug").unwrap().0.as_integer(),
            Some(1)
        );
        ctx.env
            .insert("CARGO_PROFILE_DEV_DEBUG".into(), "false".into());
        let (v, src) = ctx.profile_value("dev", "debug").unwrap();
        assert_eq!(v.as_bool(), Some(false));
        assert_eq!(src, "CARGO_PROFILE_DEV_DEBUG");
    }
}
