//! What a report says: wall time, the critical path, units that ran mostly
//! alone, long code generation, time by kind of unit, linking, slowdowns
//! against an earlier report and the workspace crates a build recompiled.

use crate::{Jobs, Kind, Timings, Unit, UnitOf};
use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// What a report says.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Analysis {
    /// Wall time of the build, seconds.
    #[serde(rename = "wall_s")]
    pub wall: f64,
    /// Units compiled (not fresh).
    #[serde(rename = "units_compiled")]
    pub compiled: usize,
    /// Units cargo found fresh.
    pub units_fresh: usize,
    /// Units in the build.
    #[serde(rename = "units_total")]
    pub total_units: usize,
    /// Sum of unit durations over wall time.
    pub parallelism: f64,
    /// Average CPU use, percent, if sampled.
    #[serde(rename = "cpu_avg_pct")]
    pub cpu_avg: Option<f64>,
    /// Cargo's `jobs` and the CPU count, from the report.
    pub jobs: Option<Jobs>,
    /// Unit-seconds compiled units spent ready (whatever unblocked them
    /// done) before they started: waiting for a free job, or for a token
    /// from a jobserver. Divided by `wall`, the units ready on average.
    #[serde(rename = "queued_s", default)]
    pub queued: f64,
    /// The chain of units that decided the wall time, first to last.
    pub critical_path: Vec<PathStep>,
    /// Seconds of the critical path between a step's dependency finishing
    /// and the step starting.
    #[serde(rename = "path_gaps_s")]
    pub path_gaps: f64,
    /// Long units that ran mostly alone.
    pub serial: Vec<Serial>,
    /// Slowest units.
    #[serde(rename = "slowest_units")]
    pub slowest: Vec<UnitTime>,
    /// Built units with long code generation (`part`: codegen seconds).
    pub long_codegen: Vec<UnitTime>,
    /// Check units with a long part after their metadata (`part`: seconds
    /// after metadata).
    pub post_metadata: Vec<UnitTime>,
    /// Slow build scripts.
    pub build_scripts: Vec<UnitTime>,
    /// Seconds by kind of unit.
    pub kinds: Vec<KindTime>,
    /// Binaries and tests.
    pub linked: Linked,
    /// How much slower units ran than in an earlier report.
    pub slowdown: Option<Slowdown>,
    /// Workspace crates recompiled, when the workspace is known.
    pub rebuilt: Option<Rebuilt>,
}

/// A unit and how long it took.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UnitTime {
    /// Unit label.
    pub unit: String,
    /// What it is.
    #[serde(flatten)]
    pub of: UnitOf,
    /// Seconds it took.
    #[serde(rename = "took_s")]
    pub duration: f64,
    /// The seconds the list is about (codegen, after metadata), when it
    /// isn't the whole unit.
    #[serde(rename = "part_s", default, skip_serializing_if = "Option::is_none")]
    pub part: Option<f64>,
}

impl UnitTime {
    fn new(u: &Unit, part: Option<f64>) -> Self {
        Self {
            unit: u.label(),
            of: u.of(),
            duration: u.duration,
            part,
        }
    }
}

/// What a step of the critical path waited for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Waited {
    /// The previous step finishing (or nothing that compiled).
    #[default]
    Full,
    /// The previous step's metadata (pipelined).
    Meta,
}

/// One step of the critical path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PathStep {
    /// Unit label.
    pub unit: String,
    /// What it is.
    #[serde(flatten)]
    pub of: UnitOf,
    /// Start, seconds into the build.
    #[serde(rename = "start_s")]
    pub start: f64,
    /// Duration, seconds.
    #[serde(rename = "took_s")]
    pub duration: f64,
    /// Whether it waited for the previous step's metadata or all of it.
    pub waited: Waited,
    /// Seconds between what it waited for finishing and it starting.
    #[serde(rename = "gap_s")]
    pub gap: f64,
}

/// A unit that ran mostly alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Serial {
    /// Unit label.
    pub unit: String,
    /// What it is.
    #[serde(flatten)]
    pub of: UnitOf,
    /// Duration, seconds.
    #[serde(rename = "took_s")]
    pub duration: f64,
    /// Average number of other units running alongside.
    pub others: f64,
}

/// Time spent on one kind of unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct KindTime {
    /// The kind.
    pub kind: Kind,
    /// Units of this kind that compiled.
    pub units: usize,
    /// Their summed durations, seconds.
    #[serde(rename = "busy_s")]
    pub busy: f64,
    /// Seconds of the critical path spent on them.
    #[serde(rename = "on_path_s")]
    pub on_path: f64,
}

/// A unit label and its seconds (and, in a comparison, its seconds in the
/// earlier report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UnitSecs {
    /// Unit label.
    pub unit: String,
    /// Seconds it took.
    #[serde(rename = "took_s")]
    pub took: f64,
    /// Seconds it took in the earlier report.
    #[serde(rename = "before_s", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<f64>,
}

/// Binaries and tests: the units that link.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Linked {
    /// How many compiled and linked.
    pub units: usize,
    /// Their summed durations, seconds.
    #[serde(rename = "busy_s")]
    pub busy: f64,
    /// Seconds between the last other unit finishing and the build ending:
    /// time spent only on linking units.
    #[serde(rename = "tail_s")]
    pub tail: f64,
    /// The slowest five of them.
    pub slowest: Vec<UnitSecs>,
}

/// How much slower units ran than in an earlier report.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Slowdown {
    /// The earlier report's wall time, seconds.
    #[serde(rename = "baseline_wall_s")]
    pub baseline_wall: f64,
    /// Whether the earlier report is a `cargo check`.
    pub baseline_check: bool,
    /// Units compiled in both.
    pub compared: usize,
    /// Median of now / before.
    pub median: f64,
    /// Summed seconds over the earlier times.
    #[serde(rename = "excess_s")]
    pub excess: f64,
    /// The five biggest slowdowns, with their seconds before.
    pub top: Vec<UnitSecs>,
}

/// The workspace's crates and their direct workspace dependencies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Member package name → the members it depends on (normal deps).
    pub deps: BTreeMap<String, BTreeSet<String>>,
}

/// Workspace crates the build recompiled.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Rebuilt {
    /// How many members compiled.
    pub crates: usize,
    /// Those with no recompiled workspace dependency: where the edit was.
    pub roots: Vec<String>,
    /// Summed seconds of the members' units.
    #[serde(rename = "busy_s")]
    pub busy: f64,
    /// Why this looks like a first build in its target directory rather
    /// than an edit: then the roots are where the graph starts, not edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_build: Option<FirstBuild>,
}

/// What says a build was a first build in its target directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum FirstBuild {
    /// The target directory had no fingerprints for any workspace crate
    /// before the build, and had them after (the caller knows this, see
    /// [`Extra::first_build`]).
    NoFingerprints,
    /// No workspace crate in the report was fresh, and the recompiled
    /// crates start from two or more crates with no workspace dependency.
    FromLeaves,
}

/// What else the analysis can use besides the report.
#[derive(Debug, Clone, Copy, Default)]
pub struct Extra<'a> {
    /// An earlier report of the same workspace (a `cargo check` of the
    /// same edit, or an earlier build) to compare unit times with.
    pub baseline: Option<&'a Timings>,
    /// The workspace's crates and their dependencies.
    pub workspace: Option<&'a Workspace>,
    /// Whether the build was the first in its target directory: `Some(true)`
    /// when it had no fingerprints for the workspace before, `Some(false)`
    /// after a known edit, `None` to tell from the report.
    pub first_build: Option<bool>,
}

/// Analyze a report on its own.
#[must_use]
pub fn analyze(t: &Timings) -> Analysis {
    analyze_with(t, Extra::default())
}

/// Analyze a report, comparing with a baseline and the workspace graph
/// when given.
#[must_use]
pub fn analyze_with(t: &Timings, extra: Extra<'_>) -> Analysis {
    let ran: Vec<&Unit> = t.ran().collect();
    let mut a = Analysis {
        total_units: t.units.len(),
        compiled: ran.len(),
        units_fresh: t.units.len() - ran.len(),
        jobs: t.jobs,
        ..Analysis::default()
    };
    if ran.is_empty() {
        return a;
    }
    let first = ran
        .iter()
        .map(|u| u.start)
        .fold(f64::INFINITY, f64::min)
        .min(0.0);
    let end = ran.iter().map(|u| u.end()).fold(0.0, f64::max);
    a.wall = end - first;
    let busy: f64 = ran.iter().map(|u| u.duration).sum();
    a.parallelism = busy / a.wall.max(1e-9);
    if !t.cpu.is_empty() {
        let n = t.cpu.len() as f64;
        a.cpu_avg = Some(t.cpu.iter().map(|c| c.1).sum::<f64>() / n);
    }
    a.queued = queued(t, first);
    let path = critical_path(t);
    a.path_gaps = path.iter().map(|(_, s)| s.gap).sum();
    let on_path: Vec<(usize, f64)> = path.iter().map(|(p, s)| (*p, s.duration)).collect();
    a.kinds = kinds(&ran, &on_path, &t.units);
    a.critical_path = path.into_iter().map(|(_, s)| s).collect();
    a.serial = serial(&ran);
    per_unit(&mut a, &ran);
    a.linked = linked(&ran, end);
    a.slowdown = extra.baseline.and_then(|b| slowdown(&ran, b));
    a.rebuilt = extra
        .workspace
        .and_then(|w| rebuilt(&ran, &t.units, w, extra.first_build));
    a
}

/// Slowest units, long codegen, long post-metadata analysis, build scripts.
fn per_unit(a: &mut Analysis, ran: &[&Unit]) {
    let mut slowest: Vec<&&Unit> = ran.iter().collect();
    slowest.sort_by(|x, y| y.duration.total_cmp(&x.duration));
    a.slowest = slowest
        .iter()
        .take(10)
        .map(|u| UnitTime::new(u, None))
        .collect();
    // A check unit has no LLVM codegen: cargo splits every unit at its
    // metadata, and for a check the part after it is borrowck, MIR checks
    // and saving the incremental state.
    for u in ran {
        let Some(after) = u.codegen() else { continue };
        if after < 5.0 || after < 0.3 * u.duration {
            continue;
        }
        let row = UnitTime::new(u, Some(after));
        match u.kind() {
            Kind::Check => a.post_metadata.push(row),
            Kind::Lib | Kind::Link => a.long_codegen.push(row),
            Kind::BuildScript => {}
        }
    }
    a.long_codegen
        .sort_by(|x, y| y.part.unwrap_or(0.0).total_cmp(&x.part.unwrap_or(0.0)));
    a.post_metadata
        .sort_by(|x, y| y.part.unwrap_or(0.0).total_cmp(&x.part.unwrap_or(0.0)));
    a.build_scripts = ran
        .iter()
        .filter(|u| u.is_build_script() && u.duration >= 3.0)
        .map(|u| UnitTime::new(u, None))
        .collect();
    a.build_scripts
        .sort_by(|x, y| y.duration.total_cmp(&x.duration));
}

/// Long units that ran mostly alone.
fn serial(ran: &[&Unit]) -> Vec<Serial> {
    let mut out = Vec::new();
    for u in ran {
        if u.duration < 5.0 {
            continue;
        }
        let overlap: f64 = ran
            .iter()
            .filter(|o| !core::ptr::eq(**o, *u))
            .map(|o| (o.end().min(u.end()) - o.start.max(u.start)).max(0.0))
            .sum();
        let others = overlap / u.duration;
        if others < 2.0 {
            out.push(Serial {
                unit: u.label(),
                of: u.of(),
                duration: u.duration,
                others,
            });
        }
    }
    out.sort_by(|x, y| y.duration.total_cmp(&x.duration));
    out
}

/// Unit-seconds compiled units spent ready before they started. A unit is
/// ready when the unit that lists it in `unblocked_units` ends (or, in
/// `unblocked_rmeta_units`, writes its metadata); one no unit lists was
/// ready when the build started at `first`.
fn queued(t: &Timings, first: f64) -> f64 {
    let by_i: BTreeMap<usize, usize> = t
        .units
        .iter()
        .enumerate()
        .map(|(pos, u)| (u.i.unwrap_or(pos), pos))
        .collect();
    let mut ready: BTreeMap<usize, f64> = BTreeMap::new();
    for u in &t.units {
        for (list, at) in [
            (&u.unblocked_units, u.end()),
            (&u.unblocked_rmeta_units, u.rmeta_end()),
        ] {
            for i in list {
                if let Some(&pos) = by_i.get(i) {
                    let r = ready.entry(pos).or_insert(at);
                    *r = r.max(at);
                }
            }
        }
    }
    t.units
        .iter()
        .enumerate()
        .filter(|(_, u)| u.duration > 0.0)
        .map(|(pos, u)| (u.start - ready.get(&pos).copied().unwrap_or(first)).max(0.0))
        .sum()
}

/// The chain of units that decided the wall time, first to last, with
/// each unit's position in `t.units`. Walks back from the unit that ended
/// last through whichever unblocker finished last before it started.
#[must_use]
pub fn critical_path(t: &Timings) -> Vec<(usize, PathStep)> {
    // Index units by `i` (or array position for old reports).
    let by_i: BTreeMap<usize, usize> = t
        .units
        .iter()
        .enumerate()
        .map(|(pos, u)| (u.i.unwrap_or(pos), pos))
        .collect();
    // Who unblocked whom: unit pos -> [(unblocker pos, via rmeta)].
    let mut unblockers: BTreeMap<usize, Vec<(usize, bool)>> = BTreeMap::new();
    for (pos, u) in t.units.iter().enumerate() {
        for (list, rmeta) in [
            (&u.unblocked_units, false),
            (&u.unblocked_rmeta_units, true),
        ] {
            for i in list {
                if let Some(&dep) = by_i.get(i) {
                    unblockers.entry(dep).or_default().push((pos, rmeta));
                }
            }
        }
    }
    let Some((mut cur, _)) = t
        .units
        .iter()
        .enumerate()
        .filter(|(_, u)| u.duration > 0.0)
        .max_by(|x, y| x.1.end().total_cmp(&y.1.end()))
    else {
        return Vec::new();
    };
    let mut path = Vec::new();
    for _ in 0..t.units.len() {
        let u = &t.units[cur];
        let prev = unblockers.get(&cur).and_then(|ps| {
            ps.iter()
                .filter(|(p, _)| t.units[*p].duration > 0.0)
                .map(|&(p, r)| {
                    let p_unit = &t.units[p];
                    (p, r, if r { p_unit.rmeta_end() } else { p_unit.end() })
                })
                .filter(|&(_, _, done)| done <= u.start + 0.05)
                .max_by(|x, y| x.2.total_cmp(&y.2))
        });
        // Without a compiled unblocker there's nothing to measure a wait
        // from (it waited for fresh units, or for nothing).
        let ready = prev.map_or(u.start, |(_, _, done)| done);
        path.push((
            cur,
            PathStep {
                unit: u.label(),
                of: u.of(),
                start: u.start,
                duration: u.duration,
                waited: if prev.is_some_and(|(_, r, _)| r) {
                    Waited::Meta
                } else {
                    Waited::Full
                },
                gap: (u.start - ready).max(0.0),
            },
        ));
        match prev {
            Some((p, _, _)) => cur = p,
            None => break,
        }
    }
    path.reverse();
    path
}

/// The workspace edges on the critical path: `(dependent, dependency,
/// seconds the dependency took on the path)`, for each step that waited
/// for a workspace crate it depends on directly.
#[must_use]
pub fn path_edges(a: &Analysis, ws: &Workspace) -> Vec<(String, String, f64)> {
    let mut seen = BTreeSet::new();
    a.critical_path
        .windows(2)
        .filter_map(|w| {
            let (dep, user) = (&w[0].of.krate, &w[1].of.krate);
            (dep != user
                && ws.deps.get(user).is_some_and(|d| d.contains(dep))
                && seen.insert((user.clone(), dep.clone())))
            .then(|| (user.clone(), dep.clone(), w[0].duration))
        })
        .collect()
}

/// Seconds by kind of unit, biggest first.
fn kinds(ran: &[&Unit], path: &[(usize, f64)], units: &[Unit]) -> Vec<KindTime> {
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

fn linked(ran: &[&Unit], end: f64) -> Linked {
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
            .map(|u| UnitSecs {
                unit: u.label(),
                took: u.duration,
                before: None,
            })
            .collect(),
    }
}

/// Compare compiled lib and check units with the same crate's unit in
/// `before`. A library built here against a check there compares the part
/// up to its metadata, which is all a check does (when the report splits
/// it; a cache wrapper can hide the split, and then codegen is included).
fn slowdown(ran: &[&Unit], before: &Timings) -> Option<Slowdown> {
    let base_ran: Vec<&Unit> = before.ran().collect();
    let base_check = before.is_check();
    let mut prev: BTreeMap<(&str, Kind), f64> = BTreeMap::new();
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
        top: rows
            .into_iter()
            .take(5)
            .map(|(unit, took, before)| UnitSecs {
                unit,
                took,
                before: Some(before),
            })
            .collect(),
    })
}

fn rebuilt(
    ran: &[&Unit],
    units: &[Unit],
    ws: &Workspace,
    first_build: Option<bool>,
) -> Option<Rebuilt> {
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
        .collect::<Vec<String>>();
    let fresh_members = units.iter().any(|u| {
        u.duration <= 0.0 && u.kind() != Kind::BuildScript && ws.deps.contains_key(&u.name)
    });
    let from_leaves = !fresh_members
        && roots.len() >= 2
        && roots
            .iter()
            .all(|r| ws.deps.get(r).is_none_or(BTreeSet::is_empty));
    let first_build = match first_build {
        Some(true) => Some(FirstBuild::NoFingerprints),
        None if from_leaves => Some(FirstBuild::FromLeaves),
        _ => None,
    };
    Some(Rebuilt {
        crates: compiled.len(),
        roots,
        first_build,
        busy: ran
            .iter()
            .filter(|u| compiled.contains(u.name.as_str()))
            .map(|u| u.duration)
            .sum(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Span;
    use alloc::format;
    use alloc::vec;

    fn unit(
        i: usize,
        name: &str,
        start: f64,
        duration: f64,
        unblocks: &[usize],
        rmeta: &[usize],
    ) -> Unit {
        Unit {
            i: Some(i),
            name: name.into(),
            target: " (check)".into(),
            mode: "todo".into(),
            start,
            duration,
            unblocked_units: unblocks.to_vec(),
            unblocked_rmeta_units: rmeta.to_vec(),
            sections: Some(vec![
                (
                    "frontend".into(),
                    Span {
                        start: 0.0,
                        end: duration * 0.9,
                    },
                ),
                (
                    "codegen".into(),
                    Span {
                        start: duration * 0.9,
                        end: duration,
                    },
                ),
            ]),
            ..Unit::default()
        }
    }

    /// a, b in parallel; c needs a's metadata; big needs b and c; d fresh.
    fn fixture() -> Timings {
        Timings {
            units: vec![
                unit(10, "a", 0.0, 2.0, &[], &[12]),
                unit(11, "b", 0.0, 1.0, &[13], &[]),
                unit(12, "c", 1.8, 3.0, &[13], &[]),
                unit(13, "big", 4.8, 40.0, &[], &[]),
                unit(14, "d", 0.0, 0.0, &[], &[]),
            ],
            cpu: vec![(0.0, 50.0), (1.0, 10.0)],
            ..Timings::default()
        }
    }

    #[test]
    fn critical_path_and_serial() {
        let a = analyze(&fixture());
        assert!((a.wall - 44.8).abs() < 1e-9);
        let path: Vec<_> = a.critical_path.iter().map(|p| p.unit.as_str()).collect();
        assert_eq!(path, ["a (check)", "c (check)", "big (check)"]);
        assert_eq!(a.critical_path[1].waited, Waited::Meta);
        assert_eq!(a.critical_path[2].waited, Waited::Full);
        assert_eq!(a.serial[0].unit, "big (check)");
        assert_eq!((a.compiled, a.units_fresh, a.total_units), (4, 1, 5));
        assert_eq!(a.cpu_avg, Some(30.0));
        // big ran 4 s of its 40 with its own post-metadata part.
        assert_eq!(a.post_metadata.len(), 0);
        let ws = Workspace {
            deps: [
                ("c".to_owned(), ["a".to_owned()].into()),
                ("big".to_owned(), ["c".to_owned()].into()),
            ]
            .into(),
        };
        let edges = path_edges(&a, &ws);
        assert_eq!(
            edges,
            [
                ("c".to_owned(), "a".to_owned(), 2.0),
                ("big".to_owned(), "c".to_owned(), 3.0)
            ]
        );
    }

    #[test]
    fn queued_units() {
        // Nothing started later than it could.
        assert!(analyze(&fixture()).queued.abs() < 1e-9);
        // Three independent units one after another: the second sat ready
        // for 2 s, the third for 4 s. A unit that a's end unblocked was
        // ready at 2 s and started at 6 s.
        let t = Timings {
            units: vec![
                unit(0, "a", 0.0, 2.0, &[3], &[]),
                unit(1, "b", 2.0, 2.0, &[], &[]),
                unit(2, "c", 4.0, 2.0, &[], &[]),
                unit(3, "d", 6.0, 1.0, &[], &[]),
            ],
            ..Timings::default()
        };
        let a = analyze(&t);
        assert!((a.queued - 10.0).abs() < 1e-9, "{}", a.queued);
        assert!((a.parallelism - 1.0).abs() < 1e-9);
    }

    #[test]
    fn json_field_names() {
        let a = analyze(&fixture());
        let v = serde_json::to_value(&a).unwrap();
        assert!((v["wall_s"].as_f64().unwrap() - 44.8).abs() < 1e-9);
        assert_eq!(v["units_compiled"], 4);
        assert_eq!(v["critical_path"][1]["waited"], "meta");
        assert_eq!(v["critical_path"][1]["crate"], "c");
        assert_eq!(v["kinds"][0]["kind"], "check");
        let back: Analysis = serde_json::from_value(v).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn rebuilt_roots() {
        let t = fixture();
        let ws = Workspace {
            deps: [
                ("a".to_owned(), BTreeSet::new()),
                ("c".to_owned(), ["a".to_owned()].into()),
                ("big".to_owned(), ["c".to_owned()].into()),
            ]
            .into(),
        };
        let a = analyze_with(
            &t,
            Extra {
                baseline: None,
                workspace: Some(&ws),
                first_build: None,
            },
        );
        let r = a.rebuilt.unwrap();
        assert_eq!(r.crates, 3);
        assert_eq!(r.roots, ["a"]);
        assert!((r.busy - 45.0).abs() < 1e-9);
    }

    fn lib(i: usize, name: &str, target: &str, start: f64, duration: f64, next: &[usize]) -> Unit {
        Unit {
            i: Some(i),
            name: name.into(),
            target: target.into(),
            mode: "todo".into(),
            start,
            duration,
            unblocked_units: next.to_vec(),
            ..Unit::default()
        }
    }

    /// A chain of libraries 10x slower than their check, then six binaries
    /// of about two minutes each.
    #[test]
    fn links_and_slowdown() {
        let mut units = Vec::new();
        let mut checks = Vec::new();
        for n in 0..6_u8 {
            let name = format!("lib{n}");
            let i = usize::from(n);
            units.push(lib(i, &name, "", 60.0 * f64::from(n), 60.0, &[i + 1]));
            checks.push(lib(i, &name, " (check)", 6.0 * f64::from(n), 6.0, &[]));
        }
        for b in 0..6_u8 {
            let name = format!("tool-{b}");
            units.push(lib(
                10 + usize::from(b),
                &name,
                &format!(" {name} \"bin\""),
                360.0,
                119.0 + 15.0 * f64::from(b),
                &[],
            ));
        }
        units[5].unblocked_units = (10..16).collect();
        let t = Timings {
            units,
            ..Timings::default()
        };
        let check = Timings {
            units: checks,
            ..Timings::default()
        };
        assert!(check.is_check() && !t.is_check());
        let a = analyze_with(
            &t,
            Extra {
                baseline: Some(&check),
                workspace: None,
                first_build: None,
            },
        );
        assert!((a.wall - 554.0).abs() < 1e-9);
        assert_eq!(a.linked.units, 6);
        assert!((a.linked.tail - 194.0).abs() < 1e-9);
        assert_eq!(a.linked.slowest[0].unit, "tool-5 tool-5 \"bin\"");
        assert!((a.linked.slowest[0].took - 194.0).abs() < 1e-9);
        let s = a.slowdown.unwrap();
        assert!(s.baseline_check);
        assert_eq!(s.compared, 6);
        assert!((s.median - 10.0).abs() < 1e-9);
        assert!((s.excess - 324.0).abs() < 1e-9);
        assert_eq!(s.top[0].before, Some(6.0));
        let kinds: Vec<Kind> = a.kinds.iter().map(|k| k.kind).collect();
        assert_eq!(kinds, [Kind::Link, Kind::Lib]);
        let path: Vec<&str> = a
            .critical_path
            .iter()
            .map(|p| p.of.krate.as_str())
            .collect();
        assert_eq!(
            path,
            ["lib0", "lib1", "lib2", "lib3", "lib4", "lib5", "tool-5"]
        );
    }

    #[test]
    fn empty_and_fresh() {
        let a = analyze(&Timings::default());
        assert_eq!(a, Analysis::default());
        let a = analyze(&Timings {
            units: vec![unit(1, "a", 0.0, 0.0, &[], &[])],
            ..Timings::default()
        });
        assert_eq!((a.compiled, a.units_fresh), (0, 1));
        assert_eq!(a.critical_path.len(), 0);
    }
}
