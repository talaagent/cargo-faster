//! Dependencies: heavy feature sets, duplicate versions, proc macros.

use crate::asynctrait::{self, Scan};
use crate::context::Metadata;
use crate::finding::{Finding, Section, Severity};
use alloc::collections::{BTreeMap, BTreeSet};
use core::cmp::Ordering;
use serde::Serialize;

/// What the dependency graph says.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Analysis {
    /// Packages in the build.
    pub packages: usize,
    /// Crates with more than one version: name -> versions.
    pub duplicates: BTreeMap<String, Vec<String>>,
    /// Proc-macro crates.
    pub proc_macros: Vec<String>,
    /// Enabled tokio features.
    pub tokio_features: Vec<String>,
    /// Workspace crates that ask for tokio's `full` feature.
    pub tokio_full_in: Vec<String>,
    /// Whether syn is built with `full`.
    pub syn_full: bool,
    /// Whether syn 1 and syn 2 are both built.
    pub syn_versions: Vec<String>,
    /// Workspace crates depending on async-trait directly.
    pub async_trait_in: Vec<String>,
    /// `#[async_trait]` uses in the workspace's sources, when they were read.
    pub async_trait: Option<Scan>,
    /// Normal and build dependencies that turn on test-only features.
    pub test_features: Vec<TestFeature>,
    /// Test-only features turned on in `[workspace.dependencies]`, which every
    /// crate inheriting the entry gets, in its normal builds too.
    pub workspace_test_features: Vec<WorkspaceTestFeature>,
}

/// A `[workspace.dependencies]` entry enabling test-only features.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceTestFeature {
    /// The dependency.
    pub dependency: String,
    /// Its test-only features that are enabled.
    pub features: Vec<String>,
    /// Workspace crates with a normal or build dependency on it.
    pub inherited_by: usize,
}

/// A workspace crate enabling a test-only feature of a non-dev dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestFeature {
    /// The workspace crate whose manifest asks for it.
    pub krate: String,
    /// The dependency.
    pub dependency: String,
    /// Its test-only features that are enabled.
    pub features: Vec<String>,
}

/// Whether a feature name reads as test-only: `testing`, `test-util(s)`,
/// `test-helpers`, `mock*`, `fake*` and the like.
fn is_test_feature(f: &str) -> bool {
    let f = f.to_ascii_lowercase();
    matches!(f.as_str(), "test" | "tests" | "testing")
        || f.starts_with("test-")
        || f.starts_with("test_")
        || f.starts_with("testutil")
        || f.starts_with("mock")
        || f.starts_with("fake")
}

/// Whether a workspace crate is itself test support (`foo-test-utils`,
/// `foo-testing`, `mock-server`), whose normal dependencies may well need
/// test features.
fn is_test_crate(name: &str) -> bool {
    name.split(['-', '_']).any(|w| {
        matches!(
            w.to_ascii_lowercase().as_str(),
            "test"
                | "tests"
                | "testing"
                | "testutil"
                | "testutils"
                | "mock"
                | "mocks"
                | "fake"
                | "fakes"
                | "fixture"
                | "fixtures"
        )
    })
}

/// [`analyze`], plus a scan of the workspace's sources for `#[async_trait]`.
#[must_use]
pub fn analyze_with_sources(m: &Metadata) -> Analysis {
    let mut a = analyze(m);
    a.async_trait = Some(asynctrait::scan_workspace(m));
    if let Some(root) = std::fs::read_to_string(m.workspace_root.join("Cargo.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
    {
        attribute_workspace_features(&mut a, &root);
    }
    a
}

/// Move test features that come from `[workspace.dependencies]` out of the
/// per-crate list: `cargo metadata` shows them on every inheriting crate, but
/// there is one place to fix them.
pub fn attribute_workspace_features(a: &mut Analysis, root: &toml::Table) {
    let Some(deps) =
        crate::context::lookup(root, "workspace.dependencies").and_then(toml::Value::as_table)
    else {
        return;
    };
    for (key, entry) in deps {
        let Some(t) = entry.as_table() else { continue };
        let name = t
            .get("package")
            .and_then(toml::Value::as_str)
            .unwrap_or(key);
        let features: Vec<String> = t
            .get("features")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .filter(|f| is_test_feature(f))
            .map(str::to_owned)
            .collect();
        if features.is_empty() {
            continue;
        }
        let mut inherited_by = BTreeSet::new();
        for tf in a
            .test_features
            .iter_mut()
            .filter(|tf| tf.dependency == name)
        {
            inherited_by.insert(tf.krate.clone());
            tf.features.retain(|f| !features.contains(f));
        }
        a.workspace_test_features.push(WorkspaceTestFeature {
            dependency: name.to_owned(),
            features,
            inherited_by: inherited_by.len(),
        });
    }
    a.test_features.retain(|tf| !tf.features.is_empty());
}

/// From this many `#[async_trait]` uses on, replacing them is worth a plan.
pub const ASYNC_TRAIT_MANY: usize = 10;

/// The `disallowed-macros` entry that keeps `#[async_trait]` out.
pub const CLIPPY_BAN: &str = r#"disallowed-macros = [{ path = "async_trait::async_trait", reason = "native async fn in traits; dynosaur for dyn" }]"#;

/// Analyze `cargo metadata`.
#[must_use]
pub fn analyze(m: &Metadata) -> Analysis {
    let mut a = Analysis::default();
    let used: BTreeSet<&str> = m.resolve.as_ref().map_or_else(
        || m.packages.iter().map(|p| p.id.as_str()).collect(),
        |r| r.nodes.iter().map(|n| n.id.as_str()).collect(),
    );
    let pkgs: Vec<_> = m
        .packages
        .iter()
        .filter(|p| used.contains(p.id.as_str()))
        .collect();
    a.packages = pkgs.len();
    let mut versions: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for p in &pkgs {
        versions.entry(&p.name).or_default().insert(&p.version);
        if p.targets
            .iter()
            .any(|t| t.kind.iter().any(|k| k == "proc-macro"))
        {
            a.proc_macros.push(p.name.clone());
        }
    }
    a.duplicates = versions
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(k, v)| {
            let mut v: Vec<String> = v.into_iter().map(str::to_owned).collect();
            v.sort_by(|a, b| cmp_versions(a, b));
            (k.to_owned(), v)
        })
        .collect();
    a.proc_macros.sort();
    a.proc_macros.dedup();
    if let Some(r) = &m.resolve {
        for n in &r.nodes {
            let Some(p) = m.package(&n.id) else { continue };
            match p.name.as_str() {
                "tokio" => a.tokio_features.extend(n.features.iter().cloned()),
                "syn" => {
                    a.syn_full |= n.features.iter().any(|f| f == "full");
                    a.syn_versions.push(p.version.clone());
                }
                _ => {}
            }
        }
    }
    a.tokio_features.sort();
    a.tokio_features.dedup();
    a.syn_versions.sort_by(|a, b| cmp_versions(a, b));
    for p in m.members() {
        for d in &p.dependencies {
            if d.kind.as_deref() == Some("dev") {
                continue;
            }
            if d.name == "tokio" && d.features.iter().any(|f| f == "full") {
                a.tokio_full_in.push(p.name.clone());
            }
            if d.name == "async-trait" {
                a.async_trait_in.push(p.name.clone());
            }
            if d.optional || is_test_crate(&p.name) {
                continue;
            }
            let features: Vec<String> = d
                .features
                .iter()
                .filter(|f| is_test_feature(f))
                .cloned()
                .collect();
            if !features.is_empty() {
                a.test_features.push(TestFeature {
                    krate: p.name.clone(),
                    dependency: d.name.clone(),
                    features,
                });
            }
        }
    }
    a.test_features
        .sort_by(|x, y| (&x.krate, &x.dependency).cmp(&(&y.krate, &y.dependency)));
    a.test_features.dedup();
    a.tokio_full_in.dedup();
    a.async_trait_in.dedup();
    a
}

/// Semver order, oldest first: `0.8.8 < 0.9.5 < 0.10.3`, and a pre-release
/// sorts before its release (`1.0.0-alpha.2 < 1.0.0-beta < 1.0.0`). Build
/// metadata (`+spec-1.1.0`) is ignored, with a string compare as the last
/// tie-break so the order is total.
fn cmp_versions(a: &str, b: &str) -> Ordering {
    let (ka, kb) = (version_key(a), version_key(b));
    ka.0.cmp(&kb.0)
        .then_with(|| cmp_pre(ka.1, kb.1))
        .then_with(|| a.cmp(b))
}

/// `1.2.3-rc.1+meta` -> (`[1, 2, 3]`, `rc.1`).
fn version_key(v: &str) -> ([u64; 3], &str) {
    let v = v.split_once('+').map_or(v, |(v, _)| v);
    let (core, pre) = v.split_once('-').unwrap_or((v, ""));
    let mut nums = [0; 3];
    for (n, part) in nums.iter_mut().zip(core.split('.')) {
        *n = part.parse().unwrap_or(0);
    }
    (nums, pre)
}

/// Pre-release order: none is greatest, then dot-separated identifiers,
/// numeric ones as numbers and below alphanumeric ones.
fn cmp_pre(a: &str, b: &str) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    let mut a = a.split('.');
    let mut b = b.split('.');
    loop {
        let o = match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            },
        };
        if o != Ordering::Equal {
            return o;
        }
    }
}

/// Crates whose duplicates are expensive enough to mention.
const HEAVY: &[&str] = &[
    "syn",
    "tokio",
    "hyper",
    "http",
    "reqwest",
    "rustls",
    "ring",
    "aws-lc-sys",
    "aws-lc-rs",
    "openssl-sys",
    "h2",
    "tower",
    "axum",
    "prost",
    "tonic",
    "regex-automata",
    "regex-syntax",
    "serde_json",
    "windows-sys",
    "rand",
    "sqlx-core",
    "tungstenite",
    "base64",
    "itertools",
    "thiserror-impl",
    "darling_core",
    "zerocopy",
];

/// The `uses-async-trait` finding: from the sources when they were read,
/// else from the manifests.
fn async_trait_finding(a: &Analysis) -> Option<Finding> {
    let fix = format!(
        "Replace #[async_trait] with native `async fn` in traits (Rust 1.75+). Where callers spawn the future or need it Send, declare the method as `fn m(&self) -> impl Future<Output = T> + Send` and keep `async fn` in the impls. For traits you hold as `dyn Trait`, use the dynosaur crate (`#[dynosaur::dynosaur(DynX)]`, then `DynX::new_arc`), which boxes only at the dyn boundary. Then ban the macro in clippy.toml:\n{CLIPPY_BAN}\nImpls of upstream #[async_trait] traits keep the macro under a crate-level `#![allow(clippy::disallowed_macros)]` (clippy reports attribute macros at crate level). Tradeoff: a mechanical diff across every impl, and `-> impl Future + Send` signatures are noisier than `async fn`. Measure first with `cargo faster check-cost -p <crate>`."
    );
    let Some(scan) = &a.async_trait else {
        if a.async_trait_in.is_empty() {
            return None;
        }
        return Some(
            Finding::new("uses-async-trait", Severity::Info, format!("{} on async-trait", crates_depend(a.async_trait_in.len())), "the-cost-of-async-trait")
                .saw(format!("Direct dependency in: {}.", a.async_trait_in.join(", ")))
                .why("#[async_trait] gives every method a boxed `dyn Future + Send + 'async_trait` and its own lifetime, which keeps rustc from reusing trait-solving results across bodies.")
                .fix(fix),
        );
    };
    if scan.uses == 0 {
        if a.async_trait_in.is_empty() {
            return None;
        }
        return Some(
            Finding::new("unused-async-trait", Severity::Info, format!("{} on async-trait without using it", crates_depend(a.async_trait_in.len())), "the-cost-of-async-trait")
                .saw(format!("async-trait is a dependency of {}, but no #[async_trait] attribute is in their sources.", a.async_trait_in.join(", ")))
                .why("An unused dependency still has to be built (and its proc macro loaded) before these crates can start.")
                .fix(format!("Drop async-trait from those manifests, and keep it out with clippy.toml:\n{CLIPPY_BAN}")),
        );
    }
    let top: Vec<String> = scan
        .crates
        .iter()
        .take(5)
        .map(|c| format!("{} {}", c.name, c.uses))
        .collect();
    let more = scan.crates.len().saturating_sub(top.len());
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    let defined = match scan.traits.len() {
        0 => "They define no traits of their own.".to_owned(),
        n => format!("They define {n} traits, {}.", dyn_held(scan)),
    };
    let foreign = if scan.foreign_impls > 0 {
        format!(
            " {} are impls of traits defined outside the workspace, which stay until upstream drops the macro.",
            scan.foreign_impls
        )
    } else {
        String::new()
    };
    let sev = if scan.uses >= ASYNC_TRAIT_MANY {
        Severity::Medium
    } else {
        Severity::Low
    };
    Some(
        Finding::new("uses-async-trait", sev, format!("#[async_trait] is used {} times in {} workspace crates", scan.uses, scan.crates.len()), "the-cost-of-async-trait")
            .saw(format!(
                "{} #[async_trait] attributes: {}{more}. {defined}{foreign}",
                scan.uses,
                top.join(", "),
            ))
            .why("Each #[async_trait] method returns `Pin<Box<dyn Future + Send + 'async_trait>>` with lifetimes of its own. rustc proves the body Send while building the box, separately in every impl, and the fresh `'async_trait` lifetimes keep it from reusing those answers. Native async fn in traits gives rustc plain opaque types it can cache.")
            .fix(fix),
    )
}

/// From this many manifest entries on, test features in normal builds are
/// a medium finding.
const TEST_FEATURES_MANY: usize = 5;

/// The `test-features-in-dependencies` finding.
fn test_features_finding(a: &Analysis) -> Option<Finding> {
    let (direct, ws) = (&a.test_features, &a.workspace_test_features);
    if direct.is_empty() && ws.is_empty() {
        return None;
    }
    let crates: BTreeSet<&str> = direct.iter().map(|x| x.krate.as_str()).collect();
    let mut shown: Vec<String> = ws
        .iter()
        .map(|x| {
            format!(
                "[workspace.dependencies] {} [{}] (inherited by {} crates)",
                x.dependency,
                x.features.join(", "),
                x.inherited_by
            )
        })
        .collect();
    shown.extend(direct.iter().take(8).map(|x| {
        format!(
            "{} -> {} [{}]",
            x.krate,
            x.dependency,
            x.features.join(", ")
        )
    }));
    let more = match direct.len().saturating_sub(8) {
        0 => String::new(),
        n => format!(" and {n} more"),
    };
    let inherited: usize = ws.iter().map(|x| x.inherited_by).sum();
    let sev = if direct.len() + inherited >= TEST_FEATURES_MANY {
        Severity::Medium
    } else {
        Severity::Low
    };
    let title = match (ws.len(), direct.len()) {
        (0, n) => format!(
            "{n} [dependencies] entries in {} workspace crates enable test-only features",
            crates.len()
        ),
        (k, 0) => format!("{k} [workspace.dependencies] entries enable test-only features"),
        (k, n) => format!(
            "{k} [workspace.dependencies] and {n} [dependencies] entries enable test-only features"
        ),
    };
    let ws_fix = if ws.is_empty() {
        ""
    } else {
        " For a [workspace.dependencies] entry, drop the feature there and add `features = [...]` to the crates' [dev-dependencies] entries that need it."
    };
    Some(
        Finding::new("test-features-in-dependencies", sev, title, "keep-test-features-out-of-normal-builds")
            .saw(format!("{}{more}.", shown.join(", ")))
            .why("Features are unified across the build, so a test feature enabled from [dependencies] compiles the test helpers (mocks, fakes, fixtures, tokio's test-util) into every normal build of that crate and everything above it, and an edit to a test helper rebuilds libraries instead of only test targets.")
            .fix(format!("If only tests need it, move the feature to [dev-dependencies] (`dep = {{ workspace = true, features = [\"testing\"] }}` there, plain in [dependencies]); cargo turns it on for test builds only.{ws_fix} If another crate's tests need this crate's helpers, give this crate its own `testing = [\"dep/testing\"]` feature and enable it from that crate's [dev-dependencies]. Tradeoff: `cargo test` and `cargo build` then resolve different feature sets, so switching between them rebuilds the affected crates once each. If the feature is needed at runtime (a mock backend you ship, a `fake` data generator), it isn't test-only: leave it.")),
    )
}

/// "3 held as `dyn` (`A`, `B`, `C`)".
fn dyn_held(scan: &Scan) -> String {
    if scan.dyn_traits.is_empty() {
        return "none held as `dyn`".to_owned();
    }
    let names: Vec<String> = scan
        .dyn_traits
        .iter()
        .take(5)
        .map(|t| format!("`{t}`"))
        .collect();
    format!(
        "{} held as `dyn` ({})",
        scan.dyn_traits.len(),
        names.join(", ")
    )
}

/// The deps section.
#[must_use]
pub fn section(a: &Analysis) -> Section {
    let mut s = Section::new("deps");
    s.fact("packages", a.packages.to_string());
    s.fact("proc macros", a.proc_macros.len().to_string());
    s.fact("duplicated crates", a.duplicates.len().to_string());
    if !a.tokio_features.is_empty() {
        s.fact("tokio features", a.tokio_features.join(", "));
    }
    let mut f = Vec::new();
    if !a.tokio_full_in.is_empty() {
        f.push(
            Finding::new("tokio-full", Severity::Low, "tokio's `full` feature is enabled", "dependencies-and-features")
                .saw(format!("{} {} on tokio with features = [\"full\"].", a.tokio_full_in.join(", "), if a.tokio_full_in.len() == 1 { "depends" } else { "depend" }))
                .why("`full` turns on every tokio module (fs, process, signal, io-std, net, macros and more, everything except test-util). Each one adds code to compile, and features are unified across the graph, so one crate asking for `full` makes everyone pay.")
                .fix("List what you use, e.g. features = [\"rt-multi-thread\", \"macros\", \"net\", \"time\", \"sync\"]. The win is a few seconds of tokio build, mostly on cold builds; do it when you touch the manifest anyway."),
        );
    }
    let heavy_dups: Vec<String> = a
        .duplicates
        .iter()
        .filter(|(k, _)| HEAVY.contains(&k.as_str()))
        .map(|(k, v)| format!("{k} {}", v.join(" + ")))
        .collect();
    if !heavy_dups.is_empty() {
        f.push(
            Finding::new("duplicate-heavy-crates", if heavy_dups.len() >= 4 { Severity::Medium } else { Severity::Low }, format!("{} heavy crates are built in more than one version", heavy_dups.len()), "dependencies-and-features")
                .saw(format!("{}. {} crates are duplicated in total.", heavy_dups.join(", "), a.duplicates.len()))
                .why("Each version is a separate crate to compile, and duplicates of big crates (syn, hyper, rustls) sit on the critical path of cold builds.")
                .fix("Run `cargo tree -d -e normal,build` to see who pulls the old version, then upgrade or ask upstream. `cargo update -p <crate>` can sometimes unify semver-compatible ones."),
        );
    }
    if a.syn_versions.iter().any(|v| v.starts_with("1."))
        && a.syn_versions.iter().any(|v| v.starts_with("2."))
    {
        f.push(
            Finding::new("syn-1-and-2", Severity::Low, "Both syn 1 and syn 2 are compiled", "procedural-macros")
                .saw(format!("syn versions: {}.", a.syn_versions.join(", ")))
                .why("syn is the biggest crate behind most proc macros, and every proc macro has to wait for it. Two versions means two long builds early in a cold build.")
                .fix("Find who still uses syn 1 with `cargo tree -i syn@1 -e normal,build` and upgrade those macros."),
        );
    }
    f.extend(test_features_finding(a));
    f.extend(async_trait_finding(a));
    s.extend(f);
    s.data = serde_json::to_value(a).unwrap_or_default();
    s
}

/// `1 workspace crate depends`, `3 workspace crates depend`.
fn crates_depend(n: usize) -> String {
    if n == 1 {
        "1 workspace crate depends".to_owned()
    } else {
        format!("{n} workspace crates depend")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_dups_and_features() {
        let m = Metadata::parse(r#"{
          "workspace_root": "/w", "target_directory": "/w/target", "workspace_members": ["app"],
          "packages": [
            {"id": "app", "name": "app", "version": "0.1.0", "manifest_path": "/w/Cargo.toml",
             "dependencies": [{"name": "tokio", "kind": null, "features": ["full"]}, {"name": "async-trait", "kind": null}]},
            {"id": "tokio", "name": "tokio", "version": "1.48.0", "manifest_path": "/r/t"},
            {"id": "syn1", "name": "syn", "version": "1.0.109", "manifest_path": "/r/s1"},
            {"id": "syn2", "name": "syn", "version": "2.0.100", "manifest_path": "/r/s2"},
            {"id": "serde_derive", "name": "serde_derive", "version": "1.0.0", "manifest_path": "/r/sd",
             "targets": [{"name": "serde_derive", "kind": ["proc-macro"], "src_path": "/r/sd/lib.rs"}]}
          ],
          "resolve": {"nodes": [
            {"id": "app", "deps": []}, {"id": "tokio", "features": ["full", "rt"]},
            {"id": "syn1", "features": ["full"]}, {"id": "syn2"}, {"id": "serde_derive"}
          ]}
        }"#).unwrap();
        let a = analyze(&m);
        assert_eq!(a.packages, 5);
        assert_eq!(a.duplicates["syn"], ["1.0.109", "2.0.100"]);
        assert_eq!(a.proc_macros, ["serde_derive"]);
        assert!(a.syn_full);
        let sec = section(&a);
        let ids: Vec<_> = sec.findings.iter().map(|f| f.id).collect();
        // One crate: "depends", not "depend".
        assert!(sec.findings[0].saw.starts_with("app depends on tokio"));
        assert_eq!(
            sec.findings.last().map(|f| f.title.as_str()),
            Some("1 workspace crate depends on async-trait")
        );
        assert_eq!(
            ids,
            [
                "tokio-full",
                "duplicate-heavy-crates",
                "syn-1-and-2",
                "uses-async-trait"
            ]
        );
    }

    fn test_feature_fixture() -> Metadata {
        Metadata::parse(r#"{
          "workspace_root": "/w", "target_directory": "/w/target",
          "workspace_members": ["core", "bridges", "bin", "core-test-utils"],
          "packages": [
            {"id": "core", "name": "core", "version": "0.1.0", "manifest_path": "/w/core/Cargo.toml",
             "dependencies": [
               {"name": "config", "kind": null, "features": ["testing", "json"]},
               {"name": "tokio", "kind": null, "features": ["rt", "test-util"]},
               {"name": "store", "kind": "dev", "features": ["testing"]},
               {"name": "client", "kind": null, "features": ["mock-server"], "optional": true}
             ]},
            {"id": "bridges", "name": "bridges", "version": "0.1.0", "manifest_path": "/w/bridges/Cargo.toml",
             "dependencies": [
               {"name": "provision", "kind": null, "features": ["testing", "qr"]},
               {"name": "gen", "kind": "build", "features": ["fake"]},
               {"name": "attest", "kind": null, "features": ["attestation", "contest"]}
             ]},
            {"id": "bin", "name": "bin", "version": "0.1.0", "manifest_path": "/w/bin/Cargo.toml",
             "dependencies": [{"name": "core", "kind": "dev", "features": ["testing"]}]},
            {"id": "core-test-utils", "name": "core-test-utils", "version": "0.1.0", "manifest_path": "/w/tu/Cargo.toml",
             "dependencies": [{"name": "core", "kind": null, "features": ["testing"]}]}
          ]
        }"#).unwrap()
    }

    #[test]
    fn workspace_dependencies_test_features_are_reported_once() {
        let mut a = analyze(&test_feature_fixture());
        // tokio's test-util comes from [workspace.dependencies]: one entry
        // there, not one per inheriting crate.
        let root: toml::Table = r#"
            [workspace.dependencies]
            tokio = { version = "1", features = ["rt", "test-util"] }
            cfg = { package = "config", version = "1", features = ["json"] }
            serde = "1"
        "#
        .parse()
        .unwrap();
        attribute_workspace_features(&mut a, &root);
        assert_eq!(
            a.workspace_test_features,
            [WorkspaceTestFeature {
                dependency: "tokio".into(),
                features: vec!["test-util".into()],
                inherited_by: 1,
            }]
        );
        assert!(a.test_features.iter().all(|t| t.dependency != "tokio"));
        let f = test_features_finding(&a).unwrap();
        assert_eq!(
            f.title,
            "1 [workspace.dependencies] and 3 [dependencies] entries enable test-only features"
        );
        assert!(f.saw.starts_with(
            "[workspace.dependencies] tokio [test-util] (inherited by 1 crates), bridges -> gen [fake]"
        ));
        assert!(f.fix.contains("For a [workspace.dependencies] entry"));
    }

    #[test]
    fn flags_test_features_in_normal_dependencies() {
        let a = analyze(&test_feature_fixture());
        let got: Vec<(&str, &str, Vec<&str>)> = a
            .test_features
            .iter()
            .map(|t| {
                (
                    t.krate.as_str(),
                    t.dependency.as_str(),
                    t.features.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("bridges", "gen", vec!["fake"]),
                ("bridges", "provision", vec!["testing"]),
                ("core", "config", vec!["testing"]),
                ("core", "tokio", vec!["test-util"]),
            ]
        );
        let f = test_features_finding(&a).unwrap();
        assert_eq!(
            (f.id, f.severity),
            ("test-features-in-dependencies", Severity::Low)
        );
        assert_eq!(
            f.title,
            "4 [dependencies] entries in 2 workspace crates enable test-only features"
        );
        assert_eq!(
            f.saw,
            "bridges -> gen [fake], bridges -> provision [testing], core -> config [testing], core -> tokio [test-util]."
        );
        assert!(f.fix.contains("[dev-dependencies]"));

        // Many entries: medium, with the rest counted.
        let many: Vec<TestFeature> = (0..10)
            .map(|i| TestFeature {
                krate: format!("c{i}"),
                dependency: "core".into(),
                features: vec!["testing".into()],
            })
            .collect();
        let f = test_features_finding(&Analysis {
            test_features: many,
            ..Analysis::default()
        })
        .unwrap();
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.saw.ends_with(" and 2 more."));
        assert!(test_features_finding(&Analysis::default()).is_none());
    }

    #[test]
    fn test_feature_names() {
        for f in [
            "testing",
            "test",
            "test-util",
            "test-utils",
            "test_helpers",
            "testutils",
            "mock",
            "mocks",
            "mock-server",
            "fake",
            "faker",
            "Testing",
        ] {
            assert!(is_test_feature(f), "{f}");
        }
        for f in [
            "attestation",
            "contest",
            "testnet-free",
            "default",
            "rustls-tls",
            "full",
        ] {
            assert!(!is_test_feature(f), "{f}");
        }
        for c in ["core-test-utils", "tala_testing", "mock-server", "fixtures"] {
            assert!(is_test_crate(c), "{c}");
        }
        for c in ["core", "attestation", "contest", "testnet-node"] {
            assert!(!is_test_crate(c), "{c}");
        }
    }

    #[test]
    fn versions_sort_by_semver_oldest_first() {
        let mut v = vec![
            "0.10.3",
            "0.8.8",
            "0.9.5",
            "1.0.0",
            "1.0.0-rc.1",
            "1.0.0-alpha.10",
            "1.0.0-alpha.2",
            "1.0.0-alpha",
            "1.0.0-alpha.beta",
            "0.9.12+spec-1.1.0",
            "10.0.0",
            "2.0.0",
        ];
        v.sort_by(|a, b| cmp_versions(a, b));
        assert_eq!(
            v,
            [
                "0.8.8",
                "0.9.5",
                "0.9.12+spec-1.1.0",
                "0.10.3",
                "1.0.0-alpha",
                "1.0.0-alpha.2",
                "1.0.0-alpha.10",
                "1.0.0-alpha.beta",
                "1.0.0-rc.1",
                "1.0.0",
                "2.0.0",
                "10.0.0",
            ]
        );
    }

    #[test]
    fn duplicates_and_syn_versions_list_oldest_first() {
        let m = Metadata::parse(
            r#"{
          "workspace_root": "/w", "target_directory": "/w/target", "workspace_members": ["app"],
          "packages": [
            {"id": "app", "name": "app", "version": "0.1.0", "manifest_path": "/w/Cargo.toml"},
            {"id": "r10", "name": "rand", "version": "0.10.3", "manifest_path": "/r/r10"},
            {"id": "r8", "name": "rand", "version": "0.8.8", "manifest_path": "/r/r8"},
            {"id": "r9", "name": "rand", "version": "0.9.5", "manifest_path": "/r/r9"},
            {"id": "s10", "name": "syn", "version": "10.0.0", "manifest_path": "/r/s10"},
            {"id": "s2", "name": "syn", "version": "2.0.100", "manifest_path": "/r/s2"}
          ],
          "resolve": {"nodes": [
            {"id": "app"}, {"id": "r10"}, {"id": "r8"}, {"id": "r9"}, {"id": "s10"}, {"id": "s2"}
          ]}
        }"#,
        )
        .unwrap();
        let a = analyze(&m);
        assert_eq!(a.duplicates["rand"], ["0.8.8", "0.9.5", "0.10.3"]);
        assert_eq!(a.syn_versions, ["2.0.100", "10.0.0"]);
        let s = section(&a);
        let f = s
            .findings
            .iter()
            .find(|f| f.id == "duplicate-heavy-crates")
            .unwrap();
        assert!(
            f.saw
                .starts_with("rand 0.8.8 + 0.9.5 + 0.10.3, syn 2.0.100 + 10.0.0.")
        );
    }

    #[test]
    fn async_trait_finding_counts_uses_not_manifests() {
        let mut a = Analysis {
            async_trait_in: vec!["app".into(), "lib".into()],
            ..Analysis::default()
        };
        let finding = |a: &Analysis| {
            async_trait_finding(a).map(|f| (f.id, f.severity, f.title, f.saw, f.fix))
        };

        // Manifests only: an info note.
        let (id, sev, title, ..) = finding(&a).unwrap();
        assert_eq!((id, sev), ("uses-async-trait", Severity::Info));
        assert_eq!(title, "2 workspace crates depend on async-trait");

        // Sources read, nothing uses it: drop the dependency.
        a.async_trait = Some(Scan::default());
        assert_eq!(finding(&a).unwrap().0, "unused-async-trait");

        // A few uses: low; many: medium, with the clippy ban.
        let scan = |n: usize| {
            let files: Vec<(String, String)> = (0..n)
                .map(|i| {
                    (
                        "app".to_owned(),
                        format!("#[async_trait]\nimpl Store for S{i} {{}}\n"),
                    )
                })
                .chain([(
                    "lib".to_owned(),
                    "#[async_trait]\npub trait Store {}\nfn f(_: &dyn Store) {}\n".to_owned(),
                )])
                .collect();
            asynctrait::aggregate(&files)
        };
        a.async_trait = Some(scan(3));
        let (id, sev, title, saw, _) = finding(&a).unwrap();
        assert_eq!((id, sev), ("uses-async-trait", Severity::Low));
        assert_eq!(
            title,
            "#[async_trait] is used 4 times in 2 workspace crates"
        );
        assert_eq!(
            saw,
            "4 #[async_trait] attributes: app 3, lib 1. They define 1 traits, 1 held as `dyn` (`Store`)."
        );
        a.async_trait = Some(scan(ASYNC_TRAIT_MANY));
        let (_, sev, _, _, fix) = finding(&a).unwrap();
        assert_eq!(sev, Severity::Medium);
        assert!(fix.contains(CLIPPY_BAN));
        assert!(fix.contains("dynosaur"));

        // No dependency and no uses: nothing to say.
        assert!(
            async_trait_finding(&Analysis {
                async_trait: Some(Scan::default()),
                ..Analysis::default()
            })
            .is_none()
        );
    }
}
