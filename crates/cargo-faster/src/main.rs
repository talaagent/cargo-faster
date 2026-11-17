//! `cargo faster`: find out why your Rust build is slow and how to make it
//! faster.

use bpaf::{Parser, construct, long, positional, pure};
use bpaf_kit::log::LogArgs;
use cargo_faster::context::{Context, capture_for, cargo_bin};
use cargo_faster::finding::{Fact, Report, Section};
use cargo_faster::{cache, checkcost, deps, graph, machine, profile, setup, tests, timings};
use core::time::Duration;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Clone)]
struct Opts {
    json: bool,
    manifest_path: Option<PathBuf>,
    log: LogArgs,
    cmd: Cmd,
}

#[derive(Debug, Clone)]
enum Cmd {
    Report,
    Setup,
    Cache,
    Machine,
    Deps,
    Tests,
    Rdeps(Option<String>),
    Timings {
        file: Option<PathBuf>,
        run: bool,
        nice: i32,
    },
    CheckCost {
        profile: Option<PathBuf>,
        opts: checkcost::Options,
    },
}

fn nice() -> impl Parser<i32> {
    long("nice")
        .help("Run builds under `nice -n N` (0 to not renice); the box may be shared")
        .argument::<i32>("N")
        .fallback(10)
        .display_fallback()
}

fn simple(cmd: Cmd, name: &'static str, descr: &'static str) -> impl Parser<Cmd> {
    pure(cmd).to_options().descr(descr).command(name)
}

fn command() -> impl Parser<Cmd> {
    let report = simple(
        Cmd::Report,
        "report",
        "A quick overall report: setup, cache, machine, deps, tests, crates (the default)",
    );
    let setup = simple(
        Cmd::Setup,
        "setup",
        "Audit profiles, linker, debuginfo, codegen units, jobs and rustflags",
    );
    let cache = simple(
        Cmd::Cache,
        "cache",
        "Check the compiler cache (sccache, kache), incremental and cold worktrees",
    );
    let machine = simple(
        Cmd::Machine,
        "machine",
        "Check load, concurrent builds, memory and disk",
    );
    let deps = simple(
        Cmd::Deps,
        "deps",
        "Check heavy features, duplicate versions and proc macros",
    );
    let tests = simple(
        Cmd::Tests,
        "tests",
        "Check nextest, target runners, thread limits and database-backed tests",
    );
    let krate = positional::<String>("CRATE")
        .help("Only this workspace crate")
        .optional();
    let rdeps = construct!(krate)
        .map(Cmd::Rdeps)
        .to_options()
        .descr("What rebuilds when a crate changes, and which crates are worth splitting")
        .command("rdeps");
    let run = long("run")
        .help("Run `cargo check --workspace --timings` first")
        .switch();
    let nice_t = nice();
    let file = positional::<PathBuf>("FILE")
        .help("A cargo-timing.html report (default: the newest in target/cargo-timings)")
        .optional();
    let timings = construct!(run, nice_t, file)
        .map(|(run, nice, file)| Cmd::Timings { file, run, nice })
        .to_options()
        .descr(
            "Read a --timings report: critical path, crates that serialize the build, long codegen",
        )
        .command("timings");
    let package = long("package")
        .short('p')
        .help("The workspace member to profile (default: the root package, else the biggest)")
        .argument::<String>("NAME")
        .optional();
    let bin = long("bin")
        .help("Profile this binary target instead of the library")
        .argument::<String>("NAME")
        .optional();
    let keep = long("keep")
        .help("Write the profile into DIR and keep it")
        .argument::<PathBuf>("DIR")
        .optional();
    let nice_c = nice();
    let profile = long("profile")
        .help("Analyze an existing .mm_profdata file instead of building")
        .argument::<PathBuf>("FILE")
        .optional();
    let cargo_args = positional::<String>("CARGO_ARGS")
        .help("Extra `cargo check` arguments, after `--` (e.g. -- --features x)")
        .strict()
        .many();
    let check_cost = construct!(package, bin, keep, nice_c, profile, cargo_args)
        .map(|(package, bin, keep, nice, profile, cargo_args)| Cmd::CheckCost {
            profile,
            opts: checkcost::Options { package, bin, cargo_args, nice, keep },
        })
        .to_options()
        .descr("Self-profile type checking of one crate: top queries, trait solving by type and body, proc macros")
        .footer("Uses RUSTC_BOOTSTRAP=1 so a stable rustc accepts -Zself-profile. `-p` picks the crate to profile; cargo itself always builds with --workspace so caches keyed on the package set (kache) stay warm.")
        .command("check-cost");
    construct!([
        report, setup, cache, machine, deps, tests, rdeps, timings, check_cost
    ])
    .fallback(Cmd::Report)
}

fn options() -> bpaf::OptionParser<Opts> {
    let json = long("json").help("Print the report as JSON").switch();
    let manifest_path = long("manifest-path")
        .help("Path to the workspace Cargo.toml")
        .argument::<PathBuf>("PATH")
        .optional();
    let log = bpaf_kit::log::log_args();
    let cmd = command();
    construct!(Opts { json, manifest_path, log, cmd })
        .to_options()
        .descr("Find out why your Rust build is slow and how to make it faster")
        .header("Every finding says what we saw, why it matters, the fix, and a link into the guide:\nhttps://github.com/talaagent/cargo-faster/blob/main/docs/guide.md")
}

fn main() -> ExitCode {
    let app = bpaf_kit::App::new(options(), bpaf_kit::build_info!());
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // `cargo faster ...` runs us as `cargo-faster faster ...`.
    if args.first().is_some_and(|a| a == "faster") {
        args.remove(0);
    }
    if args.len() == 1 && args[0] == "--man" {
        return print(&bpaf_kit::man::render(app.parser(), app.info()));
    }
    if let Some(verbose) = bpaf_kit::version::version_request(&args) {
        let info = app.info().clone().with_override();
        return print(&if verbose {
            info.verbose().to_string()
        } else {
            format!("{} {}\n", info.name, info.short())
        });
    }
    let strs: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let refs: Vec<&str> = strs.iter().map(String::as_str).collect();
    let opts = match app
        .parser()
        .run_inner(bpaf::Args::from(&refs[..]).set_name("cargo-faster"))
    {
        Ok(o) => o,
        Err(e) => {
            e.print_message(100);
            let code = e.exit_code();
            return ExitCode::from(u8::try_from(code).unwrap_or(1));
        }
    };
    let _ = bpaf_kit::log::init("warn", &opts.log);
    match run(&opts) {
        Ok(report) => {
            let text = if opts.json {
                serde_json::to_string_pretty(&report).unwrap_or_default() + "\n"
            } else {
                report.render_text(bpaf_kit::color_enabled(&std::io::stdout()))
            };
            print(&text)
        }
        Err(e) => {
            eprintln!("cargo faster: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print(s: &str) -> ExitCode {
    let mut out = std::io::stdout().lock();
    match out.write_all(s.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn run(opts: &Opts) -> Result<Report, String> {
    // An explicit profile file needs no workspace.
    if let Cmd::CheckCost {
        profile: Some(file),
        ..
    } = &opts.cmd
    {
        return Ok(Report {
            workspace: None,
            sections: vec![profile_section(file, None)?],
            skipped: Vec::new(),
        });
    }
    // So does an explicit timings report.
    if let Cmd::Timings {
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
        Cmd::Report => full_report(&ctx, &m, &mut report),
        Cmd::Setup => report
            .sections
            .push(setup::check(&ctx, &setup::Tools::detect(&ctx))),
        Cmd::Cache => report
            .sections
            .push(cache::check(&ctx, &cache::Inputs::gather(&ctx))),
        Cmd::Machine => report
            .sections
            .push(machine::check(&machine::Inputs::gather(
                &ctx,
                machine::cache_dir(&ctx),
            ))),
        Cmd::Deps => report.sections.push(deps::section(&deps::analyze(&m))),
        Cmd::Tests => report
            .sections
            .push(tests::check(&ctx, &tests::Inputs::gather(&ctx))),
        Cmd::Rdeps(only) => report
            .sections
            .push(graph::section(&graph::gather(&m), only.as_deref())),
        Cmd::Timings { file, run, nice } => {
            report
                .sections
                .push(timings_section(&ctx, file.as_deref(), *run, *nice)?);
        }
        Cmd::CheckCost { opts: o, .. } => {
            let r = checkcost::run(&ctx, o)?;
            let mut s = profile_section(&r.profile, Some(&r.crate_name))?;
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
            report.sections.push(s);
        }
    }
    Ok(report)
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
    report.sections.push(deps::section(&deps::analyze(m)));
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
            value: e,
        }),
    }
    report.skipped.push(Fact {
        key: "check-cost".into(),
        value: "builds and self-profiles a crate; run `cargo faster check-cost` when type checking is the slow part".into(),
    });
}

fn profile_section(file: &Path, crate_name: Option<&str>) -> Result<Section, String> {
    let name = crate_name.map_or_else(|| checkcost::crate_from_file(file), str::to_owned);
    eprintln!("cargo faster: analyzing {}", file.display());
    let p = checkcost::load(file)?;
    let mut s = profile::section(&profile::analyze(&p), &name);
    s.fact("measured with", "RUSTC_BOOTSTRAP=1 -Zself-profile -Zself-profile-events=default,args, CARGO_INCREMENTAL=0, no RUSTC_WRAPPER");
    Ok(s)
}

fn timings_section(
    ctx: &Context,
    file: Option<&Path>,
    run: bool,
    nice: i32,
) -> Result<Section, String> {
    if run {
        let mut args: Vec<String> = Vec::new();
        let cargo = cargo_bin(&ctx.env);
        let prog = if nice > 0 {
            args.extend(["-n".to_owned(), nice.to_string(), cargo]);
            "nice".to_owned()
        } else {
            cargo
        };
        args.extend(["check", "--workspace", "--timings"].map(String::from));
        eprintln!("cargo faster: running `{prog} {}`", args.join(" "));
        let out = capture_for(
            &prog,
            &args,
            Some(ctx.root()),
            Duration::from_secs(4 * 3600),
        )?;
        if !out.success() {
            eprintln!(
                "cargo faster: the build failed; reading whatever timings it wrote\n{}",
                out.stderr_lossy().trim()
            );
        }
    }
    let path = match file {
        Some(f) => f.to_path_buf(),
        None => timings::newest_report(&ctx.target_dir())
            .ok_or("no cargo-timing.html in target/cargo-timings; run `cargo build --timings` or `cargo faster timings --run`")?,
    };
    timings_file_section(&path)
}

fn timings_file_section(path: &Path) -> Result<Section, String> {
    let html =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let t = timings::Timings::parse_html(&html)?;
    Ok(timings::section(
        &timings::analyze(&t),
        &path.display().to_string(),
    ))
}
