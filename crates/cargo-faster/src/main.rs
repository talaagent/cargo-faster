//! `cargo faster`: find out why your Rust build is slow and how to make it
//! faster.

use bpaf::Bpaf;
use bpaf_kit::log::LogArgs;
use bpaf_kit::suggest::UnknownValue;
use bpaf_kit::{Cli, cmd};
use cargo_faster::context::{Context, build, cargo_bin};
use cargo_faster::error::Error;
use cargo_faster::finding::{Fact, Report, Section};
use cargo_faster::{cache, checkcost, deps, graph, machine, profile, setup, tests, timings};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Find out why your Rust build is slow and how to make it faster
#[derive(Debug, Clone, Bpaf, Cli)]
#[bpaf(
    options("faster"),
    header(
        "Every finding says what we saw, why it matters, the fix, and a link into the guide:\nhttps://github.com/talaagent/cargo-faster/blob/main/docs/guide.md"
    )
)]
#[cli(log = "warn")]
struct Opts {
    /// Print the report as JSON
    json: bool,
    /// Path to the workspace Cargo.toml
    #[bpaf(argument("PATH"))]
    manifest_path: Option<PathBuf>,
    /// Print the builds `timings --run` and `check-cost` would start, without
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
    #[bpaf(command)]
    Report,
    /// Audit profiles, linker, debuginfo, codegen units, jobs and rustflags
    #[bpaf(command)]
    Setup,
    /// Check the compiler cache (sccache, kache), incremental and cold worktrees
    #[bpaf(command)]
    Cache,
    /// Check load, concurrent builds, memory and disk
    #[bpaf(command)]
    Machine,
    /// Check heavy features, duplicate versions and proc macros
    #[bpaf(command)]
    Deps,
    /// Check nextest, target runners, thread limits and database-backed tests
    #[bpaf(command)]
    Tests,
    /// What rebuilds when a crate changes, and which crates are worth splitting
    #[bpaf(command)]
    Rdeps {
        /// Only this workspace crate
        #[bpaf(positional("CRATE"))]
        krate: Option<String>,
    },
    /// Read a --timings report: critical path, crates that serialize the build, long codegen
    #[bpaf(command)]
    Timings {
        /// Run `cargo check --workspace --timings` first
        run: bool,
        #[bpaf(external(nice))]
        nice: Nice,
        /// A cargo-timing.html report (default: the newest in target/cargo-timings)
        #[bpaf(positional("FILE"))]
        file: Option<PathBuf>,
    },
    /// Self-profile type checking of one crate: top queries, trait solving by type and body, proc macros
    #[bpaf(
        command,
        footer(
            "Uses RUSTC_BOOTSTRAP=1 so a stable rustc accepts -Zself-profile. `-p` picks the package and --lib/--bin/--test/--example/--bench the target; the rustc command is captured from `cargo check --workspace -vv` with the matching filter (--lib, --bin NAME, ...; with --tests also --profile test), which first checks the selected targets in every member and what they depend on. Cargo always gets --workspace, never -p, so caches keyed on the package set (kache) stay warm. Target flags in CARGO_ARGS (-- --tests) replace the filter; the lib's --test build is then profiled when they include --tests."
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
        profile: Option<PathBuf>,
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

// `--nice N`, for the subcommands that build. No doc comment: bpaf would
// make it a help section of its own.
#[derive(Debug, Clone, Copy, Bpaf)]
struct Nice {
    /// Run builds under `nice -n N` (0 to not renice); the box may be shared
    #[bpaf(long("nice"), argument("N"), fallback(10), display_fallback)]
    n: i32,
}

fn main() -> ExitCode {
    let opts = Opts::cli();
    if opts.dry_run {
        cmd::set_dry_run(true);
    }
    match run(&opts) {
        Ok(report) => {
            let text = if opts.json {
                serde_json::to_string_pretty(&report).unwrap_or_default() + "\n"
            } else {
                report.render_text(bpaf_kit::color_enabled(&std::io::stdout()))
            };
            print(&text)
        }
        Err(Error::DryRun(msg)) => {
            eprintln!("cargo faster: {msg}");
            ExitCode::SUCCESS
        }
        Err(e) => bpaf_kit::report::report_as("cargo faster", &e),
    }
}

fn print(s: &str) -> ExitCode {
    let mut out = std::io::stdout().lock();
    match out.write_all(s.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn run(opts: &Opts) -> Result<Report, Error> {
    // An explicit profile file needs no workspace.
    if let Command::CheckCost {
        profile: Some(file),
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
        ..
    } = &opts.cmd
    {
        return Ok(Report {
            workspace: None,
            sections: vec![timings_file_section(file)?],
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
        Command::Tests => report
            .sections
            .push(tests::check(&ctx, &tests::Inputs::gather(&ctx))),
        Command::Rdeps { krate } => report.sections.push(rdeps_section(&m, krate.as_deref())?),
        Command::Timings { file, run, nice } => {
            report
                .sections
                .push(timings_section(&ctx, file.as_deref(), *run, nice.n)?);
        }
        Command::CheckCost {
            package,
            target,
            tests,
            keep,
            nice,
            profile: _,
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
        return Err(UnknownValue::new(
            "workspace crate",
            k,
            crates.iter().map(|c| c.name.as_str()),
        )
        .into());
    }
    Ok(graph::section(&crates, krate))
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
    report
        .sections
        .push(graph::section(&graph::gather(m), None));
    match timings_section(ctx, None, false, 0) {
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

fn timings_section(
    ctx: &Context,
    file: Option<&Path>,
    run: bool,
    nice: i32,
) -> Result<Section, Error> {
    if run {
        let out = build(ctx, nice, cargo_bin(&ctx.env))
            .args(["check", "--workspace", "--timings"])
            .current_dir(ctx.root())
            .output()?;
        if !out.success() {
            eprintln!(
                "cargo faster: the build failed; reading whatever timings it wrote\n{}",
                out.stderr_lossy().trim()
            );
        }
    }
    let path = match file {
        Some(f) => f.to_path_buf(),
        None => timings::newest_report(&ctx.target_dir()).ok_or(Error::NoTimings)?,
    };
    timings_file_section(&path)
}

fn timings_file_section(path: &Path) -> Result<Section, Error> {
    let html =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let t = timings::Timings::parse_html(&html)?;
    Ok(timings::section(
        &timings::analyze(&t),
        &path.display().to_string(),
    ))
}
