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

use crate::context::{Context, Package, Target, capture_for, cargo_bin, on_path};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// What to profile and how.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Package to profile (a workspace member). Default: the root package,
    /// else the member with the most code.
    pub package: Option<String>,
    /// Profile the binary target with this name instead of the library.
    pub bin: Option<String>,
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
}

/// Find the `Running` line for `crate_name` (`--crate-name`, underscored)
/// whose source root is `src`, and parse it.
#[must_use]
pub fn find_invocation(stderr: &str, crate_name: &str, src: &str) -> Option<Invocation> {
    let needle = format!("--crate-name {crate_name} ");
    stderr
        .lines()
        .filter(|l| l.contains(&needle) && l.contains(src))
        .find_map(|line| {
            let start = line.find("Running `")? + "Running `".len();
            let end = line.rfind('`')?;
            (end > start).then(|| parse_invocation(&line[start..end]))?
        })
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
pub fn pick<'a>(ctx: &'a Context, opts: &Options) -> Result<(&'a Package, &'a Target), String> {
    let m = ctx
        .metadata
        .as_ref()
        .ok_or("check-cost needs cargo metadata")?;
    let members: Vec<&Package> = m.members().collect();
    let pkg = if let Some(name) = &opts.package {
        *members
            .iter()
            .find(|p| &p.name == name)
            .ok_or_else(|| format!("no workspace member called {name}"))?
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
    let target = match &opts.bin {
        Some(bin) => pkg
            .targets
            .iter()
            .find(|t| kinds(t, "bin") && &t.name == bin),
        None => pkg
            .targets
            .iter()
            .find(|t| kinds(t, "lib") || kinds(t, "rlib") || kinds(t, "proc-macro"))
            .or_else(|| pkg.targets.iter().find(|t| kinds(t, "bin"))),
    }
    .ok_or_else(|| format!("{} has no matching target", pkg.name))?;
    Ok((pkg, target))
}

/// Profile one crate. Prints progress to stderr.
///
/// # Errors
///
/// When cargo or rustc can't run or no profile is written.
pub fn run(ctx: &Context, opts: &Options) -> Result<Run, String> {
    let (pkg, target) = pick(ctx, opts)?;
    let crate_name = target.name.replace('-', "_");
    let root = ctx.root().to_path_buf();
    // Make cargo run rustc for this crate. Only the mtime changes.
    std::fs::File::options()
        .append(true)
        .open(&target.src_path)
        .and_then(|f| f.set_modified(SystemTime::now()))
        .map_err(|e| format!("touching {}: {e}", target.src_path.display()))?;
    eprintln!(
        "cargo faster: running `cargo check --workspace -vv` to capture the rustc command for {} ({crate_name})",
        pkg.name
    );
    let mut args: Vec<String> = ["check", "--workspace", "-vv"].map(String::from).to_vec();
    args.extend(opts.cargo_args.iter().cloned());
    let out = nice_capture(ctx, opts.nice, &cargo_bin(&ctx.env), &args, &root, &[])?;
    let stderr = out.stderr_lossy();
    let src = target
        .src_path
        .strip_prefix(&root)
        .unwrap_or(&target.src_path)
        .display()
        .to_string();
    let inv = find_invocation(&stderr, &crate_name, &src).ok_or_else(|| {
        format!(
            "cargo didn't run rustc for {crate_name}; is it a dependency of the selected packages? last output:\n{}",
            stderr.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
        )
    })?;
    let dir = match &opts.keep {
        Some(d) => d.clone(),
        None => std::env::temp_dir().join(format!("cargo-faster-{}", std::process::id())),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let mut env: Vec<(String, String)> = inv.env.clone();
    env.push(("RUSTC_BOOTSTRAP".into(), "1".into()));
    env.push(("CARGO_INCREMENTAL".into(), "0".into()));
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
    let replay = nice_capture(
        ctx,
        opts.nice,
        &inv.rustc,
        &replay_args(&inv.args, &dir),
        &root,
        &env,
    )?;
    let wall_secs = started.elapsed().as_secs_f64();
    let profile = newest_profile(&dir).ok_or_else(|| {
        format!(
            "rustc wrote no profile in {}:\n{}",
            dir.display(),
            replay.stderr_lossy().trim()
        )
    })?;
    Ok(Run {
        crate_name,
        profile,
        wall_secs,
        failed: !replay.success(),
    })
}

fn nice_capture(
    ctx: &Context,
    nice: i32,
    prog: &str,
    args: &[String],
    dir: &Path,
    env: &[(String, String)],
) -> Result<bpaf_kit::cmd::Output, String> {
    // `nice -n N env K=V prog args`: exact environment, lower priority,
    // without touching our own process.
    let mut cmd: Vec<String> = Vec::new();
    if nice > 0 && on_path(&ctx.env, "nice").is_some() {
        cmd.extend(["nice".to_owned(), "-n".to_owned(), nice.to_string()]);
    }
    if !env.is_empty() {
        cmd.push("env".to_owned());
        cmd.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
    }
    cmd.push(prog.to_owned());
    cmd.extend(args.iter().cloned());
    capture_for(
        &cmd[0],
        &cmd[1..],
        Some(dir),
        core::time::Duration::from_secs(4 * 3600),
    )
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
pub fn load(path: &Path) -> Result<crate::profile::Profile, String> {
    let stem = if path.extension().is_some_and(|x| x == "mm_profdata") {
        path.with_extension("")
    } else {
        path.to_path_buf()
    };
    crate::profile::Profile::load(&stem)
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
        let inv = find_invocation(&stderr, "app", "crates/app/src/lib.rs").unwrap();
        assert_eq!(inv.rustc, "/u/bin/rustc");
        assert_eq!(inv.wrapper.as_deref(), Some("/u/bin/kache"));
        assert_eq!(
            inv.env[3],
            ("OUT_DIR".into(), "/w/target/debug/build/app-1/out".into())
        );
        assert_eq!(inv.args[..2], ["--crate-name", "app"]);
        assert!(find_invocation(&stderr, "other", "crates/app/src/lib.rs").is_none());

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
}
