//! `check-cost`: self-profile the type checking of one crate.
//!
//! We don't guess the rustc command line. We run `cargo check -vv
//! --workspace` once (after touching the crate's root file so cargo has to
//! run it), copy the exact `rustc` invocation cargo prints, and replay it
//! with:
//!
//! - `RUSTC_BOOTSTRAP=1`, so a stable compiler accepts `-Z` flags,
//! - `-Zself-profile -Zself-profile-events=default,args`, which records
//!   every query with its arguments (the predicate being proven, the body
//!   being checked),
//! - `CARGO_INCREMENTAL=0` and no `-C incremental`, so every query really
//!   runs,
//! - no `RUSTC_WRAPPER`, so a cache can't answer instead of rustc,
//! - its own output directory, so the real target directory isn't touched.
//!
//! The analysis lives in [`crate::profile`].

use crate::context::{Context, Package, Target, build, cargo_bin};
use crate::error::Error;
use bpaf_kit::cmd;
use bpaf_kit::suggest::UnknownValue;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// Which target of the package to profile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Select {
    /// The library, else the first binary.
    #[default]
    Auto,
    /// The library.
    Lib,
    /// The binary with this name.
    Bin(String),
    /// The integration test (`tests/NAME.rs`) with this name.
    Test(String),
    /// The example with this name.
    Example(String),
    /// The benchmark with this name.
    Bench(String),
}

/// What to profile and how.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Package to profile (a workspace member). Default: the root package,
    /// else the member with the most code.
    pub package: Option<String>,
    /// The target to profile.
    pub target: Select,
    /// Profile the `--test` (cfg(test), unit test) build of the library,
    /// binary or example instead of the normal one. Integration tests and
    /// benchmarks are always built with `--test`.
    pub tests: bool,
    /// Extra `cargo check` arguments (e.g. `--features x`). `--workspace`
    /// is always passed.
    pub cargo_args: Vec<String>,
    /// Niceness for the profiled rustc (0 = don't renice).
    pub nice: i32,
    /// Keep the profile here instead of a temporary directory.
    pub keep: Option<PathBuf>,
}

/// A replayable rustc invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Invocation {
    /// `KEY=value` pairs cargo set.
    pub env: Vec<(String, String)>,
    /// The rustc binary.
    pub rustc: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The wrapper cargo ran it under, if any.
    pub wrapper: Option<String>,
}

/// The result of a profiling run.
#[derive(Debug, Clone)]
pub struct Run {
    /// The crate profiled.
    pub crate_name: String,
    /// The `.mm_profdata` file.
    pub profile: PathBuf,
    /// Wall-clock time of the profiled rustc.
    pub wall_secs: f64,
    /// Whether rustc reported errors.
    pub failed: bool,
    /// The directory of the target's root source file (`src`).
    pub src_dir: PathBuf,
}

/// The compilation unit a rustc command line builds: which kind of target,
/// and whether it is the `--test` (cfg(test)) build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// `lib`, `proc-macro`, `bin`, `example`, `test` or `bench`.
    pub kind: &'static str,
    /// The root source file.
    pub src: String,
    /// Built with `--test`: `#[cfg(test)]` code and the test harness are in.
    pub test: bool,
}

impl Unit {
    /// Read the unit from rustc's arguments (the profile metadata's `cmd`,
    /// or a captured invocation).
    #[must_use]
    pub fn from_args<S: AsRef<str>>(args: &[S]) -> Self {
        let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
        let src = args
            .iter()
            .find(|a| !a.starts_with('-') && Path::new(a).extension().is_some_and(|x| x == "rs"))
            .map_or_else(String::new, |s| (*s).to_owned());
        let test = args.contains(&"--test");
        let crate_type = args
            .iter()
            .position(|a| *a == "--crate-type")
            .and_then(|i| args.get(i + 1).copied())
            .or_else(|| args.iter().find_map(|a| a.strip_prefix("--crate-type=")));
        let path = src.replace('\\', "/");
        let under =
            |dir: &str| path.starts_with(&format!("{dir}/")) || path.contains(&format!("/{dir}/"));
        let kind = if under("examples") {
            "example"
        } else if under("tests") && test {
            "test"
        } else if under("benches") && test {
            "bench"
        } else {
            match crate_type {
                Some("proc-macro") => "proc-macro",
                Some("bin") => "bin",
                Some(_) => "lib",
                None if path.ends_with("src/main.rs") || path.contains("src/bin/") => "bin",
                None if test => "lib",
                None => "bin",
            }
        };
        Self { kind, src, test }
    }

    /// `lib`, `lib, --test`, `test`: for the section header.
    #[must_use]
    pub fn label(&self) -> String {
        if self.test && !matches!(self.kind, "test" | "bench") {
            format!("{}, --test", self.kind)
        } else {
            self.kind.to_owned()
        }
    }

    /// A sentence for the `target` fact.
    #[must_use]
    pub fn describe(&self) -> String {
        let what = match (self.kind, self.test) {
            ("test", _) => "integration test".to_owned(),
            ("bench", _) => "benchmark, built as a test".to_owned(),
            (k, true) => format!(
                "{k} built with --test: the unit-test crate, with #[cfg(test)] code and the test harness"
            ),
            (k, false) => format!("{k}, without #[cfg(test)] code"),
        };
        if self.src.is_empty() {
            what
        } else {
            format!("{} ({what})", self.src)
        }
    }
}

/// The `cargo check` target filters for a selection, unless the user's own
/// cargo arguments already pick targets. Narrowing the capture build to the
/// one target keeps it from checking every test and example first.
#[must_use]
pub fn filter_args(
    target: &Select,
    kind: &str,
    name: &str,
    tests: bool,
    user: &[String],
) -> Vec<String> {
    let picks = |a: &String| {
        let flag = a.split('=').next().unwrap_or(a);
        matches!(
            flag,
            "--lib"
                | "--bin"
                | "--bins"
                | "--test"
                | "--tests"
                | "--example"
                | "--examples"
                | "--bench"
                | "--benches"
                | "--all-targets"
        )
    };
    if user.iter().any(picks) {
        return Vec::new();
    }
    let mut out: Vec<String> = match (target, kind) {
        (Select::Test(n), _) => vec!["--test".into(), n.clone()],
        (Select::Bench(n), _) => vec!["--bench".into(), n.clone()],
        (Select::Example(n), _) => vec!["--example".into(), n.clone()],
        (_, "bin") => vec!["--bin".into(), name.to_owned()],
        _ => vec!["--lib".into()],
    };
    let has_profile = user
        .iter()
        .any(|a| a == "--release" || a == "-r" || a.starts_with("--profile"));
    if tests && !has_profile && !matches!(target, Select::Test(_) | Select::Bench(_)) {
        out.extend(["--profile".into(), "test".into()]);
    }
    out
}

/// Whether the user's cargo arguments ask for the `--test` build
/// (`-- --tests` or `-- --profile test`).
#[must_use]
pub fn user_wants_tests(user: &[String]) -> bool {
    user.iter().any(|a| a == "--tests" || a == "--profile=test")
        || user
            .windows(2)
            .any(|w| w[0] == "--profile" && w[1] == "test")
}

/// Find the `Running` line for `crate_name` (`--crate-name`, underscored)
/// whose source root is `src`, and parse it. With `test`, only the line
/// built with (`Some(true)`) or without (`Some(false)`) `--test`: a test
/// build runs rustc on the library twice, once for each.
#[must_use]
pub fn find_invocation(
    stderr: &str,
    crate_name: &str,
    src: &str,
    test: Option<bool>,
) -> Option<Invocation> {
    let needle = format!("--crate-name {crate_name} ");
    stderr
        .lines()
        .map(strip_ansi)
        .filter(|l| l.contains(&needle) && l.contains(src))
        .filter_map(|line| {
            let start = line.find("Running `")? + "Running `".len();
            let end = line.rfind('`')?;
            (end > start).then(|| parse_invocation(&line[start..end]))?
        })
        .find(|inv| test.is_none_or(|t| inv.args.iter().any(|a| a == "--test") == t))
}

/// Drop terminal colour escapes (`ESC [ ... letter`), in case the user's
/// config forces colour on.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse a command line as cargo prints it: `K=V ... [wrapper] rustc args`.
#[must_use]
pub fn parse_invocation(cmd: &str) -> Option<Invocation> {
    let words = shlex::split(cmd)?;
    let mut it = words.into_iter().peekable();
    let mut inv = Invocation::default();
    while let Some(w) = it.next_if(|w| is_env(w)) {
        let (k, v) = w.split_once('=')?;
        inv.env.push((k.to_owned(), v.to_owned()));
    }
    let mut prog = it.next()?;
    // Wrappers sit in front of rustc: `kache /path/rustc --crate-name ...`.
    while !is_rustc(&prog) && it.peek().is_some_and(|n| !n.starts_with('-')) {
        let next = it.next()?;
        inv.wrapper = Some(core::mem::replace(&mut prog, next));
    }
    inv.rustc = prog;
    inv.args = it.collect();
    Some(inv)
}

fn is_env(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !k.starts_with(|c: char| c.is_ascii_digit())
    })
}

fn is_rustc(p: &str) -> bool {
    let name = Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.starts_with("rustc") || name.starts_with("clippy-driver")
}

/// Rewrite the arguments for a profiling replay into `out`.
#[must_use]
pub fn replay_args(args: &[String], out: &Path) -> Vec<String> {
    let mut res = Vec::with_capacity(args.len() + 4);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--error-format=json" => {}
            "--out-dir" | "--json" | "--error-format" => {
                it.next();
            }
            "-C" => {
                let Some(v) = it.next() else { break };
                if !v.starts_with("incremental=") {
                    res.push(a.clone());
                    res.push(v.clone());
                }
            }
            s if s.starts_with("--json=")
                || s.starts_with("-Cincremental=")
                || s.starts_with("--out-dir=") => {}
            s if s.starts_with("-Zself-profile") => {}
            _ => res.push(a.clone()),
        }
    }
    res.push("--out-dir".into());
    res.push(out.display().to_string());
    res.push(format!("-Zself-profile={}", out.display()));
    res.push("-Zself-profile-events=default,args".into());
    res
}

/// Pick the package and target to profile.
///
/// # Errors
///
/// When the package doesn't exist or has no matching target.
pub fn pick<'a>(ctx: &'a Context, opts: &Options) -> Result<(&'a Package, &'a Target), Error> {
    let m = ctx
        .metadata
        .as_ref()
        .ok_or("check-cost needs cargo metadata")?;
    let members: Vec<&Package> = m.members().collect();
    let pkg = if let Some(name) = &opts.package {
        *members.iter().find(|p| &p.name == name).ok_or_else(|| {
            UnknownValue::new(
                "workspace member",
                name,
                members.iter().map(|p| p.name.as_str()),
            )
        })?
    } else if let Some(p) = members
        .iter()
        .find(|p| p.manifest_path.parent() == Some(m.workspace_root.as_path()))
    {
        p
    } else {
        members
            .iter()
            .max_by_key(|p| {
                p.manifest_path
                    .parent()
                    .map_or(0, |d| crate::graph::count_lines(&d.join("src")).0)
            })
            .ok_or("the workspace has no members")?
    };
    let kinds = |t: &Target, k: &str| t.kind.iter().any(|x| x == k);
    let is_lib = |t: &Target| {
        t.kind
            .iter()
            .any(|k| k.ends_with("lib") || k == "proc-macro")
    };
    let named = |kind: &str, what: &str, name: &str| -> Result<&'a Target, Error> {
        pkg.targets
            .iter()
            .find(|t| kinds(t, kind) && t.name == name)
            .ok_or_else(|| {
                UnknownValue::new(
                    format!("{} {what}", pkg.name),
                    name,
                    pkg.targets
                        .iter()
                        .filter(|t| kinds(t, kind))
                        .map(|t| t.name.as_str()),
                )
                .into()
            })
    };
    let target = match &opts.target {
        Select::Bin(n) => named("bin", "binary", n)?,
        Select::Test(n) => named("test", "integration test", n)?,
        Select::Example(n) => named("example", "example", n)?,
        Select::Bench(n) => named("bench", "benchmark", n)?,
        Select::Lib => pkg
            .targets
            .iter()
            .find(|t| is_lib(t))
            .ok_or_else(|| format!("{} has no library target", pkg.name))?,
        Select::Auto => pkg
            .targets
            .iter()
            .find(|t| is_lib(t))
            .or_else(|| pkg.targets.iter().find(|t| kinds(t, "bin")))
            .ok_or_else(|| format!("{} has no library or binary target", pkg.name))?,
    };
    Ok((pkg, target))
}

/// The target filters for the capture build, and whether the invocation to
/// replay must be built with `--test` (`None`: integration tests and
/// benchmarks, which always are).
fn capture_plan(opts: &Options, target: &Target) -> (Vec<String>, Option<bool>) {
    let tests = opts.tests || user_wants_tests(&opts.cargo_args);
    let kind = if target.kind.iter().any(|k| k == "bin") {
        "bin"
    } else {
        "lib"
    };
    let filters = filter_args(&opts.target, kind, &target.name, tests, &opts.cargo_args);
    // Integration tests and benchmarks are always `--test`; for the rest,
    // the library is built both ways when tests are, so pick the one asked.
    let want_test = match opts.target {
        Select::Test(_) | Select::Bench(_) => None,
        _ => Some(tests),
    };
    (filters, want_test)
}

/// Profile one crate. Prints progress to stderr.
///
/// # Errors
///
/// When cargo or rustc can't run or no profile is written.
pub fn run(ctx: &Context, opts: &Options) -> Result<Run, Error> {
    let (pkg, target) = pick(ctx, opts)?;
    let crate_name = target.name.replace('-', "_");
    let root = ctx.root().to_path_buf();
    let (filters, want_test) = capture_plan(opts, target);
    eprintln!(
        "cargo faster: capturing the rustc command for {} ({crate_name}{}): `cargo check --workspace {}` checks the selected targets and what they depend on first",
        pkg.name,
        if want_test == Some(true) {
            ", --test"
        } else {
            ""
        },
        filters
            .iter()
            .chain(&opts.cargo_args)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    );
    // Make cargo run rustc for this crate. Only the mtime changes.
    if cmd::dry_run_enabled() {
        cmd::echo_line(format_args!("touch {}", target.src_path.display()));
    } else {
        std::fs::File::options()
            .append(true)
            .open(&target.src_path)
            .and_then(|f| f.set_modified(SystemTime::now()))
            .map_err(|e| format!("touching {}: {e}", target.src_path.display()))?;
    }
    let out = build(ctx, opts.nice, cargo_bin(&ctx.env))
        .args(["check", "--workspace", "-vv", "--color", "never"])
        .args(&filters)
        .args(&opts.cargo_args)
        .current_dir(&root)
        .output()?;
    if cmd::dry_run_enabled() {
        return Err(Error::DryRun(format!(
            "--dry-run: stopping before profiling {crate_name}; the rustc command line to replay comes from that build's output"
        )));
    }
    let stderr = out.stderr_lossy();
    let src = target
        .src_path
        .strip_prefix(&root)
        .unwrap_or(&target.src_path)
        .display()
        .to_string();
    let inv = find_invocation(&stderr, &crate_name, &src, want_test).ok_or_else(|| -> Error {
        format!(
            "cargo didn't run rustc for {crate_name}{}; is it a dependency of the selected packages? last output:\n{}",
            match want_test {
                Some(true) => " with --test",
                Some(false) => " without --test",
                None => "",
            },
            stderr.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
        ).into()
    })?;
    let dir = profile_dir(opts.keep.as_deref());
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    eprintln!(
        "cargo faster: profiling {crate_name} with RUSTC_BOOTSTRAP=1 -Zself-profile (no wrapper{}, no incremental){}",
        inv.wrapper
            .as_deref()
            .map(|w| format!(", was {w}"))
            .unwrap_or_default(),
        if opts.nice > 0 {
            format!(", nice -n {}", opts.nice)
        } else {
            String::new()
        }
    );
    let started = Instant::now();
    // The exact environment cargo used, at a lower priority, without
    // touching our own process. The command line runs to hundreds of
    // arguments: echo its start and end, with the environment collapsed.
    let replay = build(ctx, opts.nice, &inv.rustc)
        .args(replay_args(&inv.args, &dir))
        .envs(inv.env.iter().map(|(k, v)| (k, v)))
        .env("RUSTC_BOOTSTRAP", "1")
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(&root)
        .echo_width(REPLAY_ECHO_WIDTH)
        .output()?;
    let wall_secs = started.elapsed().as_secs_f64();
    let profile = newest_profile(&dir).ok_or_else(|| -> Error {
        format!(
            "rustc wrote no profile in {}:\n{}",
            dir.display(),
            replay.stderr_lossy().trim()
        )
        .into()
    })?;
    Ok(Run {
        crate_name,
        profile,
        wall_secs,
        failed: !replay.success(),
        src_dir: target
            .src_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
    })
}

/// Where the replay writes its profile: `--keep DIR`, else a temporary
/// directory that is removed on Ctrl-C.
fn profile_dir(keep: Option<&Path>) -> PathBuf {
    if let Some(d) = keep {
        d.to_path_buf()
    } else {
        let tmp = std::env::temp_dir();
        remove_stale_temp_dirs(&tmp);
        let dir = tmp.join(format!("{TEMP_PREFIX}{}", std::process::id()));
        // Removed on Ctrl-C; the caller removes it after the analysis.
        bpaf_kit::interrupt::on_interrupt({
            let dir = dir.clone();
            move || drop(std::fs::remove_dir_all(dir))
        })
        .keep();
        dir
    }
}

/// How much of the replayed rustc command line is echoed, in characters.
const REPLAY_ECHO_WIDTH: usize = 160;

/// Temporary profile directories are `<tmp>/cargo-faster-<pid>`.
pub const TEMP_PREFIX: &str = "cargo-faster-";

/// Remove the temporary profile directories of runs that are gone. Ctrl-C
/// removes the run's own directory (an `on_interrupt` cleanup), but a run
/// that is killed (`SIGKILL`, the OOM killer, a crash) leaves it behind;
/// the next run cleans it up. Only on systems with
/// `/proc`, where we can tell whether the process is still running.
pub fn remove_stale_temp_dirs(tmp: &Path) {
    let proc = Path::new("/proc");
    if !proc.join("self").exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(TEMP_PREFIX))
            .filter(|p| p.parse::<u32>().is_ok())
        else {
            continue;
        };
        if !proc.join(pid).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The most recently written `.mm_profdata` in `dir`.
#[must_use]
pub fn newest_profile(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "mm_profdata"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// Load a profile by its file name (with or without `.mm_profdata`).
///
/// # Errors
///
/// When it can't be read.
pub fn load(path: &Path) -> Result<crate::profile::Profile, Error> {
    let stem = if path.extension().is_some_and(|x| x == "mm_profdata") {
        path.with_extension("")
    } else {
        path.to_path_buf()
    };
    Ok(crate::profile::Profile::load(&stem)?)
}

/// The crate name from a profile file name (`tala_agent-12345.mm_profdata`).
#[must_use]
pub fn crate_from_file(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = stem.trim_end_matches(".mm_profdata");
    stem.rsplit_once('-')
        .filter(|(_, pid)| pid.chars().all(|c| c.is_ascii_digit()))
        .map_or(stem, |(n, _)| n)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "     Running `CARGO=/u/bin/cargo CARGO_PKG_NAME=app CARGO_MANIFEST_DIR=/w/crates/app OUT_DIR='/w/target/debug/build/app-1/out' /u/bin/kache /u/bin/rustc --crate-name app --edition=2024 crates/app/src/lib.rs --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type lib --emit=dep-info,metadata -C embed-bitcode=no -C debuginfo=1 -C incremental=/w/target/debug/incremental -C metadata=abc --out-dir /w/target/debug/deps -L dependency=/w/target/debug/deps --extern tokio=/w/target/debug/deps/libtokio-1.rmeta`";

    #[test]
    fn parses_cargo_running_line() {
        let stderr = format!("   Compiling app v0.1.0\n{LINE}\n");
        let inv = find_invocation(&stderr, "app", "crates/app/src/lib.rs", Some(false)).unwrap();
        assert_eq!(inv.rustc, "/u/bin/rustc");
        assert_eq!(inv.wrapper.as_deref(), Some("/u/bin/kache"));
        assert_eq!(
            inv.env[3],
            ("OUT_DIR".into(), "/w/target/debug/build/app-1/out".into())
        );
        assert_eq!(inv.args[..2], ["--crate-name", "app"]);
        assert!(find_invocation(&stderr, "other", "crates/app/src/lib.rs", None).is_none());
        // CARGO_TERM_COLOR=always wraps the status word in escapes.
        let coloured = LINE.replacen("Running", "\u{1b}[1m\u{1b}[92mRunning\u{1b}[0m", 1);
        assert_eq!(
            find_invocation(&coloured, "app", "crates/app/src/lib.rs", None),
            Some(inv.clone())
        );

        let args = replay_args(&inv.args, Path::new("/tmp/p"));
        let joined = args.join(" ");
        assert!(!joined.contains("incremental"), "{joined}");
        assert!(!joined.contains("json"), "{joined}");
        assert!(
            !joined.contains("/w/target/debug/deps --")
                || joined.contains("dependency=/w/target/debug/deps")
        );
        assert!(
            joined.contains("-C debuginfo=1 -C metadata=abc"),
            "{joined}"
        );
        assert!(
            joined.ends_with(
                "--out-dir /tmp/p -Zself-profile=/tmp/p -Zself-profile-events=default,args"
            ),
            "{joined}"
        );
    }

    #[test]
    fn no_wrapper() {
        let inv = parse_invocation("/r/rustc --crate-name x src/lib.rs").unwrap();
        assert_eq!((inv.rustc.as_str(), inv.wrapper), ("/r/rustc", None));
        assert_eq!(inv.env, []);
    }

    #[test]
    fn crate_names_from_files() {
        assert_eq!(
            crate_from_file(Path::new("/t/tala_agent_bin-2359592.mm_profdata")),
            "tala_agent_bin"
        );
        assert_eq!(crate_from_file(Path::new("x.mm_profdata")), "x");
    }

    #[test]
    fn picks_the_test_or_normal_build_of_the_library() {
        // `cargo check --tests` runs rustc on the library twice.
        let test_line = LINE.replace(" --crate-type lib ", " --test ");
        let stderr = format!("{test_line}\n{LINE}\n");
        let src = "crates/app/src/lib.rs";
        let has_test = |t| {
            find_invocation(&stderr, "app", src, t)
                .unwrap()
                .args
                .iter()
                .any(|a| a == "--test")
        };
        assert!(has_test(Some(true)));
        assert!(!has_test(Some(false)));
        assert!(has_test(None), "the first line when either will do");
        assert!(find_invocation(LINE, "app", src, Some(true)).is_none());
    }

    fn unit(cmd: &str) -> Unit {
        Unit::from_args(&shlex::split(cmd).unwrap())
    }

    #[test]
    fn reads_the_unit_from_the_command_line() {
        let lib = unit(
            "rustc --crate-name app --edition=2024 crates/app/src/lib.rs --crate-type lib --emit=metadata",
        );
        assert_eq!((lib.kind, lib.test), ("lib", false));
        assert_eq!(lib.label(), "lib");
        assert_eq!(lib.src, "crates/app/src/lib.rs");
        let t = unit("rustc --crate-name app crates/app/src/lib.rs --emit=metadata --test");
        assert_eq!(t.label(), "lib, --test");
        assert!(t.describe().contains("#[cfg(test)]"), "{}", t.describe());
        assert_eq!(
            unit("rustc --crate-name app src/main.rs --test").label(),
            "bin, --test"
        );
        assert_eq!(
            unit("rustc --crate-name app src/main.rs --crate-type bin").label(),
            "bin"
        );
        assert_eq!(
            unit("rustc --crate-name it a/tests/it.rs --test").label(),
            "test"
        );
        assert_eq!(
            unit("rustc --crate-name bb benches/bb.rs --test").label(),
            "bench"
        );
        assert_eq!(
            unit("rustc --crate-name ex a/examples/ex.rs --crate-type bin").label(),
            "example"
        );
        assert_eq!(
            unit("rustc --crate-name m m/src/lib.rs --crate-type proc-macro").label(),
            "proc-macro"
        );
        // A module directory called `tests` inside src is still the lib.
        assert_eq!(
            unit("rustc --crate-name a src/tests/x.rs --crate-type lib").label(),
            "lib"
        );
    }

    #[test]
    fn filters_the_capture_build_to_the_target() {
        let f = |t: &Select, kind, tests, user: &[&str]| {
            let user: Vec<String> = user.iter().map(|s| (*s).to_owned()).collect();
            filter_args(t, kind, "app", tests, &user).join(" ")
        };
        assert_eq!(f(&Select::Auto, "lib", false, &[]), "--lib");
        assert_eq!(f(&Select::Lib, "lib", true, &[]), "--lib --profile test");
        assert_eq!(
            f(&Select::Auto, "bin", true, &[]),
            "--bin app --profile test"
        );
        assert_eq!(
            f(&Select::Test("it".into()), "test", true, &[]),
            "--test it"
        );
        assert_eq!(
            f(&Select::Bench("bb".into()), "bench", false, &[]),
            "--bench bb"
        );
        assert_eq!(
            f(&Select::Example("ex".into()), "example", true, &[]),
            "--example ex --profile test"
        );
        // The user's own target flags or profile win.
        assert_eq!(f(&Select::Auto, "lib", true, &["--tests"]), "");
        assert_eq!(f(&Select::Auto, "lib", false, &["--bin=x"]), "");
        assert_eq!(f(&Select::Lib, "lib", true, &["--release"]), "--lib");
        assert_eq!(f(&Select::Lib, "lib", false, &["--features", "x"]), "--lib");

        let w =
            |a: &[&str]| user_wants_tests(&a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert!(w(&["--tests"]));
        assert!(w(&["--profile", "test"]));
        assert!(w(&["--profile=test"]));
        assert!(!w(&["--all-targets"]));
        assert!(!w(&["--features", "test"]));
    }
}
