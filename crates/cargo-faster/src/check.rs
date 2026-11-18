//! `cargo faster check`: the configuration's rules, checked without a
//! build (layering, flagged and duplicate dependencies, the cache), and
//! its time budgets against measured numbers (`--timings`).
//!
//! The dependency rules run on a [`guppy`] package graph built from
//! `cargo metadata`, so they follow cargo's feature resolution and
//! platform-specific (`[target.'cfg(..)'.dependencies]`) edges rather than
//! every edge in the lock file.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use core::fmt::Write as _;

use guppy::PackageId;
use guppy::graph::cargo::{CargoOptions, CargoResolverVersion, CargoSet};
use guppy::graph::feature::{
    FeatureFilter, FeatureFilterFn, FeatureGraph, FeatureId, FeatureLabel, FeatureSet,
    StandardFeatures,
};
use guppy::graph::{PackageGraph, PackageLink};
use guppy::platform::{Platform, PlatformSpec, TargetFeatures};

use crate::config::{Config, any_glob};
use crate::context::{Context, cargo_bin, probe};
use crate::error::Error;
use crate::finding::{Finding, Section, Severity, table};

/// Build the package graph from `cargo metadata --format-version 1` JSON.
///
/// # Errors
///
/// When the JSON isn't `cargo metadata` output guppy understands.
pub fn package_graph(metadata_json: &str) -> Result<PackageGraph, Error> {
    PackageGraph::from_json(metadata_json)
        .map_err(|e| Error::Other(format!("building the package graph: {e}")))
}

/// Run `cargo metadata --all-features` for the workspace and build its
/// package graph. guppy resolves features itself, so it needs every
/// optional dependency in the resolve, which plain `cargo metadata` leaves
/// out when the feature is off.
///
/// # Errors
///
/// When `cargo metadata` fails or its output can't be read.
pub fn gather_graph(ctx: &Context) -> Result<PackageGraph, Error> {
    let out = probe(cargo_bin(&ctx.env))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--all-features",
            "--manifest-path",
        ])
        .arg(ctx.root().join("Cargo.toml"))
        .output()?;
    if !out.success() {
        return Err(Error::NoWorkspace(out.stderr_lossy().trim().to_owned()));
    }
    package_graph(&String::from_utf8_lossy(&out.stdout))
}

/// The [`Resolve`] for the workspace in `ctx` with its configuration.
#[must_use]
pub fn resolve_for(ctx: &Context) -> Resolve {
    Resolve {
        resolver: crate::context::lookup(&ctx.manifest, "workspace.resolver")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        features: ctx.faster.env.features.clone(),
        host: ctx.rustc.host.clone(),
    }
}

/// What the graph rules resolve with: the workspace's feature resolver,
/// the configured features and the platform builds run on.
#[derive(Debug, Clone, Default)]
pub struct Resolve {
    /// `workspace.resolver` from the root manifest (`"1"`, `"2"`, `"3"`).
    pub resolver: Option<String>,
    /// `env.features`: `name` (any workspace crate that has it) or
    /// `crate/name`.
    pub features: Vec<String>,
    /// The host triple (`rustc -vV`); duplicates and flagged crates are
    /// counted for this platform, layering for every platform.
    pub host: String,
}

impl Resolve {
    fn options(&self, include_dev: bool, platform: PlatformSpec) -> CargoOptions<'static> {
        let mut o = CargoOptions::new();
        o.set_resolver(match self.resolver.as_deref() {
            Some("1") => CargoResolverVersion::V1,
            Some("2") => CargoResolverVersion::V2,
            _ => CargoResolverVersion::V3,
        })
        .set_include_dev(include_dev)
        .set_platform(platform);
        o
    }

    fn host_platform(&self) -> PlatformSpec {
        Platform::new(self.host.clone(), TargetFeatures::Unknown)
            .map_or(PlatformSpec::Any, PlatformSpec::from)
    }

    /// The initial feature set: `ids` with default features plus the
    /// configured ones.
    fn initials<'g>(&self, pg: &'g PackageGraph, ids: &[&'g PackageId]) -> FeatureSet<'g> {
        let wanted = &self.features;
        let mut base = StandardFeatures::Default;
        let filter = FeatureFilterFn::new(move |fg: &FeatureGraph<'g>, fid: FeatureId<'g>| {
            if base.accept(fg, fid) {
                return true;
            }
            let (FeatureLabel::Named(name) | FeatureLabel::OptionalDependency(name)) = fid.label()
            else {
                return false;
            };
            let pkg = fg
                .package_graph()
                .metadata(fid.package_id())
                .map(|m| m.name())
                .unwrap_or_default();
            wanted
                .iter()
                .any(|w| w == name || w.split_once('/') == Some((pkg, name)))
        });
        pg.resolve_ids(ids.iter().copied()).map_or_else(
            |_| pg.feature_graph().resolve_none(),
            |set| set.to_feature_set(filter),
        )
    }
}

/// The links a build follows, as an adjacency list.
fn adjacency<'g>(set: &CargoSet<'g>) -> BTreeMap<&'g PackageId, Vec<PackageLink<'g>>> {
    let mut adj: BTreeMap<&PackageId, Vec<PackageLink<'g>>> = BTreeMap::new();
    for link in set.target_links().chain(set.host_links()) {
        adj.entry(link.from().id()).or_default().push(link);
    }
    adj
}

/// The shortest chain of crate names from `from` to a package `hit`
/// accepts, over `adj`.
fn chain<'g>(
    adj: &BTreeMap<&'g PackageId, Vec<PackageLink<'g>>>,
    from: &'g PackageId,
    mut hit: impl FnMut(&PackageLink<'g>) -> bool,
) -> Vec<Vec<PackageLink<'g>>> {
    let mut found = Vec::new();
    let mut seen: BTreeSet<&PackageId> = BTreeSet::from([from]);
    let mut queue: VecDeque<(&PackageId, Vec<PackageLink<'g>>)> = VecDeque::from([(from, vec![])]);
    while let Some((id, path)) = queue.pop_front() {
        for link in adj.get(id).into_iter().flatten() {
            let to = link.to().id();
            if !seen.insert(to) {
                continue;
            }
            let mut p = path.clone();
            p.push(*link);
            if hit(link) {
                found.push(p);
            } else {
                queue.push_back((to, p));
            }
        }
    }
    found
}

fn render_chain(path: &[PackageLink<'_>]) -> String {
    let mut s = path
        .first()
        .map(|l| l.from().name().to_owned())
        .unwrap_or_default();
    for l in path {
        let kind = if l.normal().is_present() {
            ""
        } else if l.build().is_present() {
            " (build)"
        } else {
            " (dev)"
        };
        let _ = write!(s, " → {}{kind}", l.to().name());
    }
    s
}

/// `[[layer]]`: every workspace crate a rule names, resolved for any
/// platform with its default and configured features; a crate it must not
/// depend on anywhere in that build is a `layer-violation`.
#[must_use]
pub fn layers(pg: &PackageGraph, cfg: &Config, r: &Resolve) -> Vec<Finding> {
    let mut out = Vec::new();
    for layer in &cfg.layers {
        for member in pg.workspace().iter() {
            if !layer.crates.matches(member.name()) {
                continue;
            }
            let initials = r.initials(pg, &[member.id()]);
            let opts = r.options(layer.dev, PlatformSpec::Any);
            let Ok(set) = CargoSet::new(initials, pg.feature_graph().resolve_none(), &opts) else {
                continue;
            };
            let adj = adjacency(&set);
            for path in chain(&adj, member.id(), |l| {
                l.to().name() != member.name() && any_glob(&layer.must_not_depend_on, l.to().name())
            }) {
                let Some(last) = path.last() else { continue };
                let banned = last.to().name();
                let mut why = String::from(
                    "a layer that reaches into a crate above it rebuilds whenever that crate changes, \
                     and the two can no longer be built, tested or published apart",
                );
                if let Some(reason) = &layer.reason {
                    let _ = write!(why, "; the rule: {reason}");
                }
                out.push(
                    Finding::new(
                        "layer-violation",
                        Severity::High,
                        format!("{} depends on {banned}", member.name()),
                        "layering-rules",
                    )
                    .krate(member.name())
                    .target(banned)
                    .saw(format!(
                        "{} (layer rule `{}` must not depend on `{}`)",
                        render_chain(&path),
                        layer.crates.patterns().join("`, `"),
                        layer.must_not_depend_on.join("`, `"),
                    ))
                    .why(why)
                    .fix(format!(
                        "cut the edge `{} → {banned}`: move what `{}` uses from `{banned}` down a \
                         layer, or invert it behind a trait the upper crate implements. If the edge is \
                         intended, narrow the rule or `[[accept]]` it with the reason",
                        last.from().name(),
                        last.from().name(),
                    )),
                );
            }
        }
    }
    out.sort_by(|a, b| (&a.krate, &a.target).cmp(&(&b.krate, &b.target)));
    out
}

/// The whole workspace's build for the host, with dev-dependencies (what
/// `cargo check --workspace --all-targets` builds).
fn workspace_set<'g>(pg: &'g PackageGraph, r: &Resolve, dev: bool) -> Option<CargoSet<'g>> {
    let ids: Vec<&PackageId> = pg.workspace().iter().map(|m| m.id()).collect();
    let initials = r.initials(pg, &ids);
    CargoSet::new(
        initials,
        pg.feature_graph().resolve_none(),
        &r.options(dev, r.host_platform()),
    )
    .ok()
}

/// Crates built in more than one version for the host, with every target
/// (`deps.allow_duplicates` left out).
#[must_use]
pub fn duplicates(pg: &PackageGraph, cfg: &Config, r: &Resolve) -> BTreeMap<String, Vec<String>> {
    let mut by_name: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Some(set) = workspace_set(pg, r, true) {
        for f in [set.target_features(), set.host_features()] {
            for p in f
                .to_package_set()
                .packages(guppy::graph::DependencyDirection::Forward)
            {
                by_name
                    .entry(p.name().to_owned())
                    .or_default()
                    .insert(p.version().to_string());
            }
        }
    }
    by_name
        .into_iter()
        .filter(|(n, v)| v.len() > 1 && !cfg.duplicate_allowed(n))
        .map(|(n, v)| (n, v.into_iter().collect()))
        .collect()
}

/// `budgets.max_duplicate_versions` against [`duplicates`].
#[must_use]
pub fn duplicate_budget(dups: &BTreeMap<String, Vec<String>>, cfg: &Config) -> Vec<Finding> {
    let Some(max) = cfg.budgets.max_duplicate_versions else {
        return vec![];
    };
    if dups.len() <= max {
        return vec![];
    }
    let list: Vec<String> = dups
        .iter()
        .map(|(n, v)| format!("{n} ({})", v.join(", ")))
        .collect();
    vec![
        Finding::new(
            "duplicates-over-budget",
            Severity::High,
            format!(
                "{} crate{} built in more than one version, budget {max}",
                dups.len(),
                if dups.len() == 1 { "" } else { "s" }
            ),
            "dependencies-and-features",
        )
        .saw(format!("built twice or more: {}", list.join("; ")))
        .why("every extra version is another crate to compile and link, cold and in CI")
        .fix(
            "`cargo tree -d` shows who pulls each version; bump the laggard, or list the crate \
             in `deps.allow_duplicates` when the split is known and waiting on upstream",
        ),
    ]
}

/// `deps.flag`: crates that must not be in the normal or build graph of
/// the host build (dev-dependencies don't count).
#[must_use]
pub fn flagged(pg: &PackageGraph, cfg: &Config, r: &Resolve) -> Vec<Finding> {
    if cfg.deps.flag.is_empty() {
        return vec![];
    }
    let Some(set) = workspace_set(pg, r, false) else {
        return vec![];
    };
    let adj = adjacency(&set);
    let mut out = Vec::new();
    let mut done = BTreeSet::new();
    for member in pg.workspace().iter() {
        for path in chain(&adj, member.id(), |l| {
            any_glob(&cfg.deps.flag, l.to().name())
        }) {
            let Some(last) = path.last() else { continue };
            let name = last.to().name();
            if !done.insert(name) {
                continue;
            }
            out.push(
                Finding::new(
                    "flagged-dependency",
                    Severity::High,
                    format!("{name} is in the build"),
                    "dependencies-and-features",
                )
                .krate(name)
                .saw(format!(
                    "{} {} (`deps.flag` = `{}`)",
                    render_chain(&path),
                    last.to().version(),
                    cfg.deps.flag.join("`, `")
                ))
                .why("the project flagged this crate as one to keep out of the build")
                .fix(format!(
                    "drop the edge that pulls it in (`cargo tree -i {name}`), or turn off the feature \
                     that enables it"
                )),
            );
        }
    }
    out
}

/// `env.cache` against the `RUSTC_WRAPPER` in effect.
#[must_use]
pub fn env_cache(cfg: &Config, wrapper: Option<&str>) -> Vec<Finding> {
    let Some(want) = cfg.env.cache else {
        return vec![];
    };
    let have = wrapper.map(|w| {
        let base = w.rsplit('/').next().unwrap_or(w);
        base.trim_end_matches(".exe").to_owned()
    });
    if have.as_deref() == want.program() {
        return vec![];
    }
    let have_s = have.as_deref().unwrap_or("no RUSTC_WRAPPER");
    let want_s = want.program().unwrap_or("no wrapper");
    vec![
        Finding::new(
            "env-cache-mismatch",
            Severity::High,
            format!("builds use {have_s}, the project expects {want_s}"),
            "sccache-and-kache",
        )
        .saw(format!("`env.cache` = {want_s}; RUSTC_WRAPPER: {have_s}"))
        .why(
            "numbers measured without the expected cache don't compare with the project's \
             budgets, and builds redo work the cache would share",
        )
        .fix(format!(
            "set RUSTC_WRAPPER={want_s} (or `build.rustc-wrapper` in .cargo/config.toml), or \
             change `env.cache`"
        )),
    ]
}

/// What `check --timings` measured.
#[derive(Debug, Clone, Default)]
pub struct Measured {
    /// Wall seconds of a cold `cargo check --workspace`.
    pub cold_check_secs: Option<f64>,
    /// The workspace crates on that check's critical path, in order.
    pub critical_path: Vec<String>,
    /// Wall seconds of the build after an edit to each hot crate.
    pub edits: Vec<(String, f64)>,
}

/// `[budgets]` against measured numbers.
#[must_use]
pub fn budgets(cfg: &Config, m: &Measured) -> Vec<Finding> {
    let b = &cfg.budgets;
    let mut out = Vec::new();
    if let (Some(max), Some(got)) = (b.cold_check_secs, m.cold_check_secs)
        && got > max
    {
        out.push(
            Finding::new(
                "cold-check-over-budget",
                Severity::High,
                format!("cold check took {got:.1}s, budget {max}s"),
                "build-budgets",
            )
            .saw(format!(
                "`cargo check --workspace` from an empty target directory: {got:.1}s (budget \
                 `cold_check_secs` = {max})"
            ))
            .why("a cold check is what a new worktree, a CI run and a cache miss pay")
            .fix(
                "`cargo faster timings --run` shows the critical path and the crates on it; if \
                 the slowdown is expected, raise the budget",
            ),
        );
    }
    if let Some(max) = b.critical_path_max_crates
        && m.critical_path.len() > max
    {
        out.push(
            Finding::new(
                "critical-path-too-long",
                Severity::High,
                format!(
                    "{} workspace crates on the critical path, budget {max}",
                    m.critical_path.len()
                ),
                "the-critical-path",
            )
            .saw(format!("critical path: {}", m.critical_path.join(" → ")))
            .why("crates on the critical path build one after another, whatever the core count")
            .fix("cut an edge on the path so the crates can build side by side"),
        );
    }
    if let Some(allowed) = &b.no_new_critical_crates {
        for c in m.critical_path.iter().filter(|c| !allowed.contains(c)) {
            out.push(
                Finding::new(
                    "new-critical-crate",
                    Severity::High,
                    format!("{c} is on the critical path"),
                    "the-critical-path",
                )
                .krate(c)
                .saw(format!(
                    "critical path: {}; `no_new_critical_crates` allows {}",
                    m.critical_path.join(" → "),
                    allowed.join(", ")
                ))
                .why("a new crate on the critical path makes every cold build wait for it")
                .fix(format!(
                    "find the edge that put `{c}` in line (`cargo faster rdeps`) and cut it, or add it \
                     to `no_new_critical_crates` if it belongs there"
                )),
            );
        }
    }
    if let Some(eb) = &b.edit_rebuild_secs {
        for (c, got) in &m.edits {
            let Some(max) = eb.for_crate(c) else { continue };
            if *got > max {
                out.push(
                    Finding::new(
                        "edit-rebuild-over-budget",
                        Severity::High,
                        format!("an edit to {c} rebuilds in {got:.1}s, budget {max}s"),
                        "build-budgets",
                    )
                    .krate(c)
                    .saw(format!(
                        "touched {c} and rebuilt: {got:.1}s (budget `edit_rebuild_secs` = {max})"
                    ))
                    .why("this is the wait after every save in a crate the team edits most")
                    .fix(format!(
                        "`cargo faster timings --edit {c}` shows which dependents rebuild and how long \
                         each takes; `cargo faster rdeps` shows the fan-out"
                    )),
                );
            }
        }
    }
    out
}

/// The `check` section: the rules that need no build.
#[must_use]
pub fn section(pg: &PackageGraph, cfg: &Config, r: &Resolve, wrapper: Option<&str>) -> Section {
    let mut s = Section::new("check");
    let members = pg.workspace().iter().count();
    s.fact("workspace crates", members.to_string());
    s.fact("layer rules", cfg.layers.len().to_string());
    let dups = duplicates(pg, cfg, r);
    s.fact(
        format!("duplicate versions ({})", r.host),
        dups.len().to_string(),
    );
    if !dups.is_empty() {
        let rows: Vec<Vec<String>> = dups
            .iter()
            .map(|(n, v)| vec![n.clone(), v.join(", ")])
            .collect();
        s.details.push(table(&["crate", "versions"], &rows));
    }
    s.extend(layers(pg, cfg, r));
    s.extend(flagged(pg, cfg, r));
    s.extend(duplicate_budget(&dups, cfg));
    s.extend(env_cache(cfg, wrapper));
    s
}

/// Render findings as GitHub Actions workflow commands
/// (`::error title=..::message`), one per line.
#[must_use]
pub fn github_annotations(findings: &[&Finding]) -> String {
    let mut out = String::new();
    for f in findings {
        let level = match f.severity {
            Severity::Info => "notice",
            Severity::Low | Severity::Medium => "warning",
            Severity::High => "error",
        };
        let msg = format!("{}\n{}\nfix: {}\n{}", f.saw, f.why, f.fix, f.guide);
        let _ = writeln!(
            out,
            "::{level} title={}::{}",
            escape_property(&format!("{}: {}", f.id, f.title)),
            escape_data(&msg)
        );
    }
    out
}

fn escape_data(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn escape_property(s: &str) -> String {
    escape_data(s).replace(':', "%3A").replace(',', "%2C")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn graph() -> PackageGraph {
        let json = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/layered/metadata.json"
        ))
        .expect("fixture");
        package_graph(&json).expect("graph")
    }

    fn linux() -> Resolve {
        Resolve {
            resolver: Some("2".into()),
            features: vec![],
            host: "x86_64-unknown-linux-gnu".into(),
        }
    }

    fn ids(f: &[Finding]) -> Vec<String> {
        f.iter().map(|f| format!("{} {}", f.id, f.title)).collect()
    }

    #[test]
    fn layers_follow_platform_and_dev_edges() {
        let pg = graph();
        let cfg = Config::parse(
            r#"
            [[layer]]
            crates = "valet-*"
            must_not_depend_on = ["tala-*"]
            reason = "valet is a library"
            "#,
        )
        .expect("config");
        let f = layers(&pg, &cfg, &linux());
        // Through util-b, and through a Windows-only dependency: the rule
        // holds on every platform. valet-ok's tala-core is a dev-dependency.
        assert_eq!(
            ids(&f),
            [
                "layer-violation valet-io depends on tala-core",
                "layer-violation valet-io depends on tala-win"
            ]
        );
        assert!(
            f[0].saw.contains("valet-io → util-b → tala-core"),
            "{}",
            f[0].saw
        );
        assert!(f[0].why.contains("valet is a library"));
        assert_eq!(f[0].krate.as_deref(), Some("valet-io"));
        assert_eq!(f[0].target.as_deref(), Some("tala-core"));

        let dev = Config::parse(
            r#"
            [[layer]]
            crates = "valet-ok"
            must_not_depend_on = ["tala-*"]
            dev = true
            "#,
        )
        .expect("config");
        let f = layers(&pg, &dev, &linux());
        assert_eq!(ids(&f), ["layer-violation valet-ok depends on tala-core"]);
        assert!(
            f[0].saw.contains("valet-ok → tala-core (dev)"),
            "{}",
            f[0].saw
        );
    }

    #[test]
    fn layers_resolve_features() {
        let pg = graph();
        let cfg = Config::parse(
            r#"
            [[layer]]
            crates = "app-*"
            must_not_depend_on = ["tala-*"]
            "#,
        )
        .expect("config");
        // tala-glue is behind app-cli's off-by-default `glue` feature.
        assert_eq!(layers(&pg, &cfg, &linux()).len(), 0);
        let mut r = linux();
        r.features = vec!["app-cli/glue".into()];
        assert_eq!(
            ids(&layers(&pg, &cfg, &r)),
            ["layer-violation app-cli depends on tala-glue"]
        );
        r.features = vec!["glue".into()];
        assert_eq!(layers(&pg, &cfg, &r).len(), 1);
    }

    #[test]
    fn duplicates_and_flags_follow_the_host() {
        let pg = graph();
        let mut cfg = Config::parse(
            r#"
            [budgets]
            max_duplicate_versions = 0
            [deps]
            flag = ["heavy", "winonly", "testkit"]
            "#,
        )
        .expect("config");
        let dups = duplicates(&pg, &cfg, &linux());
        assert_eq!(dups.get("util-a").map(Vec::len), Some(2));
        assert_eq!(dups.len(), 1);
        assert_eq!(
            ids(&duplicate_budget(&dups, &cfg)),
            ["duplicates-over-budget 1 crate built in more than one version, budget 0"]
        );
        // winonly is Windows-only, testkit a dev-dependency.
        let f = flagged(&pg, &cfg, &linux());
        assert_eq!(ids(&f), ["flagged-dependency heavy is in the build"]);
        assert!(f[0].saw.contains("app-core → heavy 0.1.0"), "{}", f[0].saw);
        let mut win = linux();
        win.host = "x86_64-pc-windows-msvc".into();
        assert_eq!(flagged(&pg, &cfg, &win).len(), 2);

        cfg.deps.allow_duplicates = vec!["util-*".into()];
        assert_eq!(duplicates(&pg, &cfg, &linux()).len(), 0);
    }

    #[test]
    fn env_cache_compares_the_wrapper() {
        let cfg = Config::parse("[env]\ncache = \"kache\"").expect("config");
        assert_eq!(
            env_cache(&cfg, Some("/nix/store/x-kache/bin/kache")).len(),
            0
        );
        assert_eq!(
            ids(&env_cache(&cfg, Some("sccache"))),
            ["env-cache-mismatch builds use sccache, the project expects kache"]
        );
        assert_eq!(env_cache(&cfg, None).len(), 1);
        let none = Config::parse("[env]\ncache = \"none\"").expect("config");
        assert_eq!(env_cache(&none, None).len(), 0);
    }

    #[test]
    fn budgets_compare_measured_numbers() {
        let cfg = Config::parse(
            r#"
            [budgets]
            cold_check_secs = 60
            critical_path_max_crates = 2
            no_new_critical_crates = ["core", "cli"]
            edit_rebuild_secs = { core = 10.0 }
            "#,
        )
        .expect("config");
        let m = Measured {
            cold_check_secs: Some(75.0),
            critical_path: vec!["core".into(), "db".into(), "cli".into()],
            edits: vec![("core".into(), 12.5), ("cli".into(), 99.0)],
        };
        assert_eq!(
            ids(&budgets(&cfg, &m)),
            [
                "cold-check-over-budget cold check took 75.0s, budget 60s",
                "critical-path-too-long 3 workspace crates on the critical path, budget 2",
                "new-critical-crate db is on the critical path",
                "edit-rebuild-over-budget an edit to core rebuilds in 12.5s, budget 10s",
            ]
        );
        let ok = Measured {
            cold_check_secs: Some(30.0),
            critical_path: vec!["core".into()],
            edits: vec![("core".into(), 2.0)],
        };
        assert_eq!(budgets(&cfg, &ok).len(), 0);
    }

    #[test]
    fn github_annotations_escape() {
        let f = Finding::new(
            "layer-violation",
            Severity::High,
            "a: b, c",
            "layering-rules",
        )
        .saw("50%\nnext");
        let out = github_annotations(&[&f]);
        assert!(
            out.starts_with("::error title=layer-violation%3A a%3A b%2C c::50%25%0Anext%0A"),
            "{out}"
        );
        assert_eq!(out.lines().count(), 1);
    }
}
