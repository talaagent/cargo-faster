//! Dependencies: heavy feature sets, duplicate versions, proc macros.

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
}

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
        .map(|(k, v)| (k.to_owned(), v.into_iter().map(str::to_owned).collect()))
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
    if !a.async_trait_in.is_empty() {
        f.push(
            Finding::new("uses-async-trait", Severity::Info, format!("{} workspace crates use async-trait", a.async_trait_in.len()), "the-cost-of-async-trait")
                .saw(format!("Direct dependency in: {}.", a.async_trait_in.join(", ")))
                .why("#[async_trait] expands every method into a boxed future plus a Send proof. That's cheap per method but can add up in crates with many impls.")
                .fix("`cargo faster check-cost -p <crate>` shows how much time 'async_trait obligations take. Native `async fn` in traits works without the macro when you don't need `dyn Trait`."),
        );
    }
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
}
