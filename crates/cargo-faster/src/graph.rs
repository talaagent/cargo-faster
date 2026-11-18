//! The workspace graph: what an edit to one crate rebuilds, and which
//! crates are big enough to split.

use crate::context::{Metadata, Package};
use crate::finding::{Finding, Section, Severity, table};
use alloc::collections::{BTreeMap, BTreeSet};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub mod edges;

/// A workspace crate and what depends on it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CrateInfo {
    /// Package name.
    pub name: String,
    /// Workspace crates that rebuild when this one changes (normal and
    /// build dependencies, transitively).
    pub rebuilds: Vec<String>,
    /// Workspace crates that use it only as a dev-dependency (their tests
    /// rebuild).
    pub dev_rebuilds: Vec<String>,
    /// Lines of Rust in its `src/` (or next to its targets).
    pub lines: usize,
    /// The biggest top-level modules: `(path, lines)`.
    pub modules: Vec<(String, usize)>,
}

/// Reverse dependencies within the workspace: member name -> direct
/// dependents, split by normal/build vs dev.
#[must_use]
pub fn reverse_deps(
    m: &Metadata,
) -> (
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, BTreeSet<String>>,
) {
    let members: HashMap<&str, &str> = m
        .members()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();
    let mut normal: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut dev: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for name in members.values() {
        normal.entry((*name).to_owned()).or_default();
    }
    if let Some(r) = &m.resolve {
        for node in &r.nodes {
            let Some(&from) = members.get(node.id.as_str()) else {
                continue;
            };
            for d in &node.deps {
                let Some(&to) = members.get(d.pkg.as_str()) else {
                    continue;
                };
                let is_dev_only = !d.dep_kinds.is_empty()
                    && d.dep_kinds.iter().all(|k| k.kind.as_deref() == Some("dev"));
                let map = if is_dev_only { &mut dev } else { &mut normal };
                map.entry(to.to_owned())
                    .or_default()
                    .insert(from.to_owned());
            }
        }
    }
    (normal, dev)
}

/// Everything that transitively depends on `name`.
#[must_use]
pub fn transitive(rdeps: &BTreeMap<String, BTreeSet<String>>, name: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![name.to_owned()];
    while let Some(n) = stack.pop() {
        for d in rdeps.get(&n).into_iter().flatten() {
            if seen.insert(d.clone()) {
                stack.push(d.clone());
            }
        }
    }
    seen
}

/// Count lines of `.rs` files under a directory, and per top-level entry.
#[must_use]
pub fn count_lines(dir: &Path) -> (usize, Vec<(String, usize)>) {
    fn walk(p: &Path) -> usize {
        if p.is_dir() {
            std::fs::read_dir(p).map_or(0, |d| d.flatten().map(|e| walk(&e.path())).sum())
        } else if p.extension().is_some_and(|e| e == "rs") {
            std::fs::read(p).map_or(0, |b| b.split(|&c| c == b'\n').count().saturating_sub(1))
        } else {
            0
        }
    }
    let mut modules: Vec<(String, usize)> = std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        walk(&e.path()),
                    )
                })
                .filter(|(_, n)| *n > 0)
                .collect()
        })
        .unwrap_or_default();
    modules.sort_by_key(|m| core::cmp::Reverse(m.1));
    (modules.iter().map(|m| m.1).sum(), modules)
}

pub(crate) fn src_dir(p: &Package) -> PathBuf {
    let root = p.manifest_path.parent().unwrap_or(Path::new("."));
    let src = root.join("src");
    if src.is_dir() {
        return src;
    }
    p.targets
        .iter()
        .find(|t| t.kind.iter().any(|k| k == "lib"))
        .and_then(|t| t.src_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.to_path_buf())
}

/// Gather crate info for all members (reads source files).
#[must_use]
pub fn gather(m: &Metadata) -> Vec<CrateInfo> {
    let (normal, dev) = reverse_deps(m);
    let mut out: Vec<CrateInfo> = m
        .members()
        .map(|p| {
            let (lines, modules) = count_lines(&src_dir(p));
            let rebuilds = transitive(&normal, &p.name);
            // Crates whose tests dev-depend on this crate or on anything
            // that rebuilds with it.
            let dev_rebuilds: Vec<String> = core::iter::once(&p.name)
                .chain(rebuilds.iter())
                .flat_map(|n| dev.get(n).into_iter().flatten())
                .filter(|d| !rebuilds.contains(*d) && **d != p.name)
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            CrateInfo {
                name: p.name.clone(),
                rebuilds: rebuilds.into_iter().collect(),
                dev_rebuilds,
                lines,
                modules: modules.into_iter().take(8).collect(),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.rebuilds
            .len()
            .cmp(&a.rebuilds.len())
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// A crate this big gets a split suggestion.
pub const BIG_CRATE_LINES: usize = 40_000;

/// Whether `name` names `krate` (`_` and `-` are the same).
#[must_use]
pub fn is_named(krate: &CrateInfo, name: &str) -> bool {
    name == krate.name || name.replace('_', "-") == krate.name
}

/// Put the configuration's hot crates (the ones edited most) first: a
/// table of what an edit to each rebuilds, and their findings ahead of the
/// rest.
pub fn rank_hot(s: &mut Section, crates: &[CrateInfo], hot: &[String]) {
    if hot.is_empty() {
        return;
    }
    let is_hot = |n: &str| hot.iter().any(|h| crate::config::glob(h, n));
    let rows: Vec<Vec<String>> = hot
        .iter()
        .filter_map(|h| crates.iter().find(|c| is_named(c, h)))
        .map(|c| {
            vec![
                c.name.clone(),
                c.rebuilds.len().to_string(),
                c.lines.to_string(),
            ]
        })
        .collect();
    s.details.insert(
        0,
        format!(
            "hot crates (`hot` in the configuration)\n{}",
            table(&["crate", "an edit rebuilds", "lines"], &rows)
        ),
    );
    s.findings
        .sort_by_key(|f| !f.krate.as_deref().is_some_and(is_hot));
}

/// The rdeps section, for one crate or the whole workspace.
#[must_use]
pub fn section(crates: &[CrateInfo], only: Option<&str>) -> Section {
    let mut s = Section::new("rdeps");
    let members = crates.len();
    s.fact("workspace crates", members.to_string());
    let shown: Vec<&CrateInfo> = crates
        .iter()
        .filter(|c| only.is_none_or(|o| is_named(c, o)))
        .collect();
    if let Some(c) = shown.first().filter(|_| only.is_some()) {
        s.fact("crate", format!("{} ({} lines)", c.name, c.lines));
        let n = c.rebuilds.len();
        let noun = if n == 1 { "crate" } else { "crates" };
        s.fact("an edit rebuilds", format!("{n} other workspace {noun}"));
        if !c.rebuilds.is_empty() {
            s.details.push(format!(
                "rebuilt when {} changes\n  {}",
                c.name,
                c.rebuilds.join("\n  ")
            ));
        }
        if !c.dev_rebuilds.is_empty() {
            s.details.push(format!(
                "tests that also rebuild (dev-dependencies)\n  {}",
                c.dev_rebuilds.join("\n  ")
            ));
        }
        if !c.modules.is_empty() {
            let rows: Vec<Vec<String>> = c
                .modules
                .iter()
                .map(|(m, n)| vec![n.to_string(), m.clone()])
                .collect();
            s.details.push(format!(
                "largest modules\n{}",
                table(&["lines", "module"], &rows)
            ));
        }
    } else {
        let rows: Vec<Vec<String>> = shown
            .iter()
            .take(15)
            .map(|c| {
                vec![
                    c.rebuilds.len().to_string(),
                    c.lines.to_string(),
                    c.name.clone(),
                ]
            })
            .collect();
        s.details.push(format!(
            "crates whose edits rebuild the most\n{}",
            table(&["rebuilds others", "lines", "crate"], &rows)
        ));
    }
    let mut f = Vec::new();
    for c in &shown {
        let fanout = c.rebuilds.len();
        let generated = c.modules.first().is_some_and(|(m, n)| {
            let m = m.trim_end_matches(".rs");
            matches!(m, "gen" | "generated" | "proto" | "pb") && n * 5 >= c.lines * 4
        });
        if c.lines >= BIG_CRATE_LINES && generated {
            let (m, n) = &c.modules[0];
            f.push(
                Finding::new("big-generated-crate", Severity::Low, format!("{} is {} lines, mostly generated code", c.name, c.lines), "splitting-a-big-crate")
                    .krate(&c.name)
                    .saw(format!("{n} of {}'s {} lines are in {m}.", c.name, c.lines))
                    .why("Generated code (protobuf, OpenAPI, bindings) compiles like hand-written code. It rarely changes, so it mostly costs cold builds and the crates that wait on it.")
                    .fix("Keep it in its own crate (it already is), generate only the services and messages you use, and make sure nothing that changes often depends on it unnecessarily. A shared cache makes it nearly free after the first build."),
            );
        } else if c.lines >= BIG_CRATE_LINES {
            let mods: Vec<String> = c
                .modules
                .iter()
                .take(4)
                .map(|(m, n)| format!("{m} ({n} lines)"))
                .collect();
            f.push(
                Finding::new("big-crate", if c.lines >= 2 * BIG_CRATE_LINES { Severity::Medium } else { Severity::Low }, format!("{} has {} lines of Rust", c.name, c.lines), "splitting-a-big-crate")
                    .krate(&c.name)
                    .saw(format!("{} is {} lines; its biggest modules are {}.", c.name, c.lines, mods.join(", ")))
                    .why("rustc checks a crate as one unit: an edit anywhere re-runs type checking for the whole crate (incremental helps, but less than you'd hope with async code), and one crate can't use more than a few cores.")
                    .fix(format!("Move self-contained modules into their own crates, starting with ones that don't depend on the rest (often {}). Each split crate builds in parallel and is skipped when you edit elsewhere.", c.modules.first().map_or("the biggest", |m| m.0.as_str()))),
            );
        }
        if members >= 6 && fanout * 2 >= members && fanout >= 5 && c.lines >= 2_000 {
            f.push(
                Finding::new("high-fanout", Severity::Low, format!("Editing {} rebuilds {fanout} other workspace crates (of {members})", c.name), "rebuild-fan-out")
                    .krate(&c.name)
                    .saw(format!("{fanout} workspace crates depend on {} directly or indirectly.", c.name))
                    .why("Every change to a crate near the bottom of the graph rebuilds everything above it, so its churn decides the cost of most edits.")
                    .fix(format!("Keep {} small and stable: move fast-changing code out of it, or split the parts few crates need into their own crate.", c.name)),
            );
        }
    }
    if only.is_none() {
        f.truncate(6);
    }
    s.extend(f);
    s.data = serde_json::to_value(&shown).unwrap_or_default();
    s
}

/// Add the edges into the crates shown that are cheap to cut: every direct
/// dependent of `only`, or of the crates whose edits rebuild the most,
/// whatever their size.
/// Reads the dependents' sources.
pub fn add_edges(m: &Metadata, crates: &[CrateInfo], only: Option<&str>, s: &mut Section) {
    let targets: Vec<&str> = match only {
        Some(o) => crates
            .iter()
            .filter(|c| is_named(c, o))
            .map(|c| c.name.as_str())
            .collect(),
        None => crates
            .iter()
            .take(15)
            .filter(|c| c.rebuilds.len() >= edges::MIN_SAVED)
            .map(|c| c.name.as_str())
            .collect(),
    };
    let found = edges::gather(m, &edges::into_crates(m, &targets));
    if let Some(d) = edges::detail(&found, only.is_some()) {
        s.details.push(d);
    }
    s.extend(edges::findings(&found, if only.is_some() { 5 } else { 3 }));
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = r#"{
      "workspace_root": "/w", "target_directory": "/w/target",
      "workspace_members": ["core 0.1", "mid 0.1", "app 0.1", "tests 0.1"],
      "packages": [
        {"id": "core 0.1", "name": "core", "version": "0.1.0", "manifest_path": "/w/core/Cargo.toml"},
        {"id": "mid 0.1", "name": "mid", "version": "0.1.0", "manifest_path": "/w/mid/Cargo.toml"},
        {"id": "app 0.1", "name": "app", "version": "0.1.0", "manifest_path": "/w/app/Cargo.toml"},
        {"id": "tests 0.1", "name": "tests", "version": "0.1.0", "manifest_path": "/w/tests/Cargo.toml"},
        {"id": "serde 1", "name": "serde", "version": "1.0.0", "manifest_path": "/r/serde/Cargo.toml"}
      ],
      "resolve": {"nodes": [
        {"id": "mid 0.1", "deps": [{"pkg": "core 0.1", "dep_kinds": [{"kind": null}]}, {"pkg": "serde 1", "dep_kinds": [{"kind": null}]}]},
        {"id": "app 0.1", "deps": [{"pkg": "mid 0.1", "dep_kinds": [{"kind": null}]}]},
        {"id": "tests 0.1", "deps": [{"pkg": "app 0.1", "dep_kinds": [{"kind": "dev"}]}]}
      ]}
    }"#;

    #[test]
    fn rebuild_sets() {
        let m = Metadata::parse(META).unwrap();
        let (normal, dev) = reverse_deps(&m);
        assert_eq!(
            transitive(&normal, "core").into_iter().collect::<Vec<_>>(),
            ["app", "mid"]
        );
        assert!(dev["app"].contains("tests"));
        let crates = gather(&m);
        let core = crates.iter().find(|c| c.name == "core").unwrap();
        assert_eq!(core.rebuilds, ["app", "mid"]);
        assert_eq!(core.dev_rebuilds, ["tests"]);
        let s = section(&crates, Some("core"));
        assert!(s.details[0].contains("mid"));
    }

    #[test]
    fn big_crate_finding() {
        let c = CrateInfo {
            name: "app".into(),
            rebuilds: vec![],
            dev_rebuilds: vec![],
            lines: 90_000,
            modules: vec![("bot".into(), 30_000)],
        };
        let s = section(&[c], None);
        assert_eq!(s.findings[0].id, "big-crate");
        assert_eq!(s.findings[0].severity, Severity::Medium);
    }
}
