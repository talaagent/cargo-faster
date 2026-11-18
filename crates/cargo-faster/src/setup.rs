//! The build setup audit: profiles, linker, rustflags, jobs.

use crate::context::Context;
use crate::finding::{Finding, Section, Severity};

/// What else the audit needs that isn't in the [`Context`].
#[derive(Debug, Clone, Default)]
pub struct Tools {
    /// Fast linkers found on `PATH` (`mold`, `wild`, `ld.lld`).
    pub linkers: Vec<String>,
    /// CPUs available.
    pub cpus: usize,
}

impl Tools {
    /// Look at the real system.
    #[must_use]
    pub fn detect(ctx: &Context) -> Self {
        Self {
            linkers: ["mold", "wild", "ld.lld"]
                .into_iter()
                .filter(|l| crate::context::on_path(&ctx.env, l).is_some())
                .map(str::to_owned)
                .collect(),
            cpus: ctx.cpus(),
        }
    }
}

/// `CARGO_TARGET_<TRIPLE>_<KEY>`, the environment form of
/// `target.<triple>.<key>`.
fn target_env(host: &str, key: &str) -> String {
    format!(
        "CARGO_TARGET_{}_{key}",
        host.to_uppercase().replace(['-', '.'], "_")
    )
}

/// All rustflags in effect and where they came from, in cargo's order:
/// `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`, then target rustflags, then
/// build rustflags.
fn rustflags(ctx: &Context) -> (Vec<String>, String) {
    if let Some(enc) = ctx.var("CARGO_ENCODED_RUSTFLAGS") {
        return (
            enc.split('\u{1f}').map(str::to_owned).collect(),
            "CARGO_ENCODED_RUSTFLAGS".into(),
        );
    }
    if let Some(f) = ctx.var("RUSTFLAGS") {
        return (
            f.split_whitespace().map(str::to_owned).collect(),
            "RUSTFLAGS".into(),
        );
    }
    config_rustflags(ctx)
}

/// The rustflags from cargo config and its `CARGO_*` environment forms:
/// `target.<triple>.rustflags` (and `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`,
/// merged after the config's) if any, else `build.rustflags` (and
/// `CARGO_BUILD_RUSTFLAGS`). Sources are listed in order, comma-separated.
fn config_rustflags(ctx: &Context) -> (Vec<String>, String) {
    let host = &ctx.rustc.host;
    let with_env = |cfg: Vec<(String, std::path::PathBuf)>, var: String| {
        let mut out: Vec<(String, String)> = cfg
            .into_iter()
            .map(|(f, p)| (f, p.display().to_string()))
            .collect();
        if let Some(v) = ctx.var(&var) {
            out.extend(v.split_whitespace().map(|f| (f.to_owned(), var.clone())));
        }
        out
    };
    let mut flags = with_env(
        ctx.config.target_rustflags(host),
        target_env(host, "RUSTFLAGS"),
    );
    if flags.is_empty() {
        flags = with_env(
            ctx.config.build_rustflags(),
            "CARGO_BUILD_RUSTFLAGS".to_owned(),
        );
    }
    let mut srcs: Vec<&str> = Vec::new();
    for (_, src) in &flags {
        if !srcs.contains(&src.as_str()) {
            srcs.push(src);
        }
    }
    let src = srcs.join(", ");
    (flags.into_iter().map(|(f, _)| f).collect(), src)
}

/// Pairs like `-C link-arg=x` and `-Clink-arg=x` both become `link-arg=x`.
fn codegen_opts(flags: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        if f == "-C" || f == "-Z" {
            if let Some(v) = it.next() {
                out.push(format!("{}{v}", &f[1..]));
            }
        } else if let Some(v) = f.strip_prefix("-C") {
            out.push(format!("C{v}"));
        } else if let Some(v) = f.strip_prefix("-Z") {
            out.push(format!("Z{v}"));
        } else if let Some(v) = f.strip_prefix("--codegen=") {
            out.push(format!("C{v}"));
        }
    }
    out
}

fn debug_level(v: &toml::Value) -> String {
    match v {
        toml::Value::Boolean(false) | toml::Value::Integer(0) => "none".into(),
        toml::Value::Integer(1) => "limited".into(),
        toml::Value::Boolean(true) | toml::Value::Integer(2) => "full".into(),
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Which linker is in use, and how we know.
fn linker(ctx: &Context, opts: &[String], flags_src: &str) -> (String, String) {
    for o in opts {
        if let Some(l) = o.strip_prefix("Clink-arg=-fuse-ld=") {
            return (l.into(), flags_src.into());
        }
        if let Some(p) = o.strip_prefix("Clink-arg=--ld-path=") {
            return (p.rsplit('/').next().unwrap_or(p).into(), flags_src.into());
        }
        if let Some(l) = o.strip_prefix("Clinker-flavor=") {
            return (l.into(), flags_src.into());
        }
        if o.starts_with("Clink-self-contained") || o.starts_with("Zlinker-features") {
            return ("rust-lld".into(), flags_src.into());
        }
    }
    let host = &ctx.rustc.host;
    let linker_env = target_env(host, "LINKER");
    let configured = ctx.var(&linker_env).map_or_else(
        || {
            ctx.config
                .get_str(&format!("target.{host}.linker"))
                .map(|(l, f)| (l, f.display().to_string()))
        },
        |l| Some((l, linker_env.clone())),
    );
    if let Some((l, src)) = configured {
        if l.contains("mold") || l.contains("wild") {
            return (l.rsplit('/').next().unwrap_or(l).into(), src);
        }
        return (format!("{l} (default ld unless it passes -fuse-ld)"), src);
    }
    if host == "x86_64-unknown-linux-gnu" && ctx.rustc.version() >= (1, 90) {
        return (
            "rust-lld".into(),
            "default since Rust 1.90 on x86_64 Linux".into(),
        );
    }
    if host.contains("linux") {
        return ("ld (system default)".into(), "default".into());
    }
    ("platform default".into(), "default".into())
}

/// Run the audit.
#[must_use]
#[allow(clippy::too_many_lines, reason = "a flat list of independent checks")]
pub fn check(ctx: &Context, tools: &Tools) -> Section {
    let mut s = Section::new("setup");
    let mut f = Vec::new();
    let (flags, flags_src) = rustflags(ctx);
    let opts = codegen_opts(&flags);

    // Debug info.
    let dev_debug = ctx.profile_value("dev", "debug");
    let level = dev_debug
        .as_ref()
        .map_or_else(|| "full".to_owned(), |(v, _)| debug_level(v));
    let level_src = dev_debug.as_ref().map_or("default", |(_, s)| s.as_str());
    s.fact("dev debuginfo", format!("{level} ({level_src})"));
    let deps_debuginfo = ctx
        .manifest
        .get("profile")
        .and_then(|p| p.get("dev"))
        .and_then(|d| d.get("package"))
        .and_then(|p| p.get("*"))
        .and_then(|p| p.get("debug"));
    if matches!(level.as_str(), "full" | "2") {
        f.push(
            Finding::new("full-debuginfo", Severity::Medium, "Dev builds emit full debug info", "debug-info")
                .saw(format!("profile.dev debug is `{level}` ({level_src}).{}", if deps_debuginfo.is_none() { " Dependencies inherit it." } else { "" }))
                .why("Full debug info is the biggest thing rustc writes in a dev build: it slows codegen, makes every object file and binary larger, and gives the linker more to do. Backtraces only need line tables.")
                .fix("In Cargo.toml:\n[profile.dev]\ndebug = \"line-tables-only\"\n[profile.dev.package.\"*\"]\ndebug = false\nKeep `debug = true` for the crates you step through in a debugger, or put it in a separate `[profile.debugging]`."),
        );
    }

    // Codegen units.
    if let Some((v, src)) = ctx.profile_value("dev", "codegen-units")
        && let Some(n) = v.as_integer()
        && n < 16
    {
        f.push(
            Finding::new("low-dev-codegen-units", Severity::Medium, format!("profile.dev uses codegen-units = {n}"), "codegen-units")
                .saw(format!("codegen-units = {n} for dev ({src}). The default is 256 for incremental builds."))
                .why("Fewer codegen units means less LLVM parallelism: a big crate's code generation runs on a few cores while the rest idle.")
                .fix("Remove the setting from the dev profile. Keep low codegen-units for release builds where runtime speed matters."),
        );
    }

    // Opt level.
    if let Some((v, src)) = ctx.profile_value("dev", "opt-level") {
        let lvl = v.to_string().trim_matches('"').to_owned();
        s.fact("dev opt-level", format!("{lvl} ({src})"));
        if lvl != "0" {
            f.push(
                Finding::new("dev-opt-level", Severity::Medium, format!("Your own crates build at opt-level {lvl} in dev"), "optimizing-dependencies-in-dev-builds")
                    .saw(format!("profile.dev opt-level = {lvl} ({src})."))
                    .why("Optimizing your workspace crates makes every edit-compile cycle pay for LLVM passes. Usually only dependencies (image decoders, crypto, regex) need to be fast at runtime.")
                    .fix("Set opt-level back to 0 for dev and optimize only dependencies:\n[profile.dev.package.\"*\"]\nopt-level = 2\nDependencies rarely change, so their optimized build is cached."),
            );
        }
    }

    // Linker.
    let (link, link_src) = linker(ctx, &opts, &flags_src);
    s.fact("linker", format!("{link} ({link_src})"));
    let fast = ["mold", "wild", "lld", "rust-lld", "ld.lld"]
        .iter()
        .any(|l| link.contains(l));
    if !fast && ctx.rustc.host.contains("linux") {
        let have = if tools.linkers.is_empty() {
            "neither is on PATH".to_owned()
        } else {
            format!("{} is on PATH", tools.linkers.join(", "))
        };
        f.push(
            Finding::new("slow-linker", Severity::Medium, "Linking uses the system ld", "linking")
                .saw(format!("No fast linker is configured for {} ({have}).", ctx.rustc.host))
                .why("Every test binary and every bin is linked after every change. GNU ld is single-threaded; mold and lld link large binaries several times faster.")
                .fix(format!("In .cargo/config.toml:\n[target.{}]\nlinker = \"clang\"\nrustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]", ctx.rustc.host)),
        );
    } else if link == "rust-lld" && tools.linkers.iter().any(|l| l == "mold" || l == "wild") {
        f.push(
            Finding::new("faster-linker-available", Severity::Info, "mold or wild is installed but lld is in use", "linking")
                .saw(format!("The default rust-lld links; {} is on PATH.", tools.linkers.join(", ")))
                .why("lld is already fast. mold and wild can be faster still on large binaries, but measure: the difference is often under a second per link.")
                .fix(format!("Try it for a week:\n[target.{}]\nrustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]", ctx.rustc.host)),
        );
    }

    // split-debuginfo (macOS).
    if ctx.rustc.host.contains("apple")
        && let Some((v, src)) = ctx.profile_value("dev", "split-debuginfo")
        && v.as_str() == Some("packed")
    {
        f.push(
            Finding::new("split-debuginfo-packed", Severity::Low, "split-debuginfo = \"packed\" on macOS", "debug-info")
                .saw(format!("profile.dev split-debuginfo = packed ({src})."))
                .why("Packing runs dsymutil on every link, which is slow. `unpacked` (the default on macOS) leaves debug info in the object files.")
                .fix("Remove the setting, or set split-debuginfo = \"unpacked\"."),
        );
    }

    jobs(ctx, tools, &mut s, &mut f);

    // Rustflags.
    if !flags.is_empty() {
        s.fact("rustflags", format!("{} ({flags_src})", flags.join(" ")));
    }
    let (shadowed, shadowed_src) = config_rustflags(ctx);
    if ctx.var("RUSTFLAGS").is_some() && !shadowed.is_empty() {
        f.push(
            Finding::new("rustflags-env-overrides-config", Severity::Medium, "RUSTFLAGS overrides the rustflags in your cargo config", "linking")
                .saw(format!("RUSTFLAGS is set in the environment, and rustflags are also set in {shadowed_src} (`{}`). Cargo uses only one source: RUSTFLAGS wins, and those are ignored.", shadowed.join(" ")))
                .why("Changing rustflags changes every crate's fingerprint. Builds with and without the variable (a terminal vs your editor, CI vs local) rebuild the whole dependency graph each time they alternate.")
                .fix("Put the flags in .cargo/config.toml only, and unset RUSTFLAGS. Make sure rust-analyzer and your shell see the same environment."),
        );
    }
    if opts.iter().any(|o| o == "Ctarget-cpu=native") {
        f.push(
            Finding::new("target-cpu-native", Severity::Low, "target-cpu=native in dev rustflags", "optimizing-dependencies-in-dev-builds")
                .saw(format!("rustflags include -C target-cpu=native ({flags_src})."))
                .why("It doesn't slow compiling much, but it makes artifacts machine-specific: a shared cache (sccache, kache, CI) can't reuse them across machines with different CPUs.")
                .fix("Use it for release or benchmark builds only."),
        );
    }
    if opts.iter().any(|o| o.starts_with("Zshare-generics")) {
        f.push(
            Finding::new("share-generics-redundant", Severity::Info, "-Zshare-generics is already the default in dev", "sharing-generics")
                .saw(format!("rustflags include -Zshare-generics ({flags_src})."))
                .why("Generic sharing is on by default for opt-level 0 and 1 builds. The flag needs a nightly compiler (or RUSTC_BOOTSTRAP) and changes nothing in dev.")
                .fix("Remove it, unless you set it for an optimized profile on purpose."),
        );
    }
    if let Some(t) = opts.iter().find(|o| o.starts_with("Zthreads")) {
        f.push(
            Finding::new("parallel-frontend", Severity::Info, "The parallel frontend is enabled", "the-parallel-frontend")
                .saw(format!("rustflags include -{t} ({flags_src}).{}", if ctx.rustc.nightly { "" } else { " This compiler is not nightly, so it only works with RUSTC_BOOTSTRAP." }))
                .why("-Zthreads parallelizes parts of type checking within one crate. It helps big leaf crates on idle machines and can slow things down on a loaded one.")
                .fix("Measure with `cargo faster check-cost` and `--timings` before and after, back to back."),
        );
    }

    // Release profile notes.
    let lto = ctx
        .profile_value("release", "lto")
        .map(|(v, _)| v.to_string());
    if let Some(l) = lto.filter(|l| l.contains("fat") || l == "true") {
        s.fact("release lto", l);
    }
    s.extend(f);
    s
}

/// The job limit: `CARGO_BUILD_JOBS` or `build.jobs`, and a jobserver
/// cargo would share with other processes.
fn jobs(ctx: &Context, tools: &Tools, s: &mut Section, f: &mut Vec<Finding>) {
    let jobs = ctx
        .var("CARGO_BUILD_JOBS")
        .map(|j| (j.to_owned(), "CARGO_BUILD_JOBS".to_owned()))
        .or_else(|| {
            ctx.config
                .get("build.jobs")
                .map(|(v, f)| (v.to_string(), f.display().to_string()))
        });
    let shared = ctx.jobserver();
    let capped = if shared.is_some() {
        ", at most: the jobserver's free tokens are the real limit"
    } else {
        ""
    };
    if let Some((j, src)) = &jobs {
        s.fact("jobs", format!("{j} ({src}){capped}"));
        if let Ok(n) = j.parse::<usize>()
            && tools.cpus > 0
            && n * 2 <= tools.cpus
        {
            f.push(
                Finding::new("low-jobs", Severity::Low, format!("cargo is limited to {n} jobs on {} CPUs", tools.cpus), "the-machine")
                    .saw(format!("jobs = {n} ({src}); this machine has {} CPUs.", tools.cpus))
                    .why("cargo runs one rustc per job. Limiting jobs is right on a shared or memory-starved machine, but otherwise leaves cores idle in the wide parts of the build.")
                    .fix("If the limit was for memory or a shared box, keep it. Otherwise remove it and let cargo use every core."),
            );
        }
    } else {
        s.fact(
            "jobs",
            format!("{} (default: one per CPU){capped}", tools.cpus),
        );
    }
    if let Some((var, auth)) = shared {
        s.fact("jobserver", format!("{auth} ({var})"));
        f.push(
            Finding::new("shared-jobserver", Severity::Medium, "cargo takes its job tokens from a shared jobserver", "shared-jobservers")
                .saw(format!("{var} has --jobserver-auth={auth}, so cargo joins that jobserver instead of making its own: it runs one unit, and a second only when it gets a token from it. The jobs setting is only an upper bound."))
                .why("A jobserver hands out a fixed number of tokens to every process that joins it. When it is machine-wide, other builds hold the tokens and yours runs one unit at a time: the build looks serial, and its critical path is only the order tokens came in.")
                .fix(format!("On a shared machine keep it for everyday builds: it's what stops builds from overloading the box. To time the build itself, run without it at a quiet moment: `env -u {var} cargo faster timings --run` (unset MAKEFLAGS, CARGO_MAKEFLAGS and MFLAGS if more than one is set).")),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{CargoConfig, RustcInfo};

    fn ctx(manifest: &str, config: &str) -> Context {
        Context {
            manifest: manifest.parse().unwrap(),
            config: CargoConfig::from_strs(&[("/w/.cargo/config.toml", config)]),
            rustc: RustcInfo {
                release: "1.98.1".into(),
                host: "x86_64-unknown-linux-gnu".into(),
                nightly: false,
            },
            ..Context::default()
        }
    }

    fn ids(s: &Section) -> Vec<&str> {
        s.findings.iter().map(|f| f.id).collect()
    }

    #[test]
    fn defaults_suggest_line_tables() {
        let s = check(
            &ctx("", ""),
            &Tools {
                linkers: vec![],
                cpus: 32,
            },
        );
        assert_eq!(ids(&s), ["full-debuginfo"]);
        assert!(
            s.facts
                .iter()
                .any(|f| f.key == "linker" && f.value.starts_with("rust-lld"))
        );
    }

    #[test]
    fn tuned_setup_is_quiet() {
        let s = check(
            &ctx(
                "[profile.dev]\ndebug = \"line-tables-only\"\n[profile.dev.package.\"*\"]\ndebug = false\n",
                "[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]\n",
            ),
            &Tools {
                linkers: vec!["mold".into()],
                cpus: 32,
            },
        );
        assert!(ids(&s).is_empty(), "{:?}", ids(&s));
        assert!(
            s.facts
                .iter()
                .any(|f| f.key == "linker" && f.value.starts_with("mold"))
        );
    }

    #[test]
    fn flags_problems() {
        let mut c = ctx(
            "[profile.dev]\ndebug = 1\ncodegen-units = 1\nopt-level = 1\n",
            "[build]\njobs = 4\nrustflags = [\"-Zshare-generics=y\", \"-C\", \"target-cpu=native\"]\n",
        );
        c.rustc.host = "aarch64-unknown-linux-gnu".into();
        let s = check(
            &c,
            &Tools {
                linkers: vec![],
                cpus: 32,
            },
        );
        let got = ids(&s);
        for want in [
            "low-dev-codegen-units",
            "dev-opt-level",
            "slow-linker",
            "low-jobs",
            "target-cpu-native",
            "share-generics-redundant",
        ] {
            assert!(got.contains(&want), "{want} missing from {got:?}");
        }
        assert!(!got.contains(&"full-debuginfo"));
    }

    #[test]
    fn env_rustflags_override() {
        let mut c = ctx(
            "[profile.dev]\ndebug = 0\n",
            "[build]\nrustflags = [\"-Dwarnings\"]\n",
        );
        c.env.insert("RUSTFLAGS".into(), "-C debuginfo=0".into());
        assert!(ids(&check(&c, &Tools::default())).contains(&"rustflags-env-overrides-config"));
    }

    #[test]
    fn shared_jobserver() {
        let mut c = ctx("[profile.dev]\ndebug = 0\n", "");
        c.env.insert("CARGO_BUILD_JOBS".into(), "16".into());
        c.env.insert(
            "MAKEFLAGS".into(),
            "--jobserver-auth=fifo:/dev/shm/js".into(),
        );
        let s = check(
            &c,
            &Tools {
                linkers: vec![],
                cpus: 32,
            },
        );
        assert!(ids(&s).contains(&"shared-jobserver"), "{:?}", ids(&s));
        let fact = |k: &str| s.facts.iter().find(|f| f.key == k).unwrap().value.clone();
        assert_eq!(fact("jobserver"), "fifo:/dev/shm/js (MAKEFLAGS)");
        assert!(fact("jobs").contains("free tokens are the real limit"));
        c.env.remove("MAKEFLAGS");
        assert!(!ids(&check(&c, &Tools::default())).contains(&"shared-jobserver"));
    }

    fn linker_fact(s: &Section) -> String {
        s.facts
            .iter()
            .find(|f| f.key == "linker")
            .map(|f| f.value.clone())
            .unwrap_or_default()
    }

    #[test]
    fn target_rustflags_from_env() {
        let mut c = ctx("[profile.dev]\ndebug = 0\n", "");
        c.env.insert(
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS".into(),
            "-C link-arg=-fuse-ld=mold".into(),
        );
        let s = check(
            &c,
            &Tools {
                linkers: vec!["mold".into()],
                cpus: 32,
            },
        );
        assert!(ids(&s).is_empty(), "{:?}", ids(&s));
        assert_eq!(
            linker_fact(&s),
            "mold (CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS)"
        );
    }

    #[test]
    fn build_rustflags_ignored_when_target_sets_them() {
        let mut c = ctx(
            "[profile.dev]\ndebug = 0\n",
            "[build]\nrustflags = [\"-C\", \"link-arg=-fuse-ld=lld\"]\n",
        );
        c.env.insert(
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS".into(),
            "-C link-arg=-fuse-ld=mold".into(),
        );
        assert_eq!(
            linker_fact(&check(&c, &Tools::default())),
            "mold (CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS)"
        );
        // Without target rustflags, CARGO_BUILD_RUSTFLAGS joins build.rustflags.
        c.env.clear();
        c.env.insert(
            "CARGO_BUILD_RUSTFLAGS".into(),
            "-C target-cpu=native".into(),
        );
        let s = check(&c, &Tools::default());
        assert_eq!(
            linker_fact(&s),
            "lld (/w/.cargo/config.toml, CARGO_BUILD_RUSTFLAGS)"
        );
        assert!(ids(&s).contains(&"target-cpu-native"));
    }

    #[test]
    fn target_linker_from_env() {
        let mut c = ctx("[profile.dev]\ndebug = 0\n", "");
        c.env.insert(
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER".into(),
            "/nix/store/x/bin/wild".into(),
        );
        assert_eq!(
            linker_fact(&check(&c, &Tools::default())),
            "wild (CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER)"
        );
    }
}
