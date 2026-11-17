//! Where a build's time went: by kind of unit, by what the cache wrapper
//! did, how much went to linking, how much slower units ran than in an
//! earlier report, and how many workspace crates the build recompiled.

use super::wrapper::Outcome;
use super::{Kind, Timings, Unit};
use alloc::collections::{BTreeMap, BTreeSet};
use serde::Serialize;
use std::collections::HashMap;

/// Time spent on one kind of unit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KindTime {
    /// The kind.
    pub kind: Kind,
    /// Units of this kind that compiled.
    pub units: usize,
    /// Their summed durations, seconds.
    pub busy: f64,
    /// Seconds of the critical path spent on them.
    pub on_path: f64,
}

/// Seconds by kind of unit, biggest first.
pub(super) fn kinds(ran: &[&Unit], path: &[(usize, f64)], units: &[Unit]) -> Vec<KindTime> {
    let mut out: Vec<KindTime> = Vec::new();
    for k in [Kind::Check, Kind::Lib, Kind::Link, Kind::BuildScript] {
        let of: Vec<&&Unit> = ran.iter().filter(|u| u.kind() == k).collect();
        if of.is_empty() {
            continue;
        }
        out.push(KindTime {
            kind: k,
            units: of.len(),
            busy: of.iter().map(|u| u.duration).sum(),
            on_path: path
                .iter()
                .filter(|(p, _)| units[*p].kind() == k)
                .map(|(_, d)| d)
                .sum(),
        });
    }
    out.sort_by(|a, b| b.busy.total_cmp(&a.busy));
    out
}

/// Binaries and tests: the units that link.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Linked {
    /// How many compiled and linked.
    pub units: usize,
    /// Their summed durations, seconds.
    pub busy: f64,
    /// Seconds between the last other unit finishing and the build ending:
    /// time spent only on linking units.
    pub tail: f64,
    /// The slowest of them, seconds.
    pub slowest: Vec<(String, f64)>,
}

pub(super) fn linked(ran: &[&Unit], end: f64) -> Linked {
    let mut links: Vec<&&Unit> = ran.iter().filter(|u| u.kind() == Kind::Link).collect();
    if links.is_empty() {
        return Linked::default();
    }
    links.sort_by(|a, b| b.duration.total_cmp(&a.duration));
    let others_end = ran
        .iter()
        .filter(|u| u.kind() != Kind::Link)
        .map(|u| u.end())
        .fold(0.0, f64::max);
    Linked {
        units: links.len(),
        busy: links.iter().map(|u| u.duration).sum(),
        tail: (end - others_end).max(0.0),
        slowest: links
            .iter()
            .take(5)
            .map(|u| (u.label(), u.duration))
            .collect(),
    }
}

/// Units by what the rustc wrapper did with them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct WrapperSummary {
    /// Compiled units the wrapper has a record of.
    pub matched: usize,
    /// Compiled units it has none for (build script runs aside).
    pub unmatched: usize,
    /// Restored from the cache: count and seconds.
    pub hits: (usize, f64),
    /// Cache misses rustc compiled: count and seconds.
    pub misses: (usize, f64),
    /// Incremental passthroughs: count and seconds.
    pub incremental: (usize, f64),
    /// Other passthroughs: count and seconds.
    pub other: (usize, f64),
    /// Seconds spent waiting for the wrapper's scheduler.
    pub wait: f64,
}

pub(super) fn wrapper_summary(ran: &[&Unit]) -> Option<WrapperSummary> {
    let mut s = WrapperSummary::default();
    for u in ran.iter().filter(|u| u.kind() != Kind::BuildScript) {
        let Some(w) = &u.wrapper else {
            s.unmatched += 1;
            continue;
        };
        s.matched += 1;
        s.wait += w.wait;
        let slot = match w.outcome {
            Outcome::Hit => &mut s.hits,
            Outcome::Miss => &mut s.misses,
            Outcome::Incr => &mut s.incremental,
            Outcome::Pass => &mut s.other,
        };
        slot.0 += 1;
        slot.1 += u.duration;
    }
    (s.matched > 0).then_some(s)
}

/// How much slower units ran than in an earlier report.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Slowdown {
    /// The earlier report's wall time, seconds.
    pub baseline_wall: f64,
    /// Whether the earlier report is a `cargo check`.
    pub baseline_check: bool,
    /// Units compiled in both.
    pub compared: usize,
    /// Median of now / before.
    pub median: f64,
    /// Summed seconds over the earlier times.
    pub excess: f64,
    /// The biggest slowdowns: unit, seconds now, seconds before.
    pub top: Vec<(String, f64, f64)>,
}

/// Compare compiled lib and check units with the same crate's unit in
/// `before`. A library built here against a check there compares the part
/// up to its metadata, which is all a check does (when the report splits
/// it; a cache wrapper can hide the split, and then codegen is included).
pub(super) fn slowdown(ran: &[&Unit], before: &Timings) -> Option<Slowdown> {
    let base_ran: Vec<&Unit> = before.ran().collect();
    let base_check = base_ran
        .iter()
        .all(|u| matches!(u.kind(), Kind::Check | Kind::BuildScript));
    let mut prev: HashMap<(&str, Kind), f64> = HashMap::new();
    for u in &base_ran {
        if matches!(u.kind(), Kind::Check | Kind::Lib) {
            prev.insert((u.name.as_str(), u.kind()), u.duration);
        }
    }
    let mut rows: Vec<(String, f64, f64)> = ran
        .iter()
        .filter_map(|u| {
            let k = u.kind();
            if let Some(&b) = prev.get(&(u.name.as_str(), k)) {
                return Some((u.label(), u.duration, b));
            }
            let b = *prev.get(&(u.name.as_str(), Kind::Check))?;
            (k == Kind::Lib).then(|| (u.label(), u.rmeta_end() - u.start, b))
        })
        .filter(|(_, _, b)| *b >= 1.0)
        .collect();
    if rows.is_empty() {
        return None;
    }
    let mut ratios: Vec<f64> = rows.iter().map(|(_, n, b)| n / b).collect();
    ratios.sort_by(f64::total_cmp);
    rows.sort_by(|x, y| (y.1 - y.2).total_cmp(&(x.1 - x.2)));
    let wall = before.ran().map(Unit::end).fold(0.0, f64::max);
    Some(Slowdown {
        baseline_wall: wall,
        baseline_check: base_check,
        compared: rows.len(),
        median: ratios[ratios.len() / 2],
        excess: rows.iter().map(|(_, n, b)| (n - b).max(0.0)).sum(),
        top: rows.into_iter().take(5).collect(),
    })
}

/// The workspace's crates and their direct workspace dependencies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    /// Member package name → the members it depends on (normal deps).
    pub deps: BTreeMap<String, BTreeSet<String>>,
}

impl Workspace {
    /// From `cargo metadata`.
    #[must_use]
    pub fn from_metadata(m: &crate::context::Metadata) -> Self {
        let (rdeps, _) = crate::graph::reverse_deps(m);
        let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (dep, users) in rdeps {
            deps.entry(dep.clone()).or_default();
            for u in users {
                deps.entry(u).or_default().insert(dep.clone());
            }
        }
        Self { deps }
    }
}

/// Workspace crates the build recompiled.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Rebuilt {
    /// How many members compiled.
    pub crates: usize,
    /// Those with no recompiled workspace dependency: where the edit was.
    pub roots: Vec<String>,
    /// Summed seconds of the members' units.
    pub busy: f64,
}

pub(super) fn rebuilt(ran: &[&Unit], ws: &Workspace) -> Option<Rebuilt> {
    let compiled: BTreeSet<&str> = ran
        .iter()
        .filter(|u| u.kind() != Kind::BuildScript && ws.deps.contains_key(&u.name))
        .map(|u| u.name.as_str())
        .collect();
    if compiled.is_empty() {
        return None;
    }
    let roots = compiled
        .iter()
        .filter(|c| {
            ws.deps
                .get(**c)
                .is_none_or(|d| !d.iter().any(|d| compiled.contains(d.as_str())))
        })
        .map(|c| (*c).to_owned())
        .collect();
    Some(Rebuilt {
        crates: compiled.len(),
        roots,
        busy: ran
            .iter()
            .filter(|u| compiled.contains(u.name.as_str()))
            .map(|u| u.duration)
            .sum(),
    })
}
