//! Reading `cargo build --timings` reports: the critical path, crates that
//! serialize the build, long code generation, linking, build scripts, and
//! (joined with the cache wrapper's records and an earlier report) cache
//! hits, waits and contention.

pub mod breakdown;
mod findings;
pub mod wrapper;

use crate::finding::{Section, table};
use crate::fmt::{ellipsize, pct, secs};
pub use breakdown::Workspace;
use breakdown::{KindTime, Linked, Rebuilt, Slowdown, WrapperSummary};
pub use findings::findings;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use wrapper::{Outcome, WrapperRun};

/// One unit (a crate target in one mode) from the report.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    /// Unit index (references in `unblocked_*` use it).
    #[serde(default)]
    pub i: Option<usize>,
    /// Package name.
    pub name: String,
    /// Package version.
    #[serde(default)]
    pub version: String,
    /// `todo` for compiling, `run-custom-build` for build scripts.
    #[serde(default)]
    pub mode: String,
    /// e.g. ` (check)`, ` (check-test)`, ` build-script (run)`.
    #[serde(default)]
    pub target: String,
    /// Start, seconds since the build began.
    pub start: f64,
    /// Duration in seconds (0 when the unit was fresh).
    pub duration: f64,
    /// Units this one unblocks when it finishes.
    #[serde(default, alias = "unlocked_units")]
    pub unblocked_units: Vec<usize>,
    /// Units this one unblocks once its metadata is ready.
    #[serde(default, alias = "unlocked_rmeta_units")]
    pub unblocked_rmeta_units: Vec<usize>,
    /// Cargo 1.88+ sections: `[["frontend", {start, end}], ["codegen", ..]]`.
    #[serde(default)]
    pub sections: Option<Vec<(String, Span)>>,
    /// Older cargo: when the metadata was ready.
    #[serde(default)]
    pub rmeta_time: Option<f64>,
    /// What the rustc wrapper did with it, once joined (see
    /// [`wrapper::join`]).
    #[serde(skip)]
    pub wrapper: Option<WrapperRun>,
}

/// What a unit produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// `cargo check`: metadata only, no code generation.
    Check,
    /// A library, built (code generated, not linked).
    Lib,
    /// A binary, test, example or bench: code generated and linked.
    Link,
    /// A build script, compiled or run.
    BuildScript,
}

impl Kind {
    /// A label for tables.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Check => "check (metadata only)",
            Self::Lib => "libraries (built)",
            Self::Link => "binaries and tests (codegen + link)",
            Self::BuildScript => "build scripts",
        }
    }
}

/// A section of a unit, relative to its start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Span {
    /// Start.
    pub start: f64,
    /// End.
    pub end: f64,
}

impl Unit {
    /// When the unit finished.
    #[must_use]
    pub fn end(&self) -> f64 {
        self.start + self.duration
    }

    /// When its metadata was ready (dependents can start checking).
    #[must_use]
    pub fn rmeta_end(&self) -> f64 {
        if let Some(s) = self.section("frontend") {
            return self.start + s.end;
        }
        self.start + self.rmeta_time.unwrap_or(self.duration)
    }

    fn section(&self, name: &str) -> Option<Span> {
        self.sections
            .as_ref()?
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| *s)
    }

    /// Codegen seconds, if the report has them.
    #[must_use]
    pub fn codegen(&self) -> Option<f64> {
        if let Some(s) = self.section("codegen") {
            return Some(s.end - s.start);
        }
        self.rmeta_time.map(|r| self.duration - r)
    }

    /// `name target` for display, e.g. `tala-agent (check-test)`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}{}", self.name, self.target)
    }

    fn is_build_script(&self) -> bool {
        self.mode == "run-custom-build" || self.target.contains("build-script")
    }

    /// What the unit produces.
    #[must_use]
    pub fn kind(&self) -> Kind {
        if self.is_build_script() {
            Kind::BuildScript
        } else if self.target.contains("(check") {
            Kind::Check
        } else if ["\"bin\"", "\"test\"", "\"example\"", "\"bench\"", "(test)"]
            .iter()
            .any(|k| self.target.contains(k))
        {
            Kind::Link
        } else {
            Kind::Lib
        }
    }
}

/// A parsed timings report.
#[derive(Debug, Clone, Default)]
pub struct Timings {
    /// Every unit, fresh ones included.
    pub units: Vec<Unit>,
    /// `(seconds, percent)` CPU samples, if present.
    pub cpu: Vec<(f64, f64)>,
    /// When the build started, Unix milliseconds.
    pub start_unix_ms: Option<f64>,
    /// `jobs` and `ncpu` from the report's "Max concurrency" line.
    pub jobs: Option<(u32, u32)>,
}

impl Timings {
    /// Parse the HTML report cargo writes to `target/cargo-timings/`.
    ///
    /// # Errors
    ///
    /// When the file doesn't contain cargo's `UNIT_DATA`.
    pub fn parse_html(html: &str) -> Result<Self, String> {
        let data = js_const(html, "UNIT_DATA")
            .ok_or("no UNIT_DATA in this file: is it a cargo --timings report?")?;
        let units: Vec<Unit> =
            serde_json::from_str(data).map_err(|e| format!("parsing UNIT_DATA: {e}"))?;
        let cpu = js_const(html, "CPU_USAGE")
            .and_then(|c| serde_json::from_str::<Vec<(f64, f64)>>(c).ok())
            .unwrap_or_default();
        Ok(Self {
            units,
            cpu,
            start_unix_ms: summary_cell(html, "Build start:").and_then(unix_ms),
            jobs: summary_cell(html, "Max concurrency:").and_then(jobs),
        })
    }

    /// Whether every unit that compiled is a check unit (or build script):
    /// the report is a `cargo check`.
    #[must_use]
    pub fn is_check(&self) -> bool {
        let mut ran = self.ran().peekable();
        ran.peek().is_some() && ran.all(|u| matches!(u.kind(), Kind::Check | Kind::BuildScript))
    }

    /// The units that actually compiled.
    pub fn ran(&self) -> impl Iterator<Item = &Unit> {
        self.units.iter().filter(|u| u.duration > 0.0)
    }
}

/// The JSON array after `const NAME = `, up to the matching `];`.
fn js_const<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    let start = html.find(&format!("const {name} = "))? + name.len() + 9;
    let rest = &html[start..];
    let end = rest
        .find("\n];")
        .map(|e| e + 2)
        .or_else(|| rest.find("];").map(|e| e + 1))?;
    Some(&rest[..end])
}

/// The value cell after `<td>LABEL</td>` in the report's summary table.
fn summary_cell<'a>(html: &'a str, label: &str) -> Option<&'a str> {
    let rest = &html[html.find(&format!("<td>{label}</td>"))? + label.len() + 9..];
    let rest = &rest[rest.find("<td>")? + 4..];
    Some(rest[..rest.find("</td>")?].trim())
}

/// `23 (jobs=16 ncpu=32)` → `(16, 32)`.
fn jobs(cell: &str) -> Option<(u32, u32)> {
    let num = |k: &str| -> Option<u32> {
        let r = &cell[cell.find(k)? + k.len()..];
        r[..r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len())]
            .parse()
            .ok()
    };
    Some((num("jobs=")?, num("ncpu=")?))
}

/// `2026-11-17T09:57:58.540254618Z` → Unix milliseconds (UTC only, which
/// is what cargo writes).
#[allow(
    clippy::many_single_char_names,
    reason = "the civil-date formula's names"
)]
fn unix_ms(ts: &str) -> Option<f64> {
    let ts = ts.strip_suffix('Z')?;
    let (date, time) = ts.split_once('T')?;
    let mut d = date.splitn(3, '-').map(str::parse::<i64>);
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.splitn(3, ':');
    let h: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next()?.parse().ok()?;
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    #[allow(clippy::cast_precision_loss, reason = "well within f64's 2^53")]
    let whole = ((days * 24 + h) * 60 + min) as f64;
    Some((whole * 60.0 + sec) * 1000.0)
}

/// The newest report in a target directory.
#[must_use]
pub fn newest_report(target_dir: &Path) -> Option<PathBuf> {
    let dir = target_dir.join("cargo-timings");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("cargo-timing-") && n.ends_with(".html")
        })
        .max_by_key(std::fs::DirEntry::file_name)
        .map(|e| e.path())
}

/// What a report says.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Analysis {
    /// Wall time of the build, seconds.
    pub wall: f64,
    /// Units compiled (not fresh).
    pub compiled: usize,
    /// Units in the build.
    pub total_units: usize,
    /// Sum of unit durations over wall time.
    pub parallelism: f64,
    /// Average CPU use, percent, if sampled.
    pub cpu_avg: Option<f64>,
    /// Cargo's `jobs` and the CPU count, from the report.
    pub jobs: Option<(u32, u32)>,
    /// The chain of units that decided the wall time, first to last.
    pub critical_path: Vec<PathStep>,
    /// Seconds of the critical path between a step's dependency finishing
    /// and the step starting.
    pub path_gaps: f64,
    /// Seconds of the critical path spent waiting inside the cache wrapper.
    pub path_waits: f64,
    /// Long units that ran mostly alone.
    pub serial: Vec<Serial>,
    /// Slowest units.
    pub slowest: Vec<(String, f64)>,
    /// Built units with long code generation: unit, duration, codegen.
    pub long_codegen: Vec<(String, f64, f64)>,
    /// Check units with a long part after their metadata: unit, duration,
    /// seconds after metadata.
    pub post_metadata: Vec<(String, f64, f64)>,
    /// Slow build scripts.
    pub build_scripts: Vec<(String, f64)>,
    /// Seconds by kind of unit.
    pub kinds: Vec<KindTime>,
    /// Binaries and tests.
    pub linked: Linked,
    /// What the cache wrapper did, when its records were joined.
    pub wrapper: Option<WrapperSummary>,
    /// How much slower units ran than in an earlier report.
    pub slowdown: Option<Slowdown>,
    /// Workspace crates recompiled, when the workspace is known.
    pub rebuilt: Option<Rebuilt>,
}

/// One step of the critical path.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PathStep {
    /// Unit label.
    pub unit: String,
    /// Start.
    pub start: f64,
    /// Duration.
    pub duration: f64,
    /// Waited for metadata only (pipelined) or the full build.
    pub via_rmeta: bool,
    /// Seconds between what it waited for finishing and it starting.
    pub gap: f64,
    /// What the cache wrapper did with it.
    pub cache: Option<Outcome>,
    /// Seconds it waited inside the cache wrapper.
    pub wait: f64,
}

/// A unit that ran mostly alone.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Serial {
    /// Unit label.
    pub unit: String,
    /// Duration.
    pub duration: f64,
    /// Average number of other units running alongside.
    pub others: f64,
}

/// What else the analysis can use besides the report.
#[derive(Debug, Clone, Copy, Default)]
pub struct Extra<'a> {
    /// An earlier report of the same workspace (a `cargo check` of the
    /// same edit, or an earlier build) to compare unit times with.
    pub baseline: Option<&'a Timings>,
    /// The workspace's crates and their dependencies.
    pub workspace: Option<&'a Workspace>,
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
        #[allow(clippy::cast_precision_loss)]
        let n = t.cpu.len() as f64;
        a.cpu_avg = Some(t.cpu.iter().map(|c| c.1).sum::<f64>() / n);
    }
    let path = critical_path(t);
    a.path_gaps = path.iter().map(|(_, s)| s.gap).sum();
    a.path_waits = path.iter().map(|(_, s)| s.wait).sum();
    let on_path: Vec<(usize, f64)> = path.iter().map(|(p, s)| (*p, s.duration)).collect();
    a.kinds = breakdown::kinds(&ran, &on_path, &t.units);
    a.critical_path = path.into_iter().map(|(_, s)| s).collect();
    a.serial = serial(&ran);
    per_unit(&mut a, &ran);
    a.linked = breakdown::linked(&ran, end);
    a.wrapper = breakdown::wrapper_summary(&ran);
    a.slowdown = extra.baseline.and_then(|b| breakdown::slowdown(&ran, b));
    a.rebuilt = extra.workspace.and_then(|w| breakdown::rebuilt(&ran, w));
    a
}

/// Slowest units, long codegen, long post-metadata analysis, build scripts.
fn per_unit(a: &mut Analysis, ran: &[&Unit]) {
    let mut slowest: Vec<&&Unit> = ran.iter().collect();
    slowest.sort_by(|x, y| y.duration.total_cmp(&x.duration));
    a.slowest = slowest
        .iter()
        .take(10)
        .map(|u| (u.label(), u.duration))
        .collect();
    // A check unit has no LLVM codegen: cargo splits every unit at its
    // metadata, and for a check the part after it is borrowck, MIR checks
    // and saving the incremental state.
    for u in ran {
        let Some(after) = u.codegen() else { continue };
        if after < 5.0 || after < 0.3 * u.duration {
            continue;
        }
        let row = (u.label(), u.duration, after);
        match u.kind() {
            Kind::Check => a.post_metadata.push(row),
            Kind::Lib | Kind::Link => a.long_codegen.push(row),
            Kind::BuildScript => {}
        }
    }
    a.long_codegen.sort_by(|x, y| y.2.total_cmp(&x.2));
    a.post_metadata.sort_by(|x, y| y.2.total_cmp(&x.2));
    a.build_scripts = ran
        .iter()
        .filter(|u| u.is_build_script() && u.duration >= 3.0)
        .map(|u| (u.label(), u.duration))
        .collect();
    a.build_scripts.sort_by(|x, y| y.1.total_cmp(&x.1));
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
                duration: u.duration,
                others,
            });
        }
    }
    out.sort_by(|x, y| y.duration.total_cmp(&x.duration));
    out
}

/// The chain of units that decided the wall time, first to last, with
/// each unit's position in `t.units`. Walks back from the unit that ended
/// last through whichever unblocker finished last before it started.
fn critical_path(t: &Timings) -> Vec<(usize, PathStep)> {
    // Index units by `i` (or array position for old reports).
    let by_i: HashMap<usize, usize> = t
        .units
        .iter()
        .enumerate()
        .map(|(pos, u)| (u.i.unwrap_or(pos), pos))
        .collect();
    // Who unblocked whom: unit pos -> [(unblocker pos, via rmeta)].
    let mut unblockers: HashMap<usize, Vec<(usize, bool)>> = HashMap::new();
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
                start: u.start,
                duration: u.duration,
                via_rmeta: prev.is_some_and(|(_, r, _)| r),
                gap: (u.start - ready).max(0.0),
                cache: u.wrapper.as_ref().map(|w| w.outcome),
                wait: u.wrapper.as_ref().map_or(0.0, |w| w.wait),
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

/// A section for a report.
#[must_use]
pub fn section(a: &Analysis, source: &str) -> Section {
    let mut s = Section::new("timings");
    s.fact("report", source);
    s.fact("wall", secs(a.wall));
    s.fact(
        "units",
        format!(
            "{} compiled, {} fresh",
            a.compiled,
            a.total_units - a.compiled
        ),
    );
    s.fact(
        "parallelism",
        format!(
            "{:.1} units at a time{}{}",
            a.parallelism,
            a.cpu_avg
                .map_or(String::new(), |c| format!(", CPU {c:.0}% on average")),
            a.jobs
                .map_or(String::new(), |(j, n)| format!(", jobs={j} on {n} CPUs"))
        ),
    );
    if let Some(w) = &a.wrapper {
        s.fact("cache", wrapper_fact(w));
    }
    if let Some(b) = &a.slowdown {
        s.fact(
            "compared with",
            format!(
                "an earlier {} ({} wall): {} units, {:.1}x their time there (median)",
                if b.baseline_check { "check" } else { "build" },
                secs(b.baseline_wall),
                b.compared,
                b.median
            ),
        );
    }
    if let Some(r) = &a.rebuilt {
        s.fact(
            "rebuilt",
            format!(
                "{} workspace crates, from {}",
                r.crates,
                ellipsize(&r.roots.join(", "), 60)
            ),
        );
    }
    path_table(&mut s, a);
    kinds_table(&mut s, a);
    if !a.slowest.is_empty() {
        let rows: Vec<Vec<String>> = a
            .slowest
            .iter()
            .map(|(u, d)| {
                vec![
                    secs(*d),
                    format!("{:.0}%", pct(*d, a.wall)),
                    ellipsize(u, 70),
                ]
            })
            .collect();
        s.details.push(format!(
            "slowest units\n{}",
            table(&["took", "% wall", "unit"], &rows)
        ));
    }
    s.extend(findings(a));
    s.data = serde_json::to_value(a).unwrap_or_default();
    s
}

fn wrapper_fact(w: &WrapperSummary) -> String {
    let parts: Vec<String> = [
        (w.hits, "cache hits"),
        (w.misses, "compiled on a miss"),
        (
            w.incremental,
            "compiled incrementally (passthrough, no cache lookup)",
        ),
        (w.other, "other passthroughs"),
    ]
    .iter()
    .filter(|((n, _), _)| *n > 0)
    .map(|((n, t), what)| format!("{n} {what} ({})", secs(*t)))
    .collect();
    format!(
        "{}; {} waiting for the wrapper{}",
        parts.join(", "),
        secs(w.wait),
        if w.unmatched > 0 {
            format!(", {} units without a wrapper record", w.unmatched)
        } else {
            String::new()
        }
    )
}

/// Seconds, or `-` under 10 ms.
fn small(s: f64) -> String {
    if s < 0.01 { "-".into() } else { secs(s) }
}

fn path_table(s: &mut Section, a: &Analysis) {
    if a.critical_path.is_empty() {
        return;
    }
    let cache = a.wrapper.is_some();
    let rows: Vec<Vec<String>> = a
        .critical_path
        .iter()
        .map(|p| {
            let mut row = vec![secs(p.start), secs(p.duration), small(p.gap)];
            if cache {
                row.push(p.cache.map_or("-", Outcome::label).into());
                row.push(small(p.wait));
            }
            row.push(if p.via_rmeta { "meta" } else { "full" }.into());
            row.push(ellipsize(&p.unit, 70));
            row
        })
        .collect();
    let headers: &[&str] = if cache {
        &["start", "took", "gap", "cache", "wait", "waited", "unit"]
    } else {
        &["start", "took", "gap", "waited", "unit"]
    };
    s.details.push(format!(
        "critical path (each step waited for the one above it; gap: idle after it was ready)\n{}",
        table(headers, &rows)
    ));
}

fn kinds_table(s: &mut Section, a: &Analysis) {
    if a.kinds.is_empty() {
        return;
    }
    let busy: f64 = a.kinds.iter().map(|k| k.busy).sum();
    let rows: Vec<Vec<String>> = a
        .kinds
        .iter()
        .map(|k| {
            vec![
                secs(k.busy),
                format!("{:.0}%", pct(k.busy, busy)),
                secs(k.on_path),
                k.units.to_string(),
                k.kind.label().into(),
            ]
        })
        .collect();
    s.details.push(format!(
        "where the time went\n{}",
        table(&["busy", "% busy", "on path", "units", "kind"], &rows)
    ));
}

/// The newest report older than `report` in the same directory that is a
/// `cargo check`, to compare a build with (`None` when `report` is itself
/// a check).
#[must_use]
pub fn check_before(report: &Path) -> Option<(PathBuf, Timings)> {
    let dir = report.parent()?;
    let name = report.file_name()?.to_owned();
    let mut older: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.starts_with("cargo-timing-") && s.ends_with(".html") && n < name
        })
        .map(|e| e.path())
        .collect();
    older.sort();
    older.into_iter().rev().take(10).find_map(|p| {
        let t = Timings::parse_html(&std::fs::read_to_string(&p).ok()?).ok()?;
        t.is_check().then_some((p, t))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(a.critical_path[1].via_rmeta);
        assert_eq!(a.serial[0].unit, "big (check)");
        assert_eq!(a.compiled, 4);
        let ids: Vec<_> = findings(&a).iter().map(|f| f.id).collect();
        assert_eq!(ids, ["serial-crate"]);
    }

    #[test]
    fn parses_html_and_old_format() {
        let html = "<script>\nconst UNIT_DATA = [\n  {\"i\": 0, \"name\": \"x\", \"version\": \"1\", \"mode\": \"todo\", \"target\": \"\", \"start\": 0.5, \"duration\": 10.0, \"rmeta_time\": 2.0, \"unlocked_units\": []}\n];\nconst CPU_USAGE = [[0.1, 40.0]];\n</script>";
        let t = Timings::parse_html(html).unwrap();
        assert_eq!(t.units.len(), 1);
        assert!((t.units[0].codegen().unwrap() - 8.0).abs() < 1e-9);
        assert!((t.units[0].rmeta_end() - 2.5).abs() < 1e-9);
        let a = analyze(&t);
        assert_eq!(a.long_codegen.len(), 1);
        assert!(Timings::parse_html("<html></html>").is_err());
    }

    #[test]
    fn real_report() {
        let html = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cargo-timing.html"
        ))
        .unwrap();
        let t = Timings::parse_html(&html).unwrap();
        let a = analyze(&t);
        assert!(a.compiled > 0 && a.wall > 0.0);
        assert_ne!(a.critical_path.len(), 0);
    }

    fn fixture_file(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn ids(a: &Analysis) -> Vec<&'static str> {
        findings(a).iter().map(|f| f.id).collect()
    }

    #[test]
    fn report_header() {
        assert_eq!(jobs("23 (jobs=16 ncpu=32)"), Some((16, 32)));
        // 2026-11-17T09:57:58.5Z, checked with `date -d ... +%s`.
        let ms = unix_ms("2026-11-17T09:57:58.540254618Z").unwrap();
        assert!((ms - 1_794_909_478_540.254_6).abs() < 1.0);
        assert_eq!(unix_ms("1970-01-01T00:00:00Z"), Some(0.0));
        assert!(unix_ms("2026-11-17 09:57:58").is_none());
    }

    #[test]
    fn kinds() {
        let k = |target: &str| {
            Unit {
                target: target.into(),
                ..Unit::default()
            }
            .kind()
        };
        assert_eq!(k(" (check)"), Kind::Check);
        assert_eq!(k(" x \"test\" (check-test)"), Kind::Check);
        assert_eq!(k(""), Kind::Lib);
        assert_eq!(k(" tala-agent \"bin\""), Kind::Link);
        assert_eq!(k(" (test)"), Kind::Link);
        assert_eq!(k(" build-script (run)"), Kind::BuildScript);
    }

    /// A check unit's part after metadata is analysis, not codegen (#11).
    #[test]
    fn check_units_have_no_codegen() {
        let mut u = unit(1, "tala-config", 0.0, 14.8, &[], &[]);
        u.sections = Some(vec![
            (
                "frontend".into(),
                Span {
                    start: 0.0,
                    end: 9.3,
                },
            ),
            (
                "codegen".into(),
                Span {
                    start: 9.3,
                    end: 14.8,
                },
            ),
        ]);
        let mut built = u.clone();
        built.target = String::new();
        let a = analyze(&Timings {
            units: vec![u],
            ..Timings::default()
        });
        assert_eq!(a.long_codegen.len(), 0);
        assert_eq!(a.post_metadata.len(), 1);
        assert!(ids(&a).contains(&"post-metadata-analysis"));
        assert!(!ids(&a).contains(&"long-codegen"));
        let a = analyze(&Timings {
            units: vec![built],
            ..Timings::default()
        });
        assert!(ids(&a).contains(&"long-codegen"));
        assert!(!ids(&a).contains(&"post-metadata-analysis"));
    }

    /// The real build after a one-line tala-core edit, joined with kache's
    /// records and compared with the check of the same edit (#12, #13).
    #[test]
    #[allow(clippy::many_single_char_names, reason = "t: timings, a: analysis")]
    fn real_build_with_kache_and_check() {
        let mut t = Timings::parse_html(&fixture_file("timings-build.html")).unwrap();
        let check = Timings::parse_html(&fixture_file("timings-check.html")).unwrap();
        assert_eq!(t.jobs, Some((16, 32)));
        assert!(check.is_check() && !t.is_check());
        let v: serde_json::Value = serde_json::from_str(&fixture_file("kache-trace.json")).unwrap();
        let events = wrapper::kache_events(&v);
        let matched = wrapper::join(&mut t, &events);
        let compiled = t.ran().filter(|u| u.kind() != Kind::BuildScript).count();
        assert_eq!(matched, compiled);
        // Everything that depends on tala-core.
        let ws = Workspace {
            deps: t
                .ran()
                .filter(|u| u.name.starts_with("tala-"))
                .map(|u| {
                    let deps = if u.name == "tala-core" {
                        alloc::collections::BTreeSet::default()
                    } else {
                        ["tala-core".to_owned()].into()
                    };
                    (u.name.clone(), deps)
                })
                .collect(),
        };
        let a = analyze_with(
            &t,
            Extra {
                baseline: Some(&check),
                workspace: Some(&ws),
            },
        );
        let w = a.wrapper.as_ref().unwrap();
        assert_eq!(w.incremental.0, matched);
        assert_eq!(w.hits.0 + w.misses.0, 0);
        assert!(
            a.critical_path
                .iter()
                .all(|p| p.cache == Some(Outcome::Incr))
        );
        assert!(a.critical_path.last().unwrap().unit.ends_with("\"bin\""));
        let r = a.rebuilt.as_ref().unwrap();
        assert_eq!(r.roots, ["tala-core"]);
        assert!(r.crates >= 80);
        let sd = a.slowdown.as_ref().unwrap();
        assert!(sd.baseline_check && sd.compared >= 50);
        assert!(sd.median < 3.0);
        let found = ids(&a);
        assert!(found.contains(&"edit-rebuilt-many"));
        assert!(!found.contains(&"contention"));
        let s = section(&a, "x");
        assert!(
            s.details
                .iter()
                .any(|d| d.starts_with("where the time went"))
        );
        assert!(s.facts.iter().any(|f| f.key == "cache"));
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

    /// The 566 s build from #13: a chain of libraries 10x slower than their
    /// check, then six tool binaries of ~2 minutes each.
    fn slow_build() -> (Timings, Timings) {
        let mut units = Vec::new();
        let mut checks = Vec::new();
        for n in 0..6_u8 {
            let name = format!("tala-lib{n}");
            let start = 60.0 * f64::from(n);
            let i = usize::from(n);
            units.push(lib(i, &name, "", start, 60.0, &[i + 1]));
            checks.push(lib(i, &name, " (check)", 6.0 * f64::from(n), 6.0, &[]));
        }
        for b in 0..6_u8 {
            let name = format!("tala-tool-{b}");
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
        (
            Timings {
                units,
                ..Timings::default()
            },
            Timings {
                units: checks,
                ..Timings::default()
            },
        )
    }

    #[test]
    fn links_and_contention() {
        let (t, check) = slow_build();
        let a = analyze_with(
            &t,
            Extra {
                baseline: Some(&check),
                workspace: None,
            },
        );
        assert!((a.wall - 554.0).abs() < 1e-9);
        assert_eq!(a.linked.units, 6);
        assert!(a.linked.tail > 190.0);
        let found = ids(&a);
        assert!(found.contains(&"slow-links"), "{found:?}");
        assert!(found.contains(&"contention"), "{found:?}");
        assert!(!found.contains(&"slow-build"));
    }

    /// Never "nothing to fix" on a slow build.
    #[test]
    fn slow_build_safety_net() {
        let units = (0..20)
            .map(|n| lib(n, &format!("c{n}"), "", 5.0 * n as f64, 30.0, &[]))
            .collect();
        let t = Timings {
            units,
            ..Timings::default()
        };
        let a = analyze(&t);
        assert!(a.wall > 60.0);
        assert_eq!(ids(&a), ["slow-build"]);
        // Fast builds stay quiet.
        assert!(ids(&analyze(&fixture())).iter().all(|i| *i != "slow-build"));
    }

    #[test]
    fn waiting_on_the_critical_path() {
        let mut units = vec![
            lib(0, "a", "", 0.0, 10.0, &[1]),
            lib(1, "b", "", 25.0, 20.0, &[]),
        ];
        units[1].wrapper = Some(WrapperRun {
            outcome: Outcome::Miss,
            compile: 8.0,
            wait: 12.0,
            elapsed: 20.0,
        });
        let a = analyze(&Timings {
            units,
            ..Timings::default()
        });
        assert!((a.path_gaps - 15.0).abs() < 1e-9);
        assert!((a.path_waits - 12.0).abs() < 1e-9);
        let found = ids(&a);
        assert!(found.contains(&"critical-path-waiting"), "{found:?}");
        assert!(found.contains(&"wrapper-wait"), "{found:?}");
    }
}
