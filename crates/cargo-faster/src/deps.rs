//! Dependencies: heavy feature sets, duplicate versions, proc macros.

use crate::asynctrait::{self, Scan};
use crate::context::Metadata;
use crate::finding::{Finding, Section, Severity};
use alloc::collections::{BTreeMap, BTreeSet};
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
}

/// [`analyze`], plus a scan of the workspace's sources for `#[async_trait]`.
#[must_use]
pub fn analyze_with_sources(m: &Metadata) -> Analysis {
    let mut a = analyze(m);
    a.async_trait = Some(asynctrait::scan_workspace(m));
    a
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
            v.sort_by_key(|s| version_key(s));
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
    a.syn_versions.sort();
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
        }
    }
    a.tokio_full_in.dedup();
    a.async_trait_in.dedup();
    a
}

/// Sort key for `1.2.3`-style versions: numeric parts compare as numbers.
fn version_key(v: &str) -> Vec<u64> {
    v.split(['.', '-', '+'])
        .map(|p| p.parse().unwrap_or(0))
        .collect()
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
            Finding::new("uses-async-trait", Severity::Info, format!("{} workspace crates depend on async-trait", a.async_trait_in.len()), "the-cost-of-async-trait")
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
            Finding::new("unused-async-trait", Severity::Info, format!("{} workspace crates depend on async-trait without using it", a.async_trait_in.len()), "the-cost-of-async-trait")
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
                .saw(format!("{} depend on tokio with features = [\"full\"].", a.tokio_full_in.join(", ")))
                .why("`full` turns on every tokio module (fs, process, signal, io-std, test-util-free parts and more). Each one adds code to compile, and features are unified across the graph, so one crate asking for `full` makes everyone pay.")
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
    f.extend(async_trait_finding(a));
    s.extend(f);
    s.data = serde_json::to_value(a).unwrap_or_default();
    s
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
        let ids: Vec<_> = section(&a).findings.iter().map(|f| f.id).collect();
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
