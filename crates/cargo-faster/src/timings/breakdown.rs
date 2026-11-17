//! What the cache wrapper did across a build, and the workspace graph the
//! analysis compares rebuilt crates with.

use super::wrapper::{Outcome, WrapperRun};
use super::{Kind, Timings, Workspace};
use alloc::collections::{BTreeMap, BTreeSet};
use serde::Serialize;

/// Units by what the rustc wrapper did with them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct WrapperSummary {
    /// Compiled units the wrapper has a record of.
    pub matched: usize,
    /// Compiled units it has none for (build script runs aside).
    pub unmatched: usize,
    /// Restored from the cache: count and seconds.
    #[serde(serialize_with = "count_json")]
    pub hits: (usize, f64),
    /// Cache misses rustc compiled: count and seconds.
    #[serde(serialize_with = "count_json")]
    pub misses: (usize, f64),
    /// Incremental passthroughs: count and seconds.
    #[serde(serialize_with = "count_json")]
    pub incremental: (usize, f64),
    /// Other passthroughs: count and seconds.
    #[serde(serialize_with = "count_json")]
    pub other: (usize, f64),
    /// Seconds spent waiting for the wrapper's scheduler.
    #[serde(rename = "wait_s")]
    pub wait: f64,
}

/// Sum up the wrapper's records of the compiled units (`runs` is indexed
/// like `t.units`). `None` when it has none.
pub(super) fn wrapper_summary(t: &Timings, runs: &[Option<WrapperRun>]) -> Option<WrapperSummary> {
    let mut s = WrapperSummary::default();
    for (pos, u) in t.units.iter().enumerate() {
        if u.duration <= 0.0 || u.kind() == Kind::BuildScript {
            continue;
        }
        let Some(w) = runs.get(pos).and_then(Option::as_ref) else {
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

/// The workspace's crates and their direct workspace dependencies (normal
/// deps), from `cargo metadata`.
#[must_use]
pub fn workspace_from_metadata(m: &crate::context::Metadata) -> Workspace {
    let (rdeps, _) = crate::graph::reverse_deps(m);
    let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (dep, users) in rdeps {
        deps.entry(dep.clone()).or_default();
        for u in users {
            deps.entry(u).or_default().insert(dep.clone());
        }
    }
    Workspace { deps }
}

fn count_json<S: serde::Serializer>(v: &(usize, f64), s: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Count {
        units: usize,
        took_s: f64,
    }
    Count {
        units: v.0,
        took_s: v.1,
    }
    .serialize(s)
}
