//! Joining a timings report with what the rustc wrapper (kache) did with
//! each unit: a cache hit, a real compile, an incremental passthrough, and
//! how long it waited for the wrapper's scheduler.

use super::Timings;
use crate::context::{Context, probe};
use crate::finding::{Finding, Severity};
use serde::{Deserialize, Serialize};

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

/// The parts of a `kache stats --format json` report we read. Everything
/// is optional: kache's JSON is not a stable interface.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct KacheReport {
    /// Per-crate trace events (unlike `all_events`, passthroughs too).
    #[serde(default, rename = "traceEvents", deserialize_with = "null_as_empty")]
    pub trace_events: Vec<TraceEvent>,
    /// The session(s) the report covers.
    #[serde(default)]
    pub meta: Option<KacheMeta>,
}

/// A list that may be `null`: empty.
fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// One trace event.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TraceEvent {
    /// When the wrapper started, Unix microseconds.
    pub ts: Option<f64>,
    /// What it did.
    pub args: Option<TraceArgs>,
}

/// A trace event's `args`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TraceArgs {
    /// The rustc crate name.
    pub crate_name: Option<String>,
    /// `local_hit`, `miss`, `passthrough`, ...
    pub result: Option<String>,
    /// Why a passthrough was one (`adaptive active`).
    pub reason: Option<String>,
    /// Milliseconds from the wrapper starting to it exiting.
    pub elapsed_ms: Option<f64>,
    /// Milliseconds rustc ran.
    pub compile_time_ms: Option<f64>,
    /// Milliseconds waiting for a scheduler permit.
    pub permit_wait_ms: Option<f64>,
    /// Milliseconds waiting for another build's compile of the same key.
    pub flight_wait_ms: Option<f64>,
}

/// A report's `meta`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct KacheMeta {
    /// The latest session.
    pub session: Option<KacheSession>,
    /// Several sessions, when the report spans them.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub sessions: Vec<KacheSession>,
}

/// A kache session. `argv`, `cwd` and `commands` are provisional (see
/// [`CargoCommand`]).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct KacheSession {
    /// When it started (RFC 3339).
    pub started_at: Option<String>,
    /// The tree it ran in.
    pub root: Option<String>,
    /// The command line that started it.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub argv: Vec<String>,
    /// The directory that command ran in.
    pub cwd: Option<String>,
    /// The cargo commands it saw.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub commands: Vec<KacheCommand>,
}

/// One cargo command of a session, as kache would record it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct KacheCommand {
    /// When cargo started, Unix milliseconds.
    pub started_unix_ms: Option<f64>,
    /// When cargo started (RFC 3339), when `started_unix_ms` is absent.
    pub started_at: Option<String>,
    /// Its command line, `cargo` first.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub argv: Vec<String>,
    /// The directory it ran in.
    pub cwd: Option<String>,
}

impl KacheReport {
    /// The report in a JSON value; the default (nothing recorded) when it
    /// isn't shaped like one.
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Self {
        Self::deserialize(value).unwrap_or_default()
    }

    /// The latest session (`meta.session`).
    #[must_use]
    pub fn session(&self) -> Option<&KacheSession> {
        self.meta.as_ref()?.session.as_ref()
    }
}

impl KacheSession {
    /// The command line that started it: `None` when kache didn't record
    /// it (provisional, see [`CargoCommand`]).
    #[must_use]
    pub fn argv(&self) -> Option<&[String]> {
        (!self.argv.is_empty()).then_some(self.argv.as_slice())
    }
}

/// The compile events in a `kache stats --format json` report (its
/// `traceEvents`, which unlike `all_events` include passthroughs).
#[must_use]
pub fn kache_events(report: &KacheReport) -> Vec<WrapperEvent> {
    report
        .trace_events
        .iter()
        .filter_map(|event| {
            let args = event.args.as_ref()?;
            let crate_name = args.crate_name.as_deref()?;
            let reason = args.reason.as_deref().unwrap_or("");
            let outcome = outcome(args.result.as_deref()?, reason)?;
            if crate_name.starts_with("build_script_") || crate_name == "unknown" {
                return None;
            }
            let secs = |ms: Option<f64>| ms.unwrap_or(0.0) / 1000.0;
            Some(WrapperEvent {
                crate_name: crate_name.to_owned(),
                start_ms: event.ts? / 1000.0,
                run: WrapperRun {
                    outcome,
                    compile: if outcome == Outcome::Hit {
                        0.0
                    } else {
                        secs(args.compile_time_ms)
                    },
                    wait: secs(args.permit_wait_ms) + secs(args.flight_wait_ms),
                    elapsed: secs(args.elapsed_ms),
                },
            })
        })
        .collect()
}

/// A cargo command kache saw in a session.
///
/// Provisional: kache doesn't record these yet. The fields follow the
/// proposal in <https://github.com/kunobi-ninja/kache/issues/1968>
/// (`meta.session.commands[]`: `started_at`, `started_unix_ms`, `argv`,
/// `cwd`), all optional, and may change when kache ships them.
#[derive(Debug, Clone, PartialEq)]
pub struct CargoCommand {
    /// When cargo started, Unix milliseconds (`started_unix_ms`, else
    /// `started_at`).
    pub started_unix_ms: f64,
    /// Its command line (`argv`), `cargo` first.
    pub argv: Vec<String>,
    /// The directory it ran in (`cwd`).
    pub cwd: Option<String>,
}

impl CargoCommand {
    /// The command line joined with spaces: `cargo test --workspace`.
    #[must_use]
    pub fn command_line(&self) -> String {
        self.argv.join(" ")
    }
}

fn command(recorded: &KacheCommand) -> Option<CargoCommand> {
    let started_unix_ms = recorded.started_unix_ms.or_else(|| {
        recorded
            .started_at
            .as_deref()
            .and_then(cargo_timings_parse::parse_timestamp)
    })?;
    if recorded.argv.is_empty() {
        return None;
    }
    Some(CargoCommand {
        started_unix_ms,
        argv: recorded.argv.clone(),
        cwd: recorded.cwd.clone(),
    })
}

/// The cargo commands of the session(s) in a `kache stats --format json`
/// report, oldest first: `meta.session.commands`, and the `commands` of
/// each of `meta.sessions` (both provisional, see [`CargoCommand`]).
/// Empty when kache doesn't record them.
#[must_use]
pub fn session_commands(report: &KacheReport) -> Vec<CargoCommand> {
    let Some(meta) = &report.meta else {
        return Vec::new();
    };
    let mut out: Vec<CargoCommand> = meta
        .session
        .iter()
        .chain(&meta.sessions)
        .flat_map(|session| &session.commands)
        .filter_map(command)
        .collect();
    out.sort_by(|left, right| left.started_unix_ms.total_cmp(&right.started_unix_ms));
    out.dedup();
    out
}

/// What kache recorded for a time window: its per-crate events and, when
/// it records them, the cargo commands of the session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Records {
    /// The compile events (see [`kache_events`]).
    pub events: Vec<WrapperEvent>,
    /// The session's cargo commands, oldest first (see
    /// [`session_commands`]); empty when kache doesn't record them.
    pub commands: Vec<CargoCommand>,
}

impl Records {
    /// Read a `kache stats --format json` report.
    #[must_use]
    pub fn from_report(report: &KacheReport) -> Self {
        Self {
            events: kache_events(report),
            commands: session_commands(report),
        }
    }
}

/// Ask kache for its records of the builds in this workspace since a
/// report started (`kache stats --since <minutes> --root <root> --format
/// json`). Empty when the wrapper isn't kache, the report is over a week
/// old, or kache fails.
#[must_use]
pub fn gather(ctx: &Context, start_unix_ms: f64) -> Records {
    let uses_kache = crate::cache::wrapper(ctx).is_some_and(|(w, _)| w.contains("kache"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    let minutes = ((now - start_unix_ms) / 60_000.0).ceil() + 2.0;
    if !uses_kache || !(0.0..=7.0 * 24.0 * 60.0).contains(&minutes) {
        return Records::default();
    }
    let root = ctx.root();
    let out = probe("kache")
        .args(["stats", "--since", &format!("{minutes:.0}m"), "--root"])
        .arg(root)
        .args(["--format", "json"])
        .current_dir(root)
        .output();
    out.ok()
        .and_then(|output| serde_json::from_slice::<KacheReport>(&output.stdout).ok())
        .map(|report| Records::from_report(&report))
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
    join_events(t, events)
        .into_iter()
        .map(|j| j.map(|j| events[j].run.clone()))
        .collect()
}

/// Like [`join`], but the index into `events` of each unit's record.
#[must_use]
pub fn join_events(t: &Timings, events: &[WrapperEvent]) -> Vec<Option<usize>> {
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
            runs[i] = Some(j);
        }
    }
    runs
}

/// Attribute records to the cargo command that was running when each
/// started: the latest command that started at or before it. Returns
/// `(index into commands, records)` for each command with records, oldest
/// first; records from before the first command are left out. Empty when
/// there are no commands.
#[must_use]
pub fn by_command(
    events: &[WrapperEvent],
    joined: impl IntoIterator<Item = usize>,
    commands: &[CargoCommand],
) -> Vec<(usize, usize)> {
    let mut counts = vec![0_usize; commands.len()];
    for j in joined {
        let Some(e) = events.get(j) else { continue };
        if let Some(c) = commands
            .iter()
            .rposition(|c| c.started_unix_ms <= e.start_ms)
        {
            counts[c] += 1;
        }
    }
    counts
        .into_iter()
        .enumerate()
        .filter(|(_, n)| *n > 0)
        .collect()
}

/// A warning when the kache records joined with a report came from more
/// than one cargo command (`counts` from [`by_command`]).
#[must_use]
pub fn mixed_commands(commands: &[CargoCommand], counts: &[(usize, usize)]) -> Option<Finding> {
    if counts.len() < 2 {
        return None;
    }
    let total: usize = counts.iter().map(|(_, n)| n).sum();
    let list = counts
        .iter()
        .filter_map(|&(c, n)| {
            let c = commands.get(c)?;
            Some(format!(
                "`{}`: {n} record{}",
                c.command_line(),
                if n == 1 { "" } else { "s" }
            ))
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(
        Finding::new("kache-records-mixed-commands", Severity::Medium, format!("The cache records joined with this report come from {} cargo commands", counts.len()), "kache-sessions-and-cargo-commands")
            .saw(format!("kache's session recorded several cargo commands while this report's units ran; the {total} records matched to its units started under {list}."))
            .why("The cache outcomes and waits shown for this report are matched by crate name and start time. Records from another cargo command (a clippy or test run in the same kache session) can describe a different build than the one the report timed, so the hits, misses and waits may not be this build's.")
            .fix("Run `cargo faster timings --run`, which times one cargo command and reads kache's records for it alone, or run one cargo command per kache session when you time a build yourself. Tradeoff: none beyond running the build again."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trace_events() {
        let v: KacheReport = serde_json::from_value(serde_json::json!({"traceEvents": [
            {"ts": 2_000_000.0, "args": {"crate_name": "a_b", "result": "local_hit", "elapsed_ms": 90, "compile_time_ms": 4000, "permit_wait_ms": 0}},
            {"ts": 3_000_000.0, "args": {"crate_name": "c", "result": "passthrough", "reason": "adaptive active", "elapsed_ms": 9000, "compile_time_ms": 3000, "permit_wait_ms": 6000}},
            {"ts": 3_000_000.0, "args": {"crate_name": "unknown", "result": "passthrough", "reason": "not-a-compile|probe"}},
            {"ts": 4_000_000.0, "args": {"crate_name": "d", "result": "dup", "elapsed_ms": 10, "compile_time_ms": 8}}
        ]}))
        .unwrap();
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
