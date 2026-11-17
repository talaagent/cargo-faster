//! Reading `cargo build --timings` reports: the critical path, crates that
//! serialize the build, long code generation and build scripts.

use crate::finding::{Finding, Section, Severity, table};
use crate::fmt::{ellipsize, pct, secs};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
}

/// A parsed timings report.
#[derive(Debug, Clone, Default)]
pub struct Timings {
    /// Every unit, fresh ones included.
    pub units: Vec<Unit>,
    /// `(seconds, percent)` CPU samples, if present.
    pub cpu: Vec<(f64, f64)>,
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
        Ok(Self { units, cpu })
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
    /// The chain of units that decided the wall time, first to last.
    pub critical_path: Vec<PathStep>,
    /// Long units that ran mostly alone.
    pub serial: Vec<Serial>,
    /// Slowest units.
    pub slowest: Vec<(String, f64)>,
    /// Units with long code generation.
    pub long_codegen: Vec<(String, f64, f64)>,
    /// Slow build scripts.
    pub build_scripts: Vec<(String, f64)>,
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

/// Analyze a report.
#[must_use]
#[allow(
    clippy::many_single_char_names,
    clippy::too_many_lines,
    reason = "a: analysis, t: timings, u: unit; one pass over the units"
)]
pub fn analyze(t: &Timings) -> Analysis {
    let ran: Vec<&Unit> = t.ran().collect();
    let mut a = Analysis {
        total_units: t.units.len(),
        compiled: ran.len(),
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
    a.wall = ran.iter().map(|u| u.end()).fold(0.0, f64::max) - first;
    let busy: f64 = ran.iter().map(|u| u.duration).sum();
    a.parallelism = busy / a.wall.max(1e-9);
    if !t.cpu.is_empty() {
        #[allow(clippy::cast_precision_loss)]
        let n = t.cpu.len() as f64;
        a.cpu_avg = Some(t.cpu.iter().map(|c| c.1).sum::<f64>() / n);
    }

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
        return a;
    };
    let mut path = Vec::new();
    for _ in 0..t.units.len() {
        let u = &t.units[cur];
        path.push(PathStep {
            unit: u.label(),
            start: u.start,
            duration: u.duration,
            via_rmeta: false,
        });
        let prev = unblockers.get(&cur).and_then(|ps| {
            ps.iter()
                .filter(|(p, _)| t.units[*p].duration > 0.0)
                .map(|&(p, r)| {
                    (
                        p,
                        r,
                        if r {
                            t.units[p].rmeta_end()
                        } else {
                            t.units[p].end()
                        },
                    )
                })
                .filter(|&(_, _, done)| done <= u.start + 0.05)
                .max_by(|x, y| x.2.total_cmp(&y.2))
        });
        match prev {
            Some((p, r, _)) => {
                // `u` could start once `p`'s metadata (r) or all of it was done.
                if let Some(step) = path.last_mut() {
                    step.via_rmeta = r;
                }
                cur = p;
            }
            None => break,
        }
    }
    path.reverse();
    a.critical_path = path;

    // Overlap: for each long unit, how many others ran on average.
    for u in &ran {
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
            a.serial.push(Serial {
                unit: u.label(),
                duration: u.duration,
                others,
            });
        }
    }
    a.serial.sort_by(|x, y| y.duration.total_cmp(&x.duration));

    let mut slowest: Vec<&&Unit> = ran.iter().collect();
    slowest.sort_by(|x, y| y.duration.total_cmp(&x.duration));
    a.slowest = slowest
        .iter()
        .take(10)
        .map(|u| (u.label(), u.duration))
        .collect();
    a.long_codegen = ran
        .iter()
        .filter_map(|u| u.codegen().map(|c| (u, c)))
        .filter(|(u, c)| *c >= 5.0 && *c >= 0.3 * u.duration)
        .map(|(u, c)| (u.label(), u.duration, c))
        .collect();
    a.long_codegen.sort_by(|x, y| y.2.total_cmp(&x.2));
    a.build_scripts = ran
        .iter()
        .filter(|u| u.is_build_script() && u.duration >= 3.0)
        .map(|u| (u.label(), u.duration))
        .collect();
    a.build_scripts.sort_by(|x, y| y.1.total_cmp(&x.1));
    a
}

/// Findings for an analysis.
#[must_use]
pub fn findings(a: &Analysis) -> Vec<Finding> {
    let mut out = Vec::new();
    if a.wall <= 0.0 {
        return out;
    }
    if let Some(s) = a.serial.first()
        && pct(s.duration, a.wall) >= 25.0
    {
        let list: Vec<String> = a
            .serial
            .iter()
            .take(3)
            .map(|s| {
                format!(
                    "{} ({}, {:.1} others alongside)",
                    s.unit,
                    secs(s.duration),
                    s.others
                )
            })
            .collect();
        out.push(
            Finding::new("serial-crate", if pct(s.duration, a.wall) >= 50.0 { Severity::High } else { Severity::Medium }, format!("{} serializes the build", s.unit), "the-critical-path")
                .saw(format!("{} took {} of the {} build while little else could run: {}.", s.unit, secs(s.duration), secs(a.wall), list.join("; ")))
                .why("Cargo parallelizes across crates, but one crate is compiled by one rustc. A big crate everything else waits for (or that waits for everything) runs on a few cores while the rest idle.")
                .fix(format!("Find out what's slow inside it with `cargo faster check-cost -p {}`. If it's just big, split it (see `cargo faster rdeps`) so the pieces build in parallel and edits rebuild less.", s.unit.split(' ').next().unwrap_or(&s.unit))),
        );
    }
    if let Some(top) = a
        .critical_path
        .iter()
        .max_by(|x, y| x.duration.total_cmp(&y.duration))
        && a.critical_path.len() > 1
        && pct(top.duration, a.wall) >= 40.0
        && !a.serial.iter().any(|s| s.unit == top.unit)
    {
        out.push(
            Finding::new("critical-path-dominated", Severity::Medium, format!("{} is most of the critical path", top.unit), "the-critical-path")
                .saw(format!("{} of the {} wall time is {} on the critical path.", secs(top.duration), secs(a.wall), top.unit))
                .why("Speeding up crates off the critical path doesn't change the wall time. This one does.")
                .fix(format!("Profile it: `cargo faster check-cost -p {}`.", top.unit.split(' ').next().unwrap_or(&top.unit))),
        );
    }
    if let Some((u, d, c)) = a.long_codegen.first() {
        out.push(
            Finding::new("long-codegen", Severity::Medium, format!("Code generation takes {} in {u}", secs(*c)), "generic-code-and-monomorphization")
                .saw(format!("{u} spent {} of {} in codegen.", secs(*c), secs(*d)))
                .why("Codegen time grows with the amount of monomorphized code: every generic function instantiated with each type it's used with.")
                .fix("Run `cargo llvm-lines -p <crate>` (on its own checkout, not with a shared cache) to find what's instantiated most, and make the bulk of those functions non-generic."),
        );
    }
    if let Some((u, d)) = a.build_scripts.first() {
        out.push(
            Finding::new("slow-build-script", Severity::Low, format!("Build script {u} takes {}", secs(*d)), "dependencies-and-features")
                .saw(format!("Slow build scripts: {}.", a.build_scripts.iter().take(4).map(|(u, d)| format!("{u} {}", secs(*d))).collect::<Vec<_>>().join(", ")))
                .why("Build scripts run before their crate compiles, often compiling C code (ring, openssl, zstd) or generating code (protobuf). They rerun when their inputs change.")
                .fix("Check `cargo:rerun-if-changed` lines so it doesn't rerun needlessly, prefer pure-Rust alternatives, or link a system library (`*-sys` crates often support `pkg-config`)."),
        );
    }
    if a.compiled >= 50 && a.parallelism < 3.0 && a.wall >= 30.0 {
        out.push(
            Finding::new("low-parallelism", Severity::Low, format!("Only {:.1} crates compiled at a time on average", a.parallelism), "the-build-timeline")
                .saw(format!("{} units over {} wall: {:.1} busy on average{}.", a.compiled, secs(a.wall), a.parallelism, a.cpu_avg.map_or(String::new(), |c| format!(", CPU {c:.0}%"))))
                .why("The dependency graph decides how wide a build can go. Long chains of crates that each wait on the previous one leave cores idle.")
                .fix("Look at the critical path above. Break long chains: move what downstream crates need into small crates early in the graph."),
        );
    }
    out
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
            "{:.1} units at a time{}",
            a.parallelism,
            a.cpu_avg
                .map_or(String::new(), |c| format!(", CPU {c:.0}% on average"))
        ),
    );
    if !a.critical_path.is_empty() {
        let rows: Vec<Vec<String>> = a
            .critical_path
            .iter()
            .map(|p| {
                vec![
                    secs(p.start),
                    secs(p.duration),
                    if p.via_rmeta {
                        "meta".into()
                    } else {
                        "full".into()
                    },
                    ellipsize(&p.unit, 70),
                ]
            })
            .collect();
        s.details.push(format!(
            "critical path (each step waited for the one above it)\n{}",
            table(&["start", "took", "waited", "unit"], &rows)
        ));
    }
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
}
