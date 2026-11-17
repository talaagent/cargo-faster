//! `cargo faster`: find out why your Rust build is slow and how to make it
//! faster.

use bpaf::Bpaf;
use bpaf_kit::log::LogArgs;
use bpaf_kit::{Cli, cmd};
use cargo_faster::context::{Context, build, cargo_bin};
use cargo_faster::error::Error;
use cargo_faster::error::UnknownName;
use cargo_faster::finding::{Fact, Report, Section, Severity};
use cargo_faster::{
    cache, checkcost, deps, graph, machine, profile, setup, testrun, tests, timings,
};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Instant, SystemTime};

/// The global options, for the footer of each subcommand's help.
macro_rules! global_help {
    () => {
        "Global options (before or after the subcommand): --json, --manifest-path PATH, --fail-on LEVEL, --dry-run, -v, -q. See `cargo faster --help`."
    };
}

/// Find out why your Rust build is slow and how to make it faster
#[derive(Debug, Clone, Bpaf, Cli)]
#[bpaf(
    options("faster"),
    header(
        concat!(
            "Every finding says what we saw, why it matters, the fix, and a link into the guide:\nhttps://github.com/talaagent/cargo-faster/blob/v",
            env!("CARGO_PKG_VERSION"),
            "/docs/guide.md"
        )
    ),
    footer(
        "Exit status: 0 on success, findings included; 1 on an error, a failed build, or a finding at or above --fail-on; 2 on a usage error."
    )
)]
#[cli(log = "warn")]
struct Opts {
    /// Print the report as JSON
    json: bool,
    /// Exit 1 when any finding is at least this severe: high, medium or low (for CI)
    #[bpaf(argument("LEVEL"))]
    fail_on: Option<FailOn>,
    /// Path to the workspace Cargo.toml
    #[bpaf(argument("PATH"))]
    manifest_path: Option<PathBuf>,
    /// Print the builds `timings --run`, `tests --run` and `check-cost` would start, without
    /// starting them
    dry_run: bool,
    #[bpaf(external(bpaf_kit::log::log_args))]
    #[cli(log_args)]
    log: LogArgs,
    #[bpaf(external(command), fallback(Command::Report))]
    cmd: Command,
}

// Doc comments here are the help text, where backticks would show.
#[expect(clippy::doc_markdown, reason = "doc comments are --help text")]
#[derive(Debug, Clone, Bpaf)]
enum Command {
    /// A quick overall report: setup, cache, machine, deps, tests, crates (the default)
    #[bpaf(command, footer(global_help!()))]
    Report,
    /// Audit profiles, linker, debuginfo, codegen units, jobs and rustflags
    #[bpaf(command, footer(global_help!()))]
    Setup,
    /// Check the compiler cache (sccache, kache), incremental and cold worktrees
    #[bpaf(command, footer(global_help!()))]
    Cache,
    /// Check load, concurrent builds, memory and disk
    #[bpaf(command, footer(global_help!()))]
    Machine,
    /// Check heavy features, duplicate versions and proc macros
    #[bpaf(command, footer(global_help!()))]
    Deps,
    /// Check nextest, target runners, thread limits and database-backed tests
    #[bpaf(
        command,
        footer(
            concat!(
                "With --run: times `cargo nextest run --workspace --no-run --timings`, then runs the tests with NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 and `--message-format libtest-json-plus`, and reports build vs run time, the slowest tests and binaries, and how busy the test threads were.",
                "\n",
                global_help!()
            )
        )
    )]
    Tests {
        /// Build and run the tests with nextest and report where the time goes
        run: bool,
        #[bpaf(external(nice))]
        nice: Nice,
        /// Extra `cargo nextest run` arguments, after `--` (e.g. -- --profile ci)
        #[bpaf(positional("NEXTEST_ARGS"), strict, many)]
        nextest_args: Vec<String>,
    },
    /// What rebuilds when a crate changes, and which crates are worth splitting
    #[bpaf(command, footer(global_help!()))]
    Rdeps {
        /// Only this workspace crate
        #[bpaf(short('p'), long("package"), argument("CRATE"))]
        package: Option<String>,
        /// Only this workspace crate (the same as -p)
        #[bpaf(positional("CRATE"))]
        krate: Option<String>,
    },
    /// Read a --timings report: critical path, where the time went, cache hits and waits, slow links, contention
    #[bpaf(
        command,
        footer(
            concat!(
                "--run times `cargo check --workspace --timings`; --cmd picks the build: build (`cargo build`), clippy, or test (`cargo nextest run --no-run`, or `cargo test --no-run` without nextest). Extra cargo arguments go after `--`. --edit CRATE measures what an edit to CRATE costs: it runs the build once to bring the tree up to date, appends a probe function to CRATE's lib.rs (a new public one; with --body, a private one whose body changes), times the build, and restores the file. Cargo always gets --workspace, never -p.",
                "\n",
                global_help!()
            )
        )
    )]
    Timings {
        /// Run the build with --timings first, then read its report
        run: bool,
        /// The build --run and --edit time: check (the default), build, clippy or test
        #[bpaf(argument("CMD"))]
        cmd: Option<timings::run::Build>,
        /// Measure an edit to this workspace crate: warm up, add a probe, time the build, restore
        #[bpaf(argument("CRATE"))]
        edit: Option<String>,
        /// With --edit, change a private function's body instead of the crate's API
        body: bool,
        /// Exit 0 when the timed build fails (the report covers what it compiled); without it, exit 3
        allow_failed_build: bool,
        #[bpaf(external(nice))]
        nice: Nice,
        /// An earlier report to compare unit times with (default for a build: the newest earlier check report)
        #[bpaf(argument("FILE"))]
        baseline: Option<PathBuf>,
        /// A cargo-timing.html report (default: the newest in target/cargo-timings)
        #[bpaf(positional("FILE"), non_strict, optional)]
        file: Option<PathBuf>,
        /// Extra cargo arguments for --run and --edit, after `--` (e.g. -- --profile gate)
        #[bpaf(positional("CARGO_ARGS"), strict, many)]
        cargo_args: Vec<String>,
    },
    /// Self-profile type checking of one crate: top queries, trait solving by type and body, proc macros
    #[bpaf(
        command,
        footer(
            concat!(
                "Uses RUSTC_BOOTSTRAP=1 so a stable rustc accepts -Zself-profile. `-p` picks the package and --lib/--bin/--test/--example/--bench the target; the rustc command is captured from `cargo check --workspace -vv` with the matching filter (--lib, --bin NAME, ...; with --tests also --profile test), which first checks the selected targets in every member and what they depend on. Cargo always gets --workspace, never -p, so caches keyed on the package set (kache) stay warm. Target flags in CARGO_ARGS (-- --tests) replace the filter; the lib's --test build is then profiled when they include --tests.",
                "\n",
                global_help!()
            )
        )
    )]
    CheckCost {
        /// The workspace member to profile (default: the root package, else the biggest)
        #[bpaf(short('p'), long, argument("NAME"))]
        package: Option<String>,
        #[bpaf(external(target_sel), optional)]
        target: Option<TargetSel>,
        /// Profile the --test (unit test, cfg(test)) build of the library, binary or example
        tests: bool,
        /// Write the profile into DIR and keep it
        #[bpaf(argument("DIR"))]
        keep: Option<PathBuf>,
        #[bpaf(external(nice))]
        nice: Nice,
        /// Analyze an existing .mm_profdata file instead of building
        #[bpaf(argument("FILE"))]
        profdata: Option<PathBuf>,
        /// Extra `cargo check` arguments, after `--` (e.g. -- --features x)
        #[bpaf(positional("CARGO_ARGS"), strict, many)]
        cargo_args: Vec<String>,
    },
}

// Which target `check-cost` profiles. No doc comment: bpaf would make it a
// help section of its own.
#[derive(Debug, Clone, Bpaf)]
enum TargetSel {
    /// Profile the library (the default; a package without one: its first binary)
    #[bpaf(long("lib"))]
    Lib,
    Bin {
        /// Profile this binary
        #[bpaf(argument("NAME"))]
        bin: String,
    },
    Test {
        /// Profile this integration test (tests/NAME.rs; always built with --test)
        #[bpaf(argument("NAME"))]
        test: String,
    },
    Example {
        /// Profile this example
        #[bpaf(argument("NAME"))]
        example: String,
    },
    Bench {
        /// Profile this benchmark (always built with --test)
        #[bpaf(argument("NAME"))]
        bench: String,
    },
}

/// `--fail-on LEVEL`: the least severe finding that fails the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FailOn(Severity);

impl core::str::FromStr for FailOn {
    type Err = bpaf_kit::suggest::UnknownValue;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const LEVELS: &[(&str, Severity)] = &[
            ("high", Severity::High),
            ("medium", Severity::Medium),
            ("low", Severity::Low),
        ];
        bpaf_kit::suggest::parse_choice("severity", s, LEVELS).map(Self)
    }
}

// `--nice N`, for the subcommands that build. No doc comment: bpaf would
// make it a help section of its own.
#[derive(Debug, Clone, Copy, Bpaf)]
struct Nice {
    /// Run builds under `nice -n N` (0 to not renice); the box may be shared
    #[bpaf(long("nice"), argument("N"), fallback(10), display_fallback)]
    n: i32,
}

fn main() -> ExitCode {
    let opts = match parse() {
        Ok(o) => o,
        Err(e) => return bpaf_kit::report::report_as("cargo faster", &e),
    };
    if opts.dry_run {
        cmd::set_dry_run(true);
    }
    match run(&opts) {
        Ok(report) => {
            let text = if opts.json {
                report.to_json()
            } else {
                report.render_text(bpaf_kit::color_enabled(&std::io::stdout()))
            };
            let printed = print(&text);
            if printed != ExitCode::SUCCESS {
                return printed;
            }
            if let Some(f) = report.findings().iter().find(|f| f.id == "build-failed")
                && !matches!(
                    opts.cmd,
                    Command::Timings {
                        allow_failed_build: true,
                        ..
                    }
                )
            {
                let e = Error::BuildFailed(format!(
                    "{}; the report above covers only what it compiled (pass --allow-failed-build to exit 0)",
                    f.title.to_lowercase()
                ));
                return bpaf_kit::report::report_as("cargo faster", &e);
            }
            match opts.fail_on {
                Some(FailOn(level)) if report.findings().iter().any(|f| f.severity >= level) => {
                    eprintln!(
                        "cargo faster: failing: a finding is {} or worse (--fail-on {})",
                        level.name(),
                        level.name()
                    );
                    ExitCode::FAILURE
                }
                _ => ExitCode::SUCCESS,
            }
        }
        Err(Error::DryRun(msg)) => {
            eprintln!("cargo faster: {msg}");
            ExitCode::SUCCESS
        }
        Err(e) => bpaf_kit::report::report_as("cargo faster", &e),
    }
}

/// Parse the command line. Help says `cargo faster`, and a usage error is
/// an `error[usage]` with exit status 2, like every other error but for
/// the status. `--man` and `--version` are left to bpaf-kit.
fn parse() -> Result<Opts, Error> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let first = args.iter().find(|a| *a != "faster");
    let meta = first.is_some_and(|a| a == "--man" || a == "--version" || a == "-V");
    if !meta {
        let named = bpaf::Args::current_args().set_name("cargo faster");
        match Opts::parser().run_inner(named) {
            Ok(_) => {}
            Err(bpaf::ParseFailure::Stderr(doc)) => {
                return Err(Error::Usage(doc.monochrome(true)));
            }
            Err(f) => {
                f.print_message(100);
                std::process::exit(0);
            }
        }
    }
    let opts = Opts::cli();
    check_usage(&opts.cmd)?;
    Ok(opts)
}

/// Combinations the parser accepts but that make no sense.
fn check_usage(cmd: &Command) -> Result<(), Error> {
    match cmd {
        Command::Timings {
            body: true,
            edit: None,
            ..
        } => Err(Error::Usage(
            "--body changes what `--edit CRATE` appends; it needs --edit".into(),
        )),
        Command::Rdeps {
            package: Some(p),
            krate: Some(k),
        } if p != k => Err(Error::Usage(format!(
            "rdeps takes one crate: -p {p} or {k}, not both"
        ))),
        _ => Ok(()),
    }
}

fn print(s: &str) -> ExitCode {
    let mut out = std::io::stdout().lock();
    match out.write_all(s.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

#[expect(clippy::too_many_lines, reason = "one match arm per subcommand")]
fn run(opts: &Opts) -> Result<Report, Error> {
    // An explicit profile file needs no workspace.
    if let Command::CheckCost {
        profdata: Some(file),
        ..
    } = &opts.cmd
    {
        return Ok(Report {
            workspace: None,
            sections: vec![profile_section(file, None, &[])?],
            skipped: Vec::new(),
        });
    }
    // So does an explicit timings report.
    if let Command::Timings {
        file: Some(file),
        run: false,
        cmd: None,
        edit: None,
        baseline,
        ..
    } = &opts.cmd
        && Context::gather(opts.manifest_path.as_deref()).is_err()
    {
        return Ok(Report {
            workspace: None,
            sections: vec![timings_file_section(None, file, baseline.as_deref())?],
            skipped: Vec::new(),
        });
    }
    let ctx = Context::gather(opts.manifest_path.as_deref())?;
    let mut report = Report {
        workspace: Some(ctx.root().display().to_string()),
        ..Report::default()
    };
    let m = ctx.metadata.clone().unwrap_or_default();
    match &opts.cmd {
        Command::Report => full_report(&ctx, &m, &mut report),
        Command::Setup => report
            .sections
            .push(setup::check(&ctx, &setup::Tools::detect(&ctx))),
        Command::Cache => report
            .sections
            .push(cache::check(&ctx, &cache::Inputs::gather(&ctx))),
        Command::Machine => report
            .sections
            .push(machine::check(&machine::Inputs::gather(
                &ctx,
                machine::cache_dir(&ctx),
            ))),
        Command::Deps => report
            .sections
            .push(deps::section(&deps::analyze_with_sources(&m))),
        Command::Tests {
            run,
            nice,
            nextest_args,
        } => tests_sections(&ctx, *run, nice.n, nextest_args, &mut report)?,
        Command::Rdeps { package, krate } => report
            .sections
            .push(rdeps_section(&m, package.as_deref().or(krate.as_deref()))?),
        Command::Timings {
            file,
            run,
            cmd,
            edit,
            body,
            nice,
            baseline,
            cargo_args,
            allow_failed_build: _,
        } => {
            let run =
                (*run || cmd.is_some() || edit.is_some() || !cargo_args.is_empty()).then(|| {
                    TimingsRun {
                        build: cmd.unwrap_or_default(),
                        edit: edit.clone(),
                        probe: if *body {
                            timings::run::Probe::Body
                        } else {
                            timings::run::Probe::Api
                        },
                        nice: nice.n,
                        cargo_args: cargo_args.clone(),
                    }
                });
            report.sections.push(timings_section(
                &ctx,
                file.as_deref(),
                run.as_ref(),
                baseline.as_deref(),
            )?);
        }
        Command::CheckCost {
            package,
            target,
            tests,
            keep,
            nice,
            profdata: _,
            cargo_args,
        } => {
            let o = checkcost::Options {
                package: package.clone(),
                target: match target.clone() {
                    None => checkcost::Select::Auto,
                    Some(TargetSel::Lib) => checkcost::Select::Lib,
                    Some(TargetSel::Bin { bin }) => checkcost::Select::Bin(bin),
                    Some(TargetSel::Test { test }) => checkcost::Select::Test(test),
                    Some(TargetSel::Example { example }) => checkcost::Select::Example(example),
                    Some(TargetSel::Bench { bench }) => checkcost::Select::Bench(bench),
                },
                tests: *tests,
                cargo_args: cargo_args.clone(),
                nice: nice.n,
                keep: keep.clone(),
            };
            report.sections.push(check_cost_section(&ctx, &o)?);
        }
    }
    Ok(report)
}

fn rdeps_section(
    m: &cargo_faster::context::Metadata,
    krate: Option<&str>,
) -> Result<Section, Error> {
    let crates = graph::gather(m);
    if let Some(k) = krate
        && !crates.iter().any(|c| graph::is_named(c, k))
    {
        return Err(
            UnknownName::new("workspace crate", k, crates.iter().map(|c| c.name.as_str()))
                .list_with("cargo faster rdeps")
                .into(),
        );
    }
    let mut s = graph::section(&crates, krate);
    graph::add_edges(m, &crates, krate, &mut s);
    Ok(s)
}

fn check_cost_section(ctx: &Context, o: &checkcost::Options) -> Result<Section, Error> {
    let r = checkcost::run(ctx, o)?;
    let (_, module_lines) = graph::count_lines(&r.src_dir);
    let mut s = profile_section(&r.profile, Some(&r.crate_name), &module_lines)?;
    s.facts.insert(
        0,
        Fact {
            key: "rustc wall time".into(),
            value: cargo_faster::fmt::secs(r.wall_secs),
        },
    );
    if r.failed {
        s.fact(
            "note",
            "rustc reported errors; the profile covers what it got through",
        );
    }
    if o.keep.is_none()
        && let Some(dir) = r.profile.parent()
    {
        let _ = std::fs::remove_dir_all(dir);
    } else {
        s.fact("profile", r.profile.display().to_string());
    }
    Ok(s)
}

fn full_report(ctx: &Context, m: &cargo_faster::context::Metadata, report: &mut Report) {
    report
        .sections
        .push(setup::check(ctx, &setup::Tools::detect(ctx)));
    report
        .sections
        .push(cache::check(ctx, &cache::Inputs::gather(ctx)));
    report
        .sections
        .push(machine::check(&machine::Inputs::gather(
            ctx,
            machine::cache_dir(ctx),
        )));
    report
        .sections
        .push(deps::section(&deps::analyze_with_sources(m)));
    report
        .sections
        .push(tests::check(ctx, &tests::Inputs::gather(ctx)));
    let crates = graph::gather(m);
    let mut rdeps = graph::section(&crates, None);
    graph::add_edges(m, &crates, None, &mut rdeps);
    report.sections.push(rdeps);
    match timings_section(ctx, None, None, None) {
        Ok(s) => report.sections.push(s),
        Err(e) => report.skipped.push(Fact {
            key: "timings".into(),
            value: e.to_string(),
        }),
    }
    report.skipped.push(Fact {
        key: "check-cost".into(),
        value: "builds and self-profiles a crate; run `cargo faster check-cost` when type checking is the slow part".into(),
    });
}

fn profile_section(
    file: &Path,
    crate_name: Option<&str>,
    module_lines: &[(String, usize)],
) -> Result<Section, Error> {
    let name = crate_name.map_or_else(|| checkcost::crate_from_file(file), str::to_owned);
    eprintln!("cargo faster: analyzing {}", file.display());
    let p = checkcost::load(file)?;
    let unit = shlex::split(&p.cmd).map(|args| checkcost::Unit::from_args(&args));
    let mut s = profile::section(&profile::analyze(&p), &name, module_lines);
    if let Some(u) = &unit {
        s.name = format!("{} ({})", s.name, u.label());
        s.facts.insert(
            0,
            Fact {
                key: "target".into(),
                value: u.describe(),
            },
        );
    }
    s.fact(
        "measured with",
        format!(
            "rustc{} with RUSTC_BOOTSTRAP=1 -Zself-profile -Zself-profile-events=default,args, CARGO_INCREMENTAL=0, no RUSTC_WRAPPER",
            if unit.as_ref().is_some_and(|u| u.test) { " --test" } else { "" }
        ),
    );
    Ok(s)
}

/// What `timings --run` (or `--edit`) builds.
struct TimingsRun {
    build: timings::run::Build,
    edit: Option<String>,
    probe: timings::run::Probe,
    nice: i32,
    cargo_args: Vec<String>,
}

fn timings_section(
    ctx: &Context,
    file: Option<&Path>,
    run: Option<&TimingsRun>,
    baseline: Option<&Path>,
) -> Result<Section, Error> {
    let Some(r) = run else {
        let path = match file {
            Some(f) => f.to_path_buf(),
            None => timings::newest_report(&ctx.target_dir()).ok_or(Error::NoTimings)?,
        };
        let mut s = timings_file_section(Some(ctx), &path, baseline)?;
        let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        if let Some(written) = modified(&path) {
            let age = SystemTime::now()
                .duration_since(written)
                .map_or(0.0, |d| d.as_secs_f64());
            let lock = modified(&ctx.root().join("Cargo.lock"));
            let (age, stale) = timings::report_age(age, lock.is_some_and(|l| l > written));
            s.facts.insert(
                1,
                Fact {
                    key: "age".into(),
                    value: age,
                },
            );
            if let Some(w) = stale {
                s.facts.insert(
                    2,
                    Fact {
                        key: "stale".into(),
                        value: w,
                    },
                );
            }
        }
        return Ok(s);
    };
    let (path, facts, failed) = timings_run(ctx, r)?;
    let mut s = timings_file_section(Some(ctx), &path, baseline)?;
    for (i, f) in facts.into_iter().enumerate() {
        s.facts.insert(i, f);
    }
    if let Some((code, stderr)) = failed {
        s.facts.insert(
            1,
            Fact {
                key: "result".into(),
                value: format!("build failed (exit {code})"),
            },
        );
        // The unit that failed has no time, so a build that failed on
        // its first unit looks like one with nothing to do.
        s.findings.retain(|f| f.id != "nothing-rebuilt");
        s.extend([timings::build_failed(code, &stderr)]);
    }
    Ok(s)
}

/// What a failed build exited with, and the end of its stderr.
type Failed = Option<(i32, String)>;

/// Run the build with `--timings` (after the probe edit, for `--edit`)
/// and return the report it wrote, with facts saying what was measured,
/// and how the build failed if it did.
fn timings_run(ctx: &Context, r: &TimingsRun) -> Result<(PathBuf, Vec<Fact>, Failed), Error> {
    use timings::run::{Edit, Probe, edit_target, probe_text};
    let nextest = tests::Inputs::gather(ctx).nextest_installed;
    let args = r.build.args(nextest);
    let cargo = cargo_bin(&ctx.env);
    let mut shown = args.clone();
    shown.push("--timings");
    shown.extend(r.cargo_args.iter().map(String::as_str));
    let mut facts = vec![Fact {
        key: "build".into(),
        value: format!("cargo {}", shown.join(" ")),
    }];
    let target = match &r.edit {
        Some(name) => Some(edit_target(
            ctx.metadata.as_ref().ok_or(Error::NoTimings)?,
            name,
        )?),
        None => None,
    };
    let mut edit = None;
    if let Some(t) = &target {
        // Bring the tree up to date first, so the timed build only pays
        // for the probe (and not for earlier edits, or the last restore).
        let warm = build(ctx, r.nice, &cargo)
            .args(&args)
            .args(&r.cargo_args)
            .current_dir(ctx.root())
            .output()?;
        if !warm.success() {
            return Err(format!(
                "the warm-up build failed, so an edit can't be measured\n{}",
                warm.stderr_lossy().trim()
            )
            .into());
        }
        let probe = if t.proc_macro { Probe::Body } else { r.probe };
        let n = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if cmd::dry_run_enabled() {
            eprintln!(
                "cargo faster: --dry-run: would append a probe to {} and restore it after the build",
                t.file.display()
            );
        } else {
            edit = Some(Edit::apply(&t.file, &probe_text(probe, n))?);
        }
        facts.push(Fact {
            key: "edit".into(),
            value: format!(
                "{}: {} in {}{}, restored after the build",
                t.krate,
                probe.describe(),
                t.file.strip_prefix(ctx.root()).unwrap_or(&t.file).display(),
                if t.proc_macro && r.probe == Probe::Api {
                    " (a proc-macro crate can't export functions)"
                } else {
                    ""
                }
            ),
        });
    }
    let start = SystemTime::now();
    let out = build(ctx, r.nice, &cargo)
        .args(&args)
        .arg("--timings")
        .args(&r.cargo_args)
        .current_dir(ctx.root())
        .output();
    if let Some(e) = edit {
        e.restore()?;
    }
    let out = out?;
    if cmd::dry_run_enabled() {
        return Err(Error::DryRun(
            "--dry-run: nothing was built, so there is nothing to time".into(),
        ));
    }
    let failed = (!out.success()).then(|| {
        (
            out.status.code().unwrap_or(-1),
            out.stderr_lossy().trim().to_owned(),
        )
    });
    let Some(path) = fresh_report(ctx, start) else {
        return Err(match failed {
            Some((code, stderr)) => Error::BuildFailed(format!(
                "the build failed (exit {code}) before writing a cargo-timing.html\n{stderr}"
            )),
            None => format!(
                "the build wrote no cargo-timing.html\n{}",
                out.stderr_lossy().trim()
            )
            .into(),
        });
    };
    Ok((path, facts, failed))
}

/// The newest report in the target directory, if it was written after
/// `since` (by the build we just ran, not an older one).
fn fresh_report(ctx: &Context, since: SystemTime) -> Option<PathBuf> {
    timings::reports(&ctx.target_dir())
        .into_iter()
        .next()
        .filter(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t >= since)
        })
}

/// `tests`: the configuration checks; with `--run`, also time the test
/// build, run the tests, report both, then the build's timings.
fn tests_sections(
    ctx: &Context,
    run: bool,
    nice: i32,
    args: &[String],
    report: &mut Report,
) -> Result<(), Error> {
    let inputs = tests::Inputs::gather(ctx);
    report.sections.push(tests::check(ctx, &inputs));
    if !run {
        return Ok(());
    }
    let build_start = SystemTime::now();
    let started = Instant::now();
    let built = build(ctx, nice, cargo_bin(&ctx.env))
        .args(["nextest", "run", "--workspace", "--no-run", "--timings"])
        .args(args)
        .current_dir(ctx.root())
        .output()?;
    let build_secs = started.elapsed().as_secs_f64();
    if !built.success() {
        return Err(format!("building the tests failed\n{}", built.stderr_lossy().trim()).into());
    }
    let started = Instant::now();
    let ran = build(ctx, nice, cargo_bin(&ctx.env))
        .env("NEXTEST_EXPERIMENTAL_LIBTEST_JSON", "1")
        .args(["nextest", "run", "--workspace", "--no-fail-fast"])
        .args(["--message-format", "libtest-json-plus"])
        .args(args)
        .current_dir(ctx.root())
        .output()?;
    let run_secs = started.elapsed().as_secs_f64();
    if cmd::dry_run_enabled() {
        return Err(Error::DryRun(
            "--dry-run: nothing was built or run, so there is nothing to time".into(),
        ));
    }
    let run = testrun::parse(&ran.stdout_lossy());
    if run.tests.is_empty() && !ran.success() {
        return Err(format!(
            "the test run failed before any test finished\n{}",
            ran.stderr_lossy().trim()
        )
        .into());
    }
    let config = inputs.nextest_config.as_ref();
    let profile = testrun::profile(args, ctx.var("NEXTEST_PROFILE"));
    let mut s = testrun::section(&testrun::Inputs {
        build_secs: Some(build_secs),
        run_secs,
        threads: testrun::threads(
            args,
            ctx.var("NEXTEST_TEST_THREADS"),
            config,
            &profile,
            ctx.cpus(),
        ),
        slow_timeout: testrun::slow_timeout(config, &profile),
        run,
    });
    s.facts.insert(
        0,
        Fact {
            key: "nextest profile".into(),
            value: profile,
        },
    );
    report.sections.push(s);
    // Only the report this build wrote, not an older one.
    match fresh_report(ctx, build_start) {
        Some(path) => report
            .sections
            .push(timings_file_section(Some(ctx), &path, None)?),
        None => report.skipped.push(Fact {
            key: "timings".into(),
            value: "the test build wrote no cargo-timing.html".into(),
        }),
    }
    Ok(())
}

/// Read a report; with a workspace, join the cache wrapper's records,
/// compare with a baseline (given, or the newest earlier check report when
/// this one is a build) and count the workspace crates it rebuilt.
fn timings_file_section(
    ctx: Option<&Context>,
    path: &Path,
    baseline: Option<&Path>,
) -> Result<Section, Error> {
    let read = |p: &Path| -> Result<timings::Timings, Error> {
        let html =
            std::fs::read_to_string(p).map_err(|e| format!("reading {}: {e}", p.display()))?;
        Ok(timings::Timings::parse_html(&html)?)
    };
    let mut t = read(path)?;
    let base = match baseline {
        Some(b) => Some((b.to_path_buf(), read(b)?)),
        None if !t.is_check() => timings::check_before(path),
        None => None,
    };
    let ws = ctx.and_then(|c| c.metadata.as_ref().map(timings::Workspace::from_metadata));
    let mut joined = None;
    if let (Some(c), Some(start)) = (ctx, t.start_unix_ms) {
        let events = timings::wrapper::gather(c, start);
        if !events.is_empty() {
            joined = Some(timings::wrapper::join(&mut t, &events));
        }
    }
    let a = timings::analyze_with(
        &t,
        timings::Extra {
            baseline: base.as_ref().map(|(_, b)| b),
            workspace: ws.as_ref(),
        },
    );
    let mut s = timings::section(&a, &path.display().to_string());
    if let Some((p, _)) = &base {
        s.fact("baseline", p.display().to_string());
    }
    if joined == Some(0) {
        s.fact(
            "cache",
            "kache has records for this period but none matched the report's units",
        );
    }
    if let (Some(m), Some(ws)) = (ctx.and_then(|c| c.metadata.as_ref()), &ws) {
        let found = graph::edges::gather(m, &timings::path_edges(&a, ws));
        if let Some(d) = graph::edges::detail(&found, false) {
            s.details.push(d);
        }
        s.extend(graph::edges::findings(&found, 3));
    }
    Ok(s)
}
