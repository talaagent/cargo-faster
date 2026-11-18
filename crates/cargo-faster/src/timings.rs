//! Reading `cargo build --timings` reports: the critical path, crates that
//! serialize the build, long code generation, linking, build scripts, and
//! (joined with the cache wrapper's records and an earlier report) cache
//! hits, waits and contention.
//!
//! Parsing and the analysis of the report itself live in
//! [`cargo_timings_parse`]; this module joins in the cache wrapper and the
//! workspace, and turns the result into findings.

pub mod breakdown;
mod findings;
pub mod run;
pub mod wrapper;

use crate::finding::{Section, table};
use crate::fmt::{ellipsize, pct, secs};
use alloc::collections::BTreeSet;
use breakdown::WrapperSummary;
pub use breakdown::workspace_from_metadata;
pub use cargo_timings_parse::{
    Extra, FirstBuild, Jobs, Kind, KindTime, Linked, PathStep, Rebuilt, Serial, Slowdown, Span,
    Timings, Unit, UnitOf, UnitSecs, UnitTime, Waited, Workspace, check_before, newest_report,
    reports,
};
pub use findings::{build_failed, findings};
use serde_json::Value;
use wrapper::{Outcome, WrapperRun};

/// How old a reused report is, for its fact, and a warning when it's
/// stale: more than a day old, or older than the last change to
/// `Cargo.lock` (so the dependencies may have changed since).
#[must_use]
pub fn report_age(age_s: f64, older_than_lock: bool) -> (String, Option<String>) {
    let age = if age_s < 120.0 {
        format!("{age_s:.0} s ago")
    } else if age_s < 2.0 * 3600.0 {
        format!("{:.0} min ago", age_s / 60.0)
    } else if age_s < 2.0 * 86_400.0 {
        format!("{:.0} h ago", age_s / 3600.0)
    } else {
        format!("{:.0} days ago", age_s / 86_400.0)
    };
    let why = if age_s > 86_400.0 {
        Some("more than a day old")
    } else if older_than_lock {
        Some("older than Cargo.lock, so dependencies may have changed since")
    } else {
        None
    };
    let warn = why.map(|w| {
        format!("this report is {w}: `cargo faster timings --run` times the build as it is now")
    });
    (age, warn)
}

/// What a report says, with what the cache wrapper did on the critical
/// path. Derefs to the report's own analysis.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Analysis {
    /// The analysis of the report on its own.
    pub report: cargo_timings_parse::Analysis,
    /// For each step of `critical_path`: what the cache wrapper did.
    pub path: Vec<PathCache>,
    /// Seconds of the critical path spent waiting inside the cache wrapper.
    pub path_waits: f64,
    /// What the cache wrapper did, when its records were joined.
    pub wrapper: Option<WrapperSummary>,
    /// The workspace members' package names, when the workspace is known:
    /// only those can be profiled with `check-cost -p`.
    pub members: Option<BTreeSet<String>>,
}

impl core::ops::Deref for Analysis {
    type Target = cargo_timings_parse::Analysis;

    fn deref(&self) -> &Self::Target {
        &self.report
    }
}

/// What the cache wrapper did with one critical path step.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PathCache {
    /// What it did, if it has a record of the unit.
    pub cache: Option<Outcome>,
    /// Seconds the unit waited inside the wrapper.
    pub wait: f64,
}

impl Analysis {
    /// The `--json` data: the report's analysis, with `path_waits_s`,
    /// `wrapper`, and each critical path step's `cache` and `wait_s`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(&self.report).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.insert("path_waits_s".into(), self.path_waits.into());
            o.insert(
                "wrapper".into(),
                serde_json::to_value(&self.wrapper).unwrap_or_default(),
            );
            if let Some(Value::Array(steps)) = o.get_mut("critical_path") {
                for (step, c) in steps.iter_mut().zip(&self.path) {
                    if let Some(step) = step.as_object_mut() {
                        step.insert(
                            "cache".into(),
                            serde_json::to_value(c.cache).unwrap_or_default(),
                        );
                        step.insert("wait_s".into(), c.wait.into());
                    }
                }
            }
        }
        v
    }
}

/// Analyze a report on its own.
#[must_use]
pub fn analyze(t: &Timings) -> Analysis {
    analyze_with(t, Extra::default(), &[])
}

/// Analyze a report, comparing with a baseline and the workspace graph
/// when given, with the cache wrapper's record of each unit (indexed like
/// `t.units`, see [`wrapper::join`]; empty when there are none).
#[must_use]
pub fn analyze_with(t: &Timings, extra: Extra<'_>, runs: &[Option<WrapperRun>]) -> Analysis {
    let run = |p: usize| runs.get(p).and_then(Option::as_ref);
    let path: Vec<PathCache> = cargo_timings_parse::critical_path(t)
        .iter()
        .map(|(p, _)| PathCache {
            cache: run(*p).map(|w| w.outcome),
            wait: run(*p).map_or(0.0, |w| w.wait),
        })
        .collect();
    Analysis {
        report: cargo_timings_parse::analyze_with(t, extra),
        path_waits: path.iter().map(|p| p.wait).sum(),
        path,
        wrapper: breakdown::wrapper_summary(t, runs),
        members: extra.workspace.map(|w| w.deps.keys().cloned().collect()),
    }
}

/// The workspace edges on the critical path: `(dependent, dependency,
/// seconds the dependency took on the path)`, for each step that waited
/// for a workspace crate it depends on directly.
#[must_use]
pub fn path_edges(a: &Analysis, ws: &Workspace) -> Vec<(String, String, Option<f64>)> {
    cargo_timings_parse::path_edges(a, ws)
        .into_iter()
        .map(|(user, dep, took)| (user, dep, Some(took)))
        .collect()
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
            a.jobs.map_or(String::new(), |j| format!(
                ", jobs={} on {} CPUs",
                j.jobs, j.ncpu
            ))
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
            if r.first_build.is_some() {
                format!(
                    "{} workspace crates: a first build in this target directory",
                    r.crates
                )
            } else {
                format!(
                    "{} workspace crates, from {}",
                    r.crates,
                    ellipsize(&r.roots.join(", "), 60)
                )
            },
        );
    }
    path_table(&mut s, a);
    kinds_table(&mut s, a);
    if !a.slowest.is_empty() {
        let rows: Vec<Vec<String>> = a
            .slowest
            .iter()
            .map(|u| {
                vec![
                    secs(u.duration),
                    format!("{:.0}%", pct(u.duration, a.wall)),
                    ellipsize(&u.unit, 70),
                ]
            })
            .collect();
        s.details.push(format!(
            "slowest units\n{}",
            table(&["took", "% wall", "unit"], &rows)
        ));
    }
    s.extend(findings(a));
    s.data = a.to_json();
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
        .zip(
            a.path
                .iter()
                .chain(core::iter::repeat(&PathCache::default())),
        )
        .map(|(p, c)| {
            let mut row = vec![secs(p.start), secs(p.duration), small(p.gap)];
            if cache {
                row.push(c.cache.map_or("-", Outcome::label).into());
                row.push(small(c.wait));
            }
            row.push(
                match p.waited {
                    Waited::Meta => "meta",
                    Waited::Full => "full",
                }
                .into(),
            );
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
        assert_eq!(a.critical_path[1].waited, Waited::Meta);
        assert_eq!(a.serial[0].unit, "big (check)");
        assert_eq!(a.compiled, 4);
        let ids: Vec<_> = findings(&a).iter().map(|f| f.id).collect();
        assert_eq!(ids, ["serial-crate"]);
    }

    #[test]
    fn advice_depends_on_whether_a_crate_is_a_member() {
        let fix = |members: Option<&[&str]>| {
            let ws = members.map(|m| Workspace {
                deps: m
                    .iter()
                    .map(|n| ((*n).to_owned(), BTreeSet::new()))
                    .collect(),
            });
            let a = analyze_with(
                &fixture(),
                Extra {
                    baseline: None,
                    workspace: ws.as_ref(),
                    first_build: None,
                },
                &[],
            );
            let f = findings(&a)
                .into_iter()
                .find(|f| f.id == "serial-crate")
                .unwrap();
            f.fix
        };
        // `big` is a member: profile it, or split it.
        let member = fix(Some(&["big", "a"]));
        assert!(
            member.contains("`cargo faster check-cost -p big`"),
            "{member}"
        );
        assert!(member.contains("split it"), "{member}");
        // A dependency: no check-cost, which can't profile it.
        let dep = fix(Some(&["a"]));
        assert!(!dep.contains("check-cost"), "{dep}");
        assert!(
            dep.contains("big is a dependency") && dep.contains("`cargo faster deps`"),
            "{dep}"
        );
        // No workspace (a report read on its own): both, conditionally.
        let unknown = fix(None);
        assert!(unknown.contains("If big is a workspace crate"), "{unknown}");
        assert!(unknown.contains("If it's a dependency"), "{unknown}");
    }

    #[test]
    fn nothing_rebuilt_and_failed_builds() {
        let t = Timings {
            units: vec![unit(1, "a", 0.0, 0.0, &[], &[])],
            ..Timings::default()
        };
        let f = findings(&analyze(&t));
        assert_eq!(
            f.iter().map(|f| f.id).collect::<Vec<_>>(),
            ["nothing-rebuilt"]
        );
        assert!(f[0].fix.contains("--edit CRATE"));
        let f = build_failed(
            101,
            "warning: x\n\nerror[E0308]: mismatched types\nerror: could not compile `app`\n    Timing report saved to /t/x.html\n",
        );
        assert_eq!(
            (f.id, f.severity),
            ("build-failed", crate::finding::Severity::High)
        );
        assert_eq!(f.title, "The build failed (exit 101)");
        assert!(
            f.saw.ends_with("error: could not compile `app`"),
            "{}",
            f.saw
        );
    }

    #[test]
    fn report_ages() {
        assert_eq!(report_age(30.0, false), ("30 s ago".into(), None));
        assert_eq!(report_age(600.0, false).0, "10 min ago");
        assert!(
            report_age(600.0, true)
                .1
                .unwrap()
                .contains("older than Cargo.lock")
        );
        let (age, warn) = report_age(3.0 * 86_400.0, false);
        assert_eq!(age, "3 days ago");
        assert!(warn.unwrap().contains("more than a day old"));
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
        assert_eq!(k(" app-agent \"bin\""), Kind::Link);
        assert_eq!(k(" (test)"), Kind::Link);
        assert_eq!(k(" build-script (run)"), Kind::BuildScript);
    }

    /// A check unit's part after metadata is analysis, not codegen (#11).
    #[test]
    fn check_units_have_no_codegen() {
        let mut u = unit(1, "app-config", 0.0, 14.8, &[], &[]);
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

    /// kache's records for the real build, with the session's cargo
    /// commands (provisional fields, kunobi-ninja/kache#1968): a `cargo
    /// test` started while the report's units ran, so the joined records
    /// span two commands. Without the fields, nothing changes.
    #[test]
    fn kache_records_from_two_commands() {
        let t = Timings::parse_html(&fixture_file("timings-build.html")).unwrap();
        let mut v: serde_json::Value =
            serde_json::from_str(&fixture_file("kache-trace.json")).unwrap();
        let plain = wrapper::Records::from_report(&v);
        assert_eq!(plain.commands, []);
        let idx = wrapper::join_events(&t, &plain.events);
        let counts = wrapper::by_command(&plain.events, idx.iter().flatten().copied(), &[]);
        assert_eq!(counts, []);
        assert_eq!(wrapper::mixed_commands(&plain.commands, &counts), None);

        let session: serde_json::Value =
            serde_json::from_str(&fixture_file("kache-session.json")).unwrap();
        v["meta"] = session["meta"].clone();
        let r = wrapper::Records::from_report(&v);
        assert_eq!(r.events, plain.events);
        assert_eq!(r.commands.len(), 2);
        assert_eq!(
            r.commands[1].command_line(),
            "cargo test --workspace --no-run"
        );
        assert_eq!(r.commands[1].cwd.as_deref(), Some("/home/user/src/app"));
        let idx = wrapper::join_events(&t, &r.events);
        let counts = wrapper::by_command(&r.events, idx.iter().flatten().copied(), &r.commands);
        let joined = idx.iter().flatten().count();
        assert_eq!(counts.iter().map(|(_, n)| n).sum::<usize>(), joined);
        assert_eq!(counts[0].0, 0);
        assert_eq!(counts[1], (1, 8));
        let f = wrapper::mixed_commands(&r.commands, &counts).unwrap();
        assert_eq!(f.id, "kache-records-mixed-commands");
        assert_eq!(f.severity, crate::finding::Severity::Medium);
        assert!(
            f.saw.contains(&format!(
                "`cargo build --timings`: {} records, `cargo test --workspace --no-run`: 8 records",
                joined - 8
            )),
            "{}",
            f.saw
        );

        // One command, or records only from one: no warning.
        let one = wrapper::by_command(&r.events, idx.iter().flatten().copied(), &r.commands[..1]);
        assert_eq!(one, [(0, joined)]);
        assert_eq!(wrapper::mixed_commands(&r.commands[..1], &one), None);
    }

    /// The real build after a one-line app-core edit, joined with kache's
    /// records and compared with the check of the same edit (#12, #13).
    #[test]
    #[allow(clippy::many_single_char_names, reason = "t: timings, a: analysis")]
    fn real_build_with_kache_and_check() {
        let t = Timings::parse_html(&fixture_file("timings-build.html")).unwrap();
        let check = Timings::parse_html(&fixture_file("timings-check.html")).unwrap();
        assert_eq!(t.jobs, Some(Jobs { jobs: 16, ncpu: 32 }));
        assert!(check.is_check() && !t.is_check());
        let v: serde_json::Value = serde_json::from_str(&fixture_file("kache-trace.json")).unwrap();
        let events = wrapper::kache_events(&v);
        let runs = wrapper::join(&t, &events);
        let matched = runs.iter().flatten().count();
        let compiled = t.ran().filter(|u| u.kind() != Kind::BuildScript).count();
        assert_eq!(matched, compiled);
        // Everything that depends on app-core.
        let ws = Workspace {
            deps: t
                .ran()
                .filter(|u| u.name.starts_with("app-"))
                .map(|u| {
                    let deps = if u.name == "app-core" {
                        alloc::collections::BTreeSet::default()
                    } else {
                        ["app-core".to_owned()].into()
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
                first_build: None,
            },
            &runs,
        );
        let w = a.wrapper.as_ref().unwrap();
        assert_eq!(w.incremental.0, matched);
        assert_eq!(w.hits.0 + w.misses.0, 0);
        assert_eq!(a.path.len(), a.critical_path.len());
        assert!(a.path.iter().all(|p| p.cache == Some(Outcome::Incr)));
        assert!(a.critical_path.last().unwrap().unit.ends_with("\"bin\""));
        // The fixture's graph has every crate on app-core only, so the one
        // workspace edge on the path is the first step.
        let edges = path_edges(&a, &ws);
        assert_eq!(edges.len(), 1);
        assert_eq!(
            (edges[0].0.as_str(), edges[0].1.as_str()),
            ("app-config", "app-core")
        );
        assert!((edges[0].2.unwrap() - a.critical_path[0].duration).abs() < 1e-9);
        let r = a.rebuilt.as_ref().unwrap();
        assert_eq!(r.roots, ["app-core"]);
        assert!(r.crates >= 80);
        assert_eq!(r.first_build, None);
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
        json_shape(&s.data);
    }

    /// The `--json` data: structured, with seconds in `_s` fields.
    fn json_shape(d: &serde_json::Value) {
        assert_eq!(d["units_compiled"], 94);
        assert_eq!(d["units_fresh"], 0);
        assert!(d["wall_s"].as_f64().unwrap() > 150.0);
        assert_eq!(d["jobs"]["jobs"], 16);
        let p = d["critical_path"].as_array().unwrap();
        assert_eq!(p[0]["unit"], "app-core");
        assert_eq!(p[0]["crate"], "app-core");
        assert_eq!(p[0]["kind"], "lib");
        assert_eq!(p[0]["waited"], "full");
        assert_eq!(p[0]["cache"], "incr");
        assert_eq!(p[1]["waited"], "meta");
        let last = p.last().unwrap();
        assert_eq!(last["crate"], "app-agent-bin");
        assert_eq!(last["target"], "app-agent \"bin\"");
        assert!(last["start_s"].as_f64().unwrap() > 100.0);
        assert!(last["took_s"].as_f64().unwrap() > 1.0);
        let slow = &d["slowest_units"][0];
        assert_eq!(slow["crate"], "app-agent");
        assert!(slow["took_s"].as_f64().unwrap() > 50.0);
        assert_eq!(d["wrapper"]["incremental"]["units"], 94);
        assert_eq!(d["rebuilt"]["roots"][0], "app-core");
        assert!(d["slowdown"]["top"][0]["before_s"].is_f64());
        assert!(d["linked"]["slowest"][0]["took_s"].is_f64());
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

    /// A first build in a new target directory (#51): every workspace crate
    /// compiles, "starting from" the workspace's leaves.
    #[test]
    fn first_build_has_no_fan_out() {
        let units = |fresh: bool| {
            let mut u = vec![
                lib(0, "leaf-a", "", 0.0, 2.0, &[]),
                lib(1, "leaf-b", "", 0.0, 2.0, &[]),
            ];
            for n in 0..12 {
                u.push(lib(n + 2, &format!("app{n}"), "", 2.0, 3.0, &[]));
            }
            if fresh {
                u.push(lib(20, "other", "", 0.0, 0.0, &[]));
            }
            Timings {
                units: u,
                ..Timings::default()
            }
        };
        let mut deps: alloc::collections::BTreeMap<String, BTreeSet<String>> = (0..12)
            .map(|n| (format!("app{n}"), ["leaf-a".to_owned()].into()))
            .collect();
        deps.insert("leaf-a".into(), BTreeSet::new());
        deps.insert("leaf-b".into(), BTreeSet::new());
        deps.insert("other".into(), BTreeSet::new());
        let ws = Workspace { deps };
        let run = |t: &Timings, first_build| {
            analyze_with(
                t,
                Extra {
                    baseline: None,
                    workspace: Some(&ws),
                    first_build,
                },
                &[],
            )
        };
        let a = run(&units(false), None);
        let r = a.rebuilt.as_ref().unwrap();
        assert_eq!(r.roots, ["leaf-a", "leaf-b"]);
        assert_eq!(r.first_build, Some(FirstBuild::FromLeaves));
        let f = findings(&a);
        assert!(f.iter().all(|f| f.id != "edit-rebuilt-many"));
        let first = f.iter().find(|f| f.id == "first-build").unwrap();
        assert_eq!(first.severity, crate::finding::Severity::Info);
        assert!(
            first
                .saw
                .contains("starting from 2 crates with no workspace dependencies (leaf-a, leaf-b)")
        );
        let s = section(&a, "x");
        assert!(
            s.facts
                .iter()
                .any(|f| f.value == "14 workspace crates: a first build in this target directory")
        );
        // A workspace crate was fresh: something was edited.
        assert!(ids(&run(&units(true), None)).contains(&"edit-rebuilt-many"));
        // `--edit` says it was an edit; no fingerprints before says it wasn't.
        assert!(ids(&run(&units(false), Some(false))).contains(&"edit-rebuilt-many"));
        let a = run(&units(true), Some(true));
        assert_eq!(
            a.rebuilt.as_ref().unwrap().first_build,
            Some(FirstBuild::NoFingerprints)
        );
        assert!(ids(&a).contains(&"first-build"));
    }

    /// The 566 s build from #13: a chain of libraries 10x slower than their
    /// check, then six tool binaries of ~2 minutes each.
    fn slow_build() -> (Timings, Timings) {
        let mut units = Vec::new();
        let mut checks = Vec::new();
        for n in 0..6_u8 {
            let name = format!("app-lib{n}");
            let start = 60.0 * f64::from(n);
            let i = usize::from(n);
            units.push(lib(i, &name, "", start, 60.0, &[i + 1]));
            checks.push(lib(i, &name, " (check)", 6.0 * f64::from(n), 6.0, &[]));
        }
        for b in 0..6_u8 {
            let name = format!("app-tool-{b}");
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
                first_build: None,
            },
            &[],
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
        let units = vec![
            lib(0, "a", "", 0.0, 10.0, &[1]),
            lib(1, "b", "", 25.0, 20.0, &[]),
        ];
        let runs = [
            None,
            Some(WrapperRun {
                outcome: Outcome::Miss,
                compile: 8.0,
                wait: 12.0,
                elapsed: 20.0,
            }),
        ];
        let a = analyze_with(
            &Timings {
                units,
                ..Timings::default()
            },
            Extra::default(),
            &runs,
        );
        assert!((a.path_gaps - 15.0).abs() < 1e-9);
        assert!((a.path_waits - 12.0).abs() < 1e-9);
        let found = ids(&a);
        assert!(found.contains(&"critical-path-waiting"), "{found:?}");
        assert!(found.contains(&"wrapper-wait"), "{found:?}");
    }
}
