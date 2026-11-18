//! Findings for a timings analysis.

use super::FirstBuild;
use super::{Analysis, Kind, UnitTime};
use crate::finding::{Finding, Severity};
use crate::fmt::{pct, secs};

fn package(unit: &str) -> &str {
    unit.split(' ').next().unwrap_or(unit)
}

/// Whether `pkg` is a workspace member, when the workspace is known.
fn is_member(a: &Analysis, pkg: &str) -> Option<bool> {
    let want = pkg.replace('_', "-");
    a.members
        .as_ref()
        .map(|m| m.iter().any(|n| n.replace('_', "-") == want))
}

/// The `check-cost` target flags that profile the build of `unit` (a
/// [`label`](cargo_timings_parse::Unit::label)): `--test NAME` for an
/// integration test, `--tests` for a unit-test build, `--bin NAME` for a
/// binary not named after its package.
fn target_flags(unit: &str) -> String {
    let Some(inner) = unit.split_once(" (").and_then(|(_, r)| r.strip_suffix(')')) else {
        return String::new();
    };
    let mut parts = inner.split(", ");
    let target = parts.next().unwrap_or_default();
    let mode = parts.next().unwrap_or(target);
    let mut flags = match target.split_once(' ') {
        Some((kind @ ("test" | "bench" | "example" | "bin"), name)) => format!(" --{kind} {name}"),
        _ => String::new(),
    };
    let unit_test =
        mode.contains("test") && !target.starts_with("test ") && !target.starts_with("bench ");
    if unit_test {
        flags.push_str(" --tests");
    }
    flags
}

/// How to find out what's slow inside `pkg`. `check-cost -p` profiles
/// workspace members only; for a dependency, `then` (advice that only
/// makes sense for your own crate) is left out too.
fn look_inside(a: &Analysis, unit: &str, then: &str) -> String {
    let pkg = package(unit);
    let flags = target_flags(unit);
    let dep = format!(
        "{pkg} is a dependency, so you can't change its code: run `cargo faster deps` to see the features you enable on it and duplicate versions, turn off default features you don't use, check whether a newer version builds faster, and whether you need it at all."
    );
    match is_member(a, pkg) {
        Some(true) => {
            format!(
                "Find out what's slow inside it with `cargo faster check-cost -p {pkg}{flags}`.{then}"
            )
        }
        Some(false) => dep,
        None => format!(
            "If {pkg} is a workspace crate, find out what's slow inside it with `cargo faster check-cost -p {pkg}{flags}`.{then} If it's a dependency, run `cargo faster deps` to see the features you enable on it and duplicate versions, turn off default features you don't use, and check whether a newer version builds faster."
        ),
    }
}

/// The timed build failed: the report covers only the units it compiled
/// before it stopped. `stderr` is the build's; its last lines go in `saw`.
#[must_use]
pub fn build_failed(code: i32, stderr: &str) -> Finding {
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.contains("Timing report saved to"))
        .collect();
    let tail = lines[lines.len().saturating_sub(5)..].join("\n");
    Finding::new("build-failed", Severity::High, format!("The build failed (exit {code})"), "the-build-timeline")
        .saw(if tail.is_empty() {
            format!("cargo exited with status {code}.")
        } else {
            format!("cargo exited with status {code}. The end of its output:\n{tail}")
        })
        .why("Cargo stops starting units after an error, so the times, the critical path and the findings below cover only part of the build.")
        .fix("Fix the build error and run it again. Pass --allow-failed-build to read the partial report and exit 0.")
}

/// Findings for an analysis.
#[must_use]
pub fn findings(a: &Analysis) -> Vec<Finding> {
    let mut out = Vec::new();
    if a.wall <= 0.0 {
        out.push(
            Finding::new("nothing-rebuilt", Severity::Info, "Nothing was rebuilt", "the-build-timeline")
                .saw(match a.total_units {
                    0 => "The report lists no units: cargo had nothing to do, so there is no build to time.".to_owned(),
                    1 => "Its one unit was fresh: cargo had nothing to compile, so there is no build to time.".to_owned(),
                    n => format!("All {n} units were fresh: cargo had nothing to compile, so there is no build to time."),
                })
                .why("A report only says where the time goes for the units it compiled.")
                .fix("Use `cargo faster timings --run --edit CRATE` to time the build after an edit to CRATE, or edit something yourself and run `cargo faster timings --run`."),
        );
        return out;
    }
    shape(a, &mut out);
    per_unit(a, &mut out);
    links(a, &mut out);
    contention(a, &mut out);
    waits(a, &mut out);
    rebuilt(a, &mut out);
    if !out.iter().any(|f| f.severity >= Severity::Medium)
        && let Some(f) = slow_build(a)
    {
        out.push(f);
    }
    out
}

/// Serial crates, a dominated critical path, low parallelism.
fn shape(a: &Analysis, out: &mut Vec<Finding>) {
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
                .fix(look_inside(a, &s.unit, " If it's just big, split it (see `cargo faster rdeps`) so the pieces build in parallel and edits rebuild less.")),
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
                .fix(look_inside(a, &top.unit, "")),
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
}

/// Long codegen in built units, long analysis after metadata in check
/// units, slow build scripts.
fn per_unit(a: &Analysis, out: &mut Vec<Finding>) {
    if let Some(UnitTime {
        unit: u,
        duration: d,
        part: Some(c),
        ..
    }) = a.long_codegen.first()
    {
        out.push(
            Finding::new("long-codegen", Severity::Medium, format!("Code generation takes {} in {u}", secs(*c)), "generic-code-and-monomorphization")
                .saw(format!("{u} spent {} of {} in codegen.", secs(*c), secs(*d)))
                .why("Codegen time grows with the amount of monomorphized code: every generic function instantiated with each type it's used with.")
                .fix("Run `cargo llvm-lines -p <crate>` (on its own checkout, not with a shared cache) to find what's instantiated most, and make the bulk of those functions non-generic."),
        );
    }
    if let Some(UnitTime {
        unit: u,
        duration: d,
        part: Some(after),
        ..
    }) = a.post_metadata.first()
    {
        out.push(
            Finding::new("post-metadata-analysis", Severity::Low, format!("{u} spends {} after its metadata", secs(*after)), "check-versus-build")
                .saw(format!("{u} took {}, {} of it after writing its metadata. A check unit has no code generation: this is analysis after metadata (borrow checking, MIR checks) and, with incremental, saving the dependency graph.", secs(*d), secs(*after)))
                .why("Dependents can start once the metadata is written, so this part is off their path, but it still holds a job slot and ends the check of anything that has to wait for the whole crate.")
                .fix(look_inside(a, u, " A large incremental save shrinks when the crate is split.")),
        );
    }
    if let Some(UnitTime {
        unit: u,
        duration: d,
        ..
    }) = a.build_scripts.first()
    {
        out.push(
            Finding::new("slow-build-script", Severity::Low, format!("Build script {u} takes {}", secs(*d)), "dependencies-and-features")
                .saw(format!("Slow build scripts: {}.", a.build_scripts.iter().take(4).map(|u| format!("{} {}", u.unit, secs(u.duration))).collect::<Vec<_>>().join(", ")))
                .why("Build scripts run before their crate compiles, often compiling C code (ring, openssl, zstd) or generating code (protobuf). They rerun when their inputs change.")
                .fix("Check `cargo:rerun-if-changed` lines so it doesn't rerun needlessly, prefer pure-Rust alternatives, or link a system library (`*-sys` crates often support `pkg-config`)."),
        );
    }
}

/// Binaries and tests taking a big share of the build.
fn links(a: &Analysis, out: &mut Vec<Finding>) {
    let l = &a.linked;
    let busy: f64 = a.kinds.iter().map(|k| k.busy).sum();
    let slowest = l.slowest.first().map_or(0.0, |s| s.took);
    let tail_heavy = l.tail >= 10.0 && pct(l.tail, a.wall) >= 15.0;
    let busy_heavy = l.busy >= 30.0 && pct(l.busy, busy) >= 25.0;
    if l.units == 0 || !(tail_heavy || busy_heavy) {
        return;
    }
    let severity = if pct(l.tail, a.wall) >= 25.0 || pct(slowest, a.wall) >= 20.0 {
        Severity::High
    } else {
        Severity::Medium
    };
    let list: Vec<String> = l
        .slowest
        .iter()
        .map(|s| format!("{} {}", s.unit, secs(s.took)))
        .collect();
    out.push(
        Finding::new("slow-links", severity, format!("{} binaries and tests took {} to build and link", l.units, secs(l.busy)), "linking")
            .saw(format!("This build compiled and linked {} binaries and tests ({} together, {:.0}% of all compile time); the slowest: {}. For the last {} of the {} build only they were running.", l.units, secs(l.busy), pct(l.busy, busy), list.join(", "), secs(l.tail), secs(a.wall)))
            .why("Every binary and test target that depends on an edited crate is code-generated and linked again, and none of that can start before the libraries are done. Many small binaries each pay for linking the whole dependency tree.")
            .fix("Check the linker and debug info with `cargo faster setup` (mold or wild, `debug = \"line-tables-only\"`, no debug info for dependencies). Merge small tool binaries into one, and integration tests into one test binary per crate. Build only the binaries you need while iterating (`--bin`)."),
    );
}

/// Units much slower than in the baseline report.
fn contention(a: &Analysis, out: &mut Vec<Finding>) {
    let Some(s) = &a.slowdown else { return };
    if s.compared < 3 || s.median < 3.0 || s.excess < 30.0 {
        return;
    }
    let what = if s.baseline_check {
        "the same crates' `cargo check` (up to metadata, which is all a check does)"
    } else {
        "the earlier report"
    };
    let list: Vec<String> = s
        .top
        .iter()
        .map(|u| {
            format!(
                "{} {} vs {}",
                u.unit,
                secs(u.took),
                secs(u.before.unwrap_or(0.0))
            )
        })
        .collect();
    let jobs = a.jobs.map_or(String::new(), |j| {
        format!(" Cargo ran with jobs={} on {} CPUs.", j.jobs, j.ncpu)
    });
    out.push(
        Finding::new("contention", if pct(s.excess, a.wall * a.parallelism) >= 25.0 { Severity::High } else { Severity::Medium }, format!("Units ran {:.1}x slower than in {}", s.median, if s.baseline_check { "the check" } else { "the earlier build" }), "the-machine")
            .saw(format!("Compared with {what}, the {} units compiled in both took {:.1}x as long (median), {} more in total: {}.{jobs}", s.compared, s.median, secs(s.excess), list.join("; ")))
            .why("When the same crate takes many times longer than its check, it was competing for CPU, memory or disk: other builds, other worktrees, test runs, or too many jobs for the memory. A cache wrapper hiding the metadata split also counts codegen here, but not 3x of it.")
            .fix("Look at what else ran: `cargo faster machine` shows competing cargo and rustc processes and memory. Don't run two workspace builds at once on one machine, lower `build.jobs` if memory is short, and compare again with an idle machine."),
    );
}

/// Waiting on the critical path and in the cache wrapper.
fn waits(a: &Analysis, out: &mut Vec<Finding>) {
    let waiting = a.path_gaps + a.path_waits;
    if waiting >= 10.0 && pct(waiting, a.wall) >= 20.0 {
        let jobs = a
            .jobs
            .map_or(String::new(), |j| format!(" (jobs={})", j.jobs));
        out.push(
            Finding::new("critical-path-waiting", if pct(waiting, a.wall) >= 40.0 { Severity::High } else { Severity::Medium }, format!("{} of the critical path is waiting", secs(waiting)), "compiles-cache-hits-and-waits")
                .saw(format!("Of the {} wall time, the critical path spent {} waiting to start after what it needed was ready (no free job slot{jobs}) and {} inside the cache wrapper waiting for a permit or another build's compile.", secs(a.wall), secs(a.path_gaps), secs(a.path_waits)))
                .why("A step on the critical path that waits pushes back everything after it, even when its own compile is fast.")
                .fix("If the wait is for job slots, other units were holding them: look at what ran alongside in the report. If it's in the wrapper, another build was using its permits (`kache stats` shows the permit wait). Avoid overlapping builds of the same workspace."),
        );
    }
    if let Some(w) = &a.wrapper {
        let busy: f64 = a.kinds.iter().map(|k| k.busy).sum();
        if w.wait >= 10.0 && pct(w.wait, busy) >= 10.0 {
            out.push(
                Finding::new("wrapper-wait", Severity::Medium, format!("Units waited {} for the cache wrapper", secs(w.wait)), "compiles-cache-hits-and-waits")
                    .saw(format!("The wrapper reports {} of permit and in-flight waiting across {} units, {:.0}% of their {} total.", secs(w.wait), w.matched, pct(w.wait, busy), secs(busy)))
                    .why("kache limits how many compiles run at once across all builds on the machine, and makes a build wait for another's compile of the same crate. Each wait holds one of cargo's job slots too.")
                    .fix("Run one workspace build at a time, or raise kache's permit limit if the machine has CPU and memory to spare."),
            );
        }
    }
}

/// Many workspace crates recompiled.
fn rebuilt(a: &Analysis, out: &mut Vec<Finding>) {
    let Some(r) = &a.rebuilt else { return };
    if r.crates < 10 {
        return;
    }
    if let Some(why) = r.first_build {
        let leaves = if r.roots.len() > 4 {
            format!("{} and {} more", r.roots[..4].join(", "), r.roots.len() - 4)
        } else {
            r.roots.join(", ")
        };
        out.push(
            Finding::new("first-build", Severity::Info, "First build in this target directory: no fan-out to report", "worktrees-and-cold-target-directories")
                .saw(match why {
                    FirstBuild::NoFingerprints => format!("The target directory had no fingerprints for any workspace crate before this build, so all {} workspace crates it compiled ({} of units) were built for the first time, whatever came from a cache.", r.crates, secs(r.busy)),
                    _ => format!("All {} workspace crates in the report compiled ({} of units) and none was fresh, starting from {} crates with no workspace dependencies ({leaves}): that's a first build in this target directory (or after `cargo clean`), not an edit.", r.crates, secs(r.busy), r.roots.len()),
                })
                .why("A first build compiles every crate, so which ones it started from says nothing about what an edit rebuilds.")
                .fix("To see what an edit to one crate costs, run `cargo faster timings --run --edit CRATE`: it brings the tree up to date, makes the edit and times the build. To make first builds cheaper, share a compile cache between target directories (`cargo faster cache`)."),
        );
        return;
    }
    let root = r.roots.first().map_or("<crate>", String::as_str);
    let roots = if r.roots.len() > 4 {
        format!("{} and {} more", r.roots[..4].join(", "), r.roots.len() - 4)
    } else {
        r.roots.join(", ")
    };
    out.push(
        Finding::new("edit-rebuilt-many", if r.crates >= 40 { Severity::High } else { Severity::Medium }, format!("This build recompiled {} workspace crates", r.crates), "rebuild-fan-out")
            .saw(format!("{} workspace crates recompiled ({} of units): {roots} and {} other workspace crates that depend on {}. An edit there rebuilds every crate that depends on it.", r.crates, secs(r.busy), r.crates.saturating_sub(r.roots.len()), if r.roots.len() == 1 { "it" } else { "them" }))
            .why("Cargo rebuilds every dependent of a changed crate. A crate low in the graph that many others use turns each edit into a rebuild of most of the workspace.")
            .fix(format!("See what depends on it and what's worth splitting out: `cargo faster rdeps {root}`. Move the often-edited parts into a crate fewer crates depend on, or the stable parts others need into a small crate below it.")),
    );
}

/// The safety net: a slow build gets a finding saying where its time went
/// even when nothing else fired.
fn slow_build(a: &Analysis) -> Option<Finding> {
    let vs_check = a
        .slowdown
        .as_ref()
        .filter(|s| s.baseline_check && s.baseline_wall > 0.0)
        .map(|s| (a.wall / s.baseline_wall, s.baseline_wall));
    if a.wall < 60.0 && vs_check.is_none_or(|(r, _)| r < 2.0 || a.wall < 20.0) {
        return None;
    }
    let busy: f64 = a.kinds.iter().map(|k| k.busy).sum();
    let parts: Vec<String> = a
        .kinds
        .iter()
        .map(|k| {
            format!(
                "{} {} ({} units, {} on the critical path)",
                k.kind.label(),
                secs(k.busy),
                k.units,
                secs(k.on_path)
            )
        })
        .collect();
    let check = vs_check.map_or(String::new(), |(r, w)| {
        format!(
            " The check of the same edit took {} ({r:.1}x less).",
            secs(w)
        )
    });
    let top = a.kinds.first().map(|k| k.kind);
    let fix = match top {
        Some(Kind::Link) => {
            "Most of it is binaries and tests: see the linking section of the guide."
        }
        Some(Kind::Lib) => {
            "Most of it is code generation of libraries: check debug info, codegen-units and dependency opt-level with `cargo faster setup`, and profile the slowest crate on the critical path."
        }
        _ => {
            "Profile the slowest workspace crate on the critical path with `cargo faster check-cost -p CRATE` (for a dependency, see `cargo faster deps`), and see `cargo faster rdeps` for how many crates an edit rebuilds."
        }
    };
    Some(
        Finding::new("slow-build", Severity::Medium, format!("{} build: where the time went", secs(a.wall)), "compiles-cache-hits-and-waits")
            .saw(format!("{} wall, {} of compiling at {:.1} units at a time: {}.{check}", secs(a.wall), secs(busy), a.parallelism, parts.join("; ")))
            .why("No single cause stood out, but the time is spent somewhere: the split above says which part of the build to look at first.")
            .fix(fix),
    )
}

#[cfg(test)]
mod tests {
    use super::target_flags;

    #[test]
    fn check_cost_target_flags() {
        assert_eq!(target_flags("app"), "");
        assert_eq!(target_flags("app (check)"), "");
        assert_eq!(target_flags("app (lib, check)"), "");
        assert_eq!(target_flags("app (bin, check-test)"), " --tests");
        assert_eq!(target_flags("app (lib, test)"), " --tests");
        assert_eq!(target_flags("app (test it, check-test)"), " --test it");
        assert_eq!(target_flags("app (bin tool, check)"), " --bin tool");
        assert_eq!(
            target_flags("app (bin tool, check-test)"),
            " --bin tool --tests"
        );
    }
}
