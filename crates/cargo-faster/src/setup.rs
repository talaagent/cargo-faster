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

/// All rustflags in effect, from the environment or config, and where.
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
    if let Some(f) = ctx.var("CARGO_BUILD_RUSTFLAGS") {
        return (
            f.split_whitespace().map(str::to_owned).collect(),
            "CARGO_BUILD_RUSTFLAGS".into(),
        );
    }
    let flags = ctx.config.rustflags(&ctx.rustc.host);
    let src = flags
        .first()
        .map(|(_, f)| f.display().to_string())
        .unwrap_or_default();
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
fn linker(ctx: &Context, opts: &[String]) -> (String, String) {
    for o in opts {
        if let Some(l) = o.strip_prefix("Clink-arg=-fuse-ld=") {
            return (l.into(), "rustflags".into());
        }
        if let Some(p) = o.strip_prefix("Clink-arg=--ld-path=") {
            return (p.rsplit('/').next().unwrap_or(p).into(), "rustflags".into());
        }
        if let Some(l) = o.strip_prefix("Clinker-flavor=") {
            return (l.into(), "rustflags".into());
        }
        if o.starts_with("Clink-self-contained") || o.starts_with("Zlinker-features") {
            return ("rust-lld".into(), "rustflags".into());
        }
    }
    let host = &ctx.rustc.host;
    if let Some((l, f)) = ctx.config.get_str(&format!("target.{host}.linker")) {
        if l.contains("mold") || l.contains("wild") {
            return (
                l.rsplit('/').next().unwrap_or(l).into(),
                f.display().to_string(),
            );
        }
        return (
            format!("{l} (default ld unless it passes -fuse-ld)"),
            f.display().to_string(),
        );
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
    let (link, link_src) = linker(ctx, &opts);
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

    // Jobs.
    let jobs = ctx
        .var("CARGO_BUILD_JOBS")
        .map(|j| (j.to_owned(), "CARGO_BUILD_JOBS".to_owned()))
        .or_else(|| {
            ctx.config
                .get("build.jobs")
                .map(|(v, f)| (v.to_string(), f.display().to_string()))
        });
    if let Some((j, src)) = &jobs {
        s.fact("jobs", format!("{j} ({src})"));
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
        s.fact("jobs", format!("{} (default: one per CPU)", tools.cpus));
    }

    // Rustflags.
    if !flags.is_empty() {
        s.fact("rustflags", format!("{} ({flags_src})", flags.join(" ")));
    }
    if ctx.var("RUSTFLAGS").is_some() && !ctx.config.rustflags(&ctx.rustc.host).is_empty() {
        f.push(
            Finding::new("rustflags-env-overrides-config", Severity::Medium, "RUSTFLAGS overrides the rustflags in your cargo config", "linking")
                .saw("RUSTFLAGS is set in the environment, and cargo config also sets rustflags. Cargo uses only one source: the environment wins, and the config flags are ignored.")
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
}
