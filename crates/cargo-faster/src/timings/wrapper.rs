//! Joining a timings report with what the rustc wrapper (kache) did with
//! each unit: a cache hit, a real compile, an incremental passthrough, and
//! how long it waited for the wrapper's scheduler.

use super::Timings;
use crate::context::{Context, probe};
use serde::Serialize;
use serde_json::Value;

/// What the wrapper did with one rustc invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Restored from the cache: rustc didn't run.
    Hit,
    /// Compiled (a miss, or a duplicate of a compile running elsewhere).
    Miss,
    /// Passed to rustc uncached because it's incremental (kache's
    /// "adaptive" passthroughs, its incremental force-list, worktree seeds).
    Incr,
    /// Passed to rustc uncached for another reason.
    Pass,
}

impl Outcome {
    /// The short label used in tables.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Incr => "incr",
            Self::Pass => "pass",
        }
    }
}

/// The wrapper's record of one unit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WrapperRun {
    /// What happened.
    pub outcome: Outcome,
    /// Seconds rustc ran (0 for a hit).
    pub compile: f64,
    /// Seconds spent waiting for a scheduler permit or another build's
    /// compile of the same key.
    pub wait: f64,
    /// Seconds from the wrapper starting to it exiting.
    pub elapsed: f64,
}

/// One per-crate event from `kache stats --format json`.
#[derive(Debug, Clone, PartialEq)]
pub struct WrapperEvent {
    /// The rustc crate name (`app_core`).
    pub crate_name: String,
    /// When the wrapper started, Unix milliseconds.
    pub start_ms: f64,
    /// What it did.
    pub run: WrapperRun,
}

fn outcome(result: &str, reason: &str) -> Option<Outcome> {
    Some(match result {
        "local_hit" | "remote_hit" | "prefetch_hit" | "hit" => Outcome::Hit,
        "miss" | "dup" => Outcome::Miss,
        "passthrough" if reason.starts_with("not-a-compile") => return None,
        "passthrough"
            if ["adaptive", "incremental", "seed"]
                .iter()
                .any(|k| reason.contains(k)) =>
        {
            Outcome::Incr
        }
        "passthrough" | "skipped" => Outcome::Pass,
        _ => return None,
    })
}

/// The compile events in a `kache stats --format json` report (its
/// `traceEvents`, which unlike `all_events` include passthroughs).
#[must_use]
pub fn kache_events(report: &Value) -> Vec<WrapperEvent> {
    let ms = |a: &Value, k: &str| a.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    report
        .get("traceEvents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let a = e.get("args")?;
            let crate_name = a.get("crate_name")?.as_str()?;
            let reason = a.get("reason").and_then(Value::as_str).unwrap_or("");
            let outcome = outcome(a.get("result")?.as_str()?, reason)?;
            if crate_name.starts_with("build_script_") || crate_name == "unknown" {
                return None;
            }
            Some(WrapperEvent {
                crate_name: crate_name.to_owned(),
                start_ms: e.get("ts")?.as_f64()? / 1000.0,
                run: WrapperRun {
                    outcome,
                    compile: if outcome == Outcome::Hit {
                        0.0
                    } else {
                        ms(a, "compile_time_ms") / 1000.0
                    },
                    wait: (ms(a, "permit_wait_ms") + ms(a, "flight_wait_ms")) / 1000.0,
                    elapsed: ms(a, "elapsed_ms") / 1000.0,
                },
            })
        })
        .collect()
}

/// Ask kache for its records of the builds in this workspace since a
/// report started (`kache stats --since <minutes> --root <root> --format
/// json`). Empty when the wrapper isn't kache, the report is over a week
/// old, or kache fails.
#[must_use]
pub fn gather(ctx: &Context, start_unix_ms: f64) -> Vec<WrapperEvent> {
    let uses_kache = crate::cache::wrapper(ctx).is_some_and(|(w, _)| w.contains("kache"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    let minutes = ((now - start_unix_ms) / 60_000.0).ceil() + 2.0;
    if !uses_kache || !(0.0..=7.0 * 24.0 * 60.0).contains(&minutes) {
        return Vec::new();
    }
    let root = ctx.root();
    let out = probe("kache")
        .args(["stats", "--since", &format!("{minutes:.0}m"), "--root"])
        .arg(root)
        .args(["--format", "json"])
        .current_dir(root)
        .output();
    out.ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .map(|v| kache_events(&v))
        .unwrap_or_default()
}

/// How far apart (ms) cargo starting a unit and the wrapper starting can
/// be and still be the same invocation.
const MATCH_MS: f64 = 5_000.0;

/// Match wrapper events to the report's units (by crate name and start
/// time, nearest first): the wrapper's record of each unit, indexed like
/// `t.units`. Needs the report's start time.
#[must_use]
pub fn join(t: &Timings, events: &[WrapperEvent]) -> Vec<Option<WrapperRun>> {
    let mut runs = vec![None; t.units.len()];
    let Some(t0) = t.start_unix_ms else {
        return runs;
    };
    let mut order: Vec<usize> = (0..t.units.len())
        .filter(|&i| t.units[i].duration > 0.0 && !t.units[i].is_build_script())
        .collect();
    order.sort_by(|&a, &b| t.units[a].start.total_cmp(&t.units[b].start));
    let mut used = vec![false; events.len()];
    for i in order {
        let u = &t.units[i];
        let name = u.crate_name();
        let at = t0 + u.start * 1000.0;
        let best = events
            .iter()
            .enumerate()
            .filter(|(j, e)| !used[*j] && e.crate_name == name)
            .map(|(j, e)| (j, (e.start_ms - at).abs()))
            .filter(|(_, d)| *d <= MATCH_MS)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((j, _)) = best {
            used[j] = true;
            runs[i] = Some(events[j].run.clone());
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trace_events() {
        let v: Value = serde_json::json!({"traceEvents": [
            {"ts": 2_000_000.0, "args": {"crate_name": "a_b", "result": "local_hit", "elapsed_ms": 90, "compile_time_ms": 4000, "permit_wait_ms": 0}},
            {"ts": 3_000_000.0, "args": {"crate_name": "c", "result": "passthrough", "reason": "adaptive active", "elapsed_ms": 9000, "compile_time_ms": 3000, "permit_wait_ms": 6000}},
            {"ts": 3_000_000.0, "args": {"crate_name": "unknown", "result": "passthrough", "reason": "not-a-compile|probe"}},
            {"ts": 4_000_000.0, "args": {"crate_name": "d", "result": "dup", "elapsed_ms": 10, "compile_time_ms": 8}}
        ]});
        let e = kache_events(&v);
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].run.outcome, Outcome::Hit);
        assert!(e[0].run.compile.abs() < 1e-9);
        assert_eq!(e[1].run.outcome, Outcome::Incr);
        assert!((e[1].run.wait - 6.0).abs() < 1e-9);
        assert!((e[1].start_ms - 3000.0).abs() < 1e-9);
        assert_eq!(e[2].run.outcome, Outcome::Miss);
    }
}
