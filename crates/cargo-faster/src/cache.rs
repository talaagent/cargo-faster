//! Compiler caches (sccache, kache), incremental compilation, and the
//! target directories of git worktrees.

use crate::context::{Context, capture, on_path};
use crate::finding::{Finding, Section, Severity};
use crate::fmt::{bytes, ellipsize, secs};
use serde_json::Value;
use std::path::PathBuf;

/// A git worktree of this repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Its directory.
    pub path: PathBuf,
    /// Whether it has a `target/` directory.
    pub has_target: bool,
    /// Whether it has incremental state (`target/debug/incremental`).
    pub has_incremental: bool,
}

/// Inputs for the cache check beyond the [`Context`].
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// `git worktree list` for the workspace.
    pub worktrees: Vec<Worktree>,
    /// `kache stats --json`.
    pub kache_stats: Option<Value>,
    /// `kache stats --json --last-build --full --root <root>`.
    pub kache_last: Option<Value>,
    /// `kache explain --json --root <root>`.
    pub kache_explain: Option<Value>,
    /// Whether `sccache` is on PATH (for the no-cache advice).
    pub sccache_on_path: bool,
}

/// Parse `git worktree list --porcelain`, checking each tree's target
/// directory on disk.
#[must_use]
pub fn parse_worktrees(
    porcelain: &str,
    exists: impl Fn(&std::path::Path) -> bool,
) -> Vec<Worktree> {
    porcelain
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(|p| {
            let path = PathBuf::from(p);
            Worktree {
                has_target: exists(&path.join("target")),
                has_incremental: exists(&path.join("target/debug/incremental")),
                path,
            }
        })
        .collect()
}

impl Inputs {
    /// Gather from the real system. Never fails: missing tools just leave
    /// fields empty.
    #[must_use]
    pub fn gather(ctx: &Context) -> Self {
        let root = ctx.root().to_path_buf();
        let worktrees = capture(
            "git",
            &["worktree".into(), "list".into(), "--porcelain".into()],
            Some(&root),
        )
        .ok()
        .filter(bpaf_kit::cmd::Output::success)
        .map(|o| parse_worktrees(&o.stdout_lossy(), std::path::Path::exists))
        .unwrap_or_default();
        let wrapper = wrapper(ctx).map(|(w, _)| w);
        let uses_kache = wrapper.as_deref().is_some_and(|w| w.contains("kache"));
        let json = |args: &[&str]| -> Option<Value> {
            let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
            let out = capture("kache", &args, Some(&root)).ok()?;
            serde_json::from_slice(&out.stdout).ok()
        };
        let root_s = root.display().to_string();
        Self {
            kache_stats: uses_kache.then(|| json(&["stats", "--json"])).flatten(),
            kache_last: uses_kache
                .then(|| {
                    json(&[
                        "stats",
                        "--json",
                        "--last-build",
                        "--full",
                        "--root",
                        &root_s,
                    ])
                })
                .flatten(),
            kache_explain: uses_kache
                .then(|| json(&["explain", "--json", "--root", &root_s]))
                .flatten(),
            sccache_on_path: on_path(&ctx.env, "sccache").is_some(),
            worktrees,
        }
    }
}

/// The `RUSTC_WRAPPER` in effect and where it's set.
#[must_use]
pub fn wrapper(ctx: &Context) -> Option<(String, String)> {
    for k in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
        if let Some(v) = ctx.var(k) {
            return Some((v.to_owned(), k.to_owned()));
        }
    }
    ctx.config
        .get_str("build.rustc-wrapper")
        .filter(|(v, _)| !v.is_empty())
        .map(|(v, f)| (v.to_owned(), f.display().to_string()))
}

fn f64_at(v: &Value, path: &[&str]) -> Option<f64> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_f64()
}

fn last_build_fact(total: f64, pass: f64, hit: f64, weighted: f64) -> String {
    if total < 1.0 && pass < 1.0 {
        "no compiles in the latest session (only probes); run a build first for per-build numbers"
            .to_owned()
    } else {
        format!(
            "{hit:.0}% hits ({weighted:.0}% by time) over {total:.0} crates, {pass:.0} passthroughs"
        )
    }
}

/// Run the check.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "a flat list of independent findings reads best in one place"
)]
pub fn check(ctx: &Context, inputs: &Inputs) -> Section {
    let mut s = Section::new("cache");
    let mut f = Vec::new();
    let wrapper = wrapper(ctx);
    let name = wrapper
        .as_ref()
        .map(|(w, _)| w.rsplit('/').next().unwrap_or(w).to_owned());
    s.fact(
        "rustc wrapper",
        wrapper
            .as_ref()
            .map_or_else(|| "none".to_owned(), |(w, src)| format!("{w} ({src})")),
    );
    let incremental_env = ctx.var("CARGO_INCREMENTAL");
    let incremental = match incremental_env {
        Some("0") => (false, "CARGO_INCREMENTAL=0".to_owned()),
        Some(_) => (true, "CARGO_INCREMENTAL".to_owned()),
        None => match ctx.profile_value("dev", "incremental") {
            Some((v, src)) => (v.as_bool().unwrap_or(true), src),
            None => match ctx.config.get("build.incremental") {
                Some((v, f)) => (v.as_bool().unwrap_or(true), f.display().to_string()),
                None => (true, "default for dev".to_owned()),
            },
        },
    };
    s.fact(
        "incremental (dev)",
        format!(
            "{} ({})",
            if incremental.0 { "on" } else { "off" },
            incremental.1
        ),
    );
    s.fact("target dir", ctx.target_dir().display().to_string());
    if ctx.var("RUSTC_WORKSPACE_WRAPPER").is_some() {
        f.push(
            Finding::new("workspace-wrapper", Severity::Medium, "RUSTC_WORKSPACE_WRAPPER is set", "sccache-and-kache")
                .saw(format!("RUSTC_WORKSPACE_WRAPPER={}", ctx.var("RUSTC_WORKSPACE_WRAPPER").unwrap_or_default()))
                .why("Cargo hashes the workspace wrapper into each workspace crate's -C metadata, so builds with and without it don't share artifacts: switching between them (clippy-driver, a profiler, your editor) rebuilds every workspace crate.")
                .fix("Set it only for the one command that needs it, not in your shell profile."),
        );
    }

    let worktrees = &inputs.worktrees;
    if worktrees.len() > 1 {
        let cold = worktrees.iter().filter(|w| !w.has_target).count();
        let incr = worktrees.iter().filter(|w| w.has_incremental).count();
        s.fact(
            "worktrees",
            format!(
                "{} ({cold} without target/, {incr} with incremental state)",
                worktrees.len()
            ),
        );
        if name.is_none() {
            let sev = if worktrees.len() >= 3 {
                Severity::High
            } else {
                Severity::Medium
            };
            f.push(
                Finding::new("no-cache-with-worktrees", sev, format!("{} worktrees and no compiler cache", worktrees.len()), "worktrees-and-cold-target-directories")
                    .saw(format!("This repository has {} git worktrees, each with its own target directory, and no RUSTC_WRAPPER.", worktrees.len()))
                    .why("Each worktree builds every dependency from scratch the first time, and incremental state never moves between them. With many worktrees (one per agent or branch), most compile time is rebuilding the same crates.")
                    .fix(format!("Install a compiler cache and set it as the global wrapper in ~/.cargo/config.toml:\n[build]\nrustc-wrapper = \"sccache\"{}\nOr share one CARGO_TARGET_DIR across worktrees if they build one at a time.", if inputs.sccache_on_path { "  # sccache is already installed" } else { "" })),
            );
        }
        if cold > 0 {
            f.push(
                Finding::new("cold-worktrees", Severity::Info, format!("{cold} worktree(s) have no target directory yet"), "worktrees-and-cold-target-directories")
                    .saw(format!("{cold} of {} worktrees have never been built.", worktrees.len()))
                    .why("The first build in a fresh worktree is a cold build: nothing incremental, everything from the cache at best. Incremental compilation only speeds up repeated builds in the same directory.")
                    .fix("Expect the first build in a new worktree to cost a full cached build. Don't measure speedups on it; measure the second edit-and-check."),
            );
        }
    }

    if let Some(n) = &name {
        if n.contains("sccache") && incremental.0 {
            f.push(
                Finding::new("sccache-incremental", Severity::Low, "sccache doesn't cache incremental builds", "sccache-and-kache")
                    .saw("RUSTC_WRAPPER is sccache and incremental compilation is on.")
                    .why("sccache passes incremental crates (your workspace members in dev) straight to rustc, so it only caches dependencies there. That's usually what you want: incremental is better for crates you edit.")
                    .fix("Nothing, if your workspace crates are small. If they are rebuilt cold a lot (CI, fresh worktrees), set CARGO_INCREMENTAL=0 in those environments so sccache caches them."),
            );
        }
    } else if !incremental.0 {
        f.push(
            Finding::new("incremental-off", Severity::Medium, "Incremental compilation is off with no cache", "incremental-compilation")
                .saw(format!("Incremental is off ({}) and no rustc wrapper caches the result.", incremental.1))
                .why("Without incremental, every edit re-checks the whole crate from scratch. In CI that's right (builds are cold anyway), but locally it makes repeated checks of a big crate much slower.")
                .fix("Leave CARGO_INCREMENTAL unset locally; set CARGO_INCREMENTAL=0 only in CI."),
        );
    }

    if let Some(st) = &inputs.kache_stats {
        let hit = f64_at(st, &["hit_rate_pct"]).unwrap_or(0.0);
        let weighted = f64_at(st, &["weighted_hit_rate_pct"]).unwrap_or(0.0);
        let saved = f64_at(st, &["time_saved_ms"]).unwrap_or(0.0) / 1000.0;
        s.fact(
            "kache (24h)",
            format!(
                "{hit:.0}% hits, {weighted:.0}% by compile time, {} saved",
                secs(saved)
            ),
        );
        let zc = f64_at(st, &["lifetime", "zero_copy_bytes"]).unwrap_or(0.0);
        let copied = f64_at(st, &["lifetime", "copied_bytes"]).unwrap_or(0.0);
        if zc + copied > 0.0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (zc_b, cp_b) = (zc as u64, copied as u64);
            s.fact(
                "kache restores",
                format!("{} reflinked, {} copied", bytes(zc_b), bytes(cp_b)),
            );
            if copied > zc * 0.5 && copied > 1e9 {
                f.push(
                    Finding::new("cache-copies", Severity::Low, "The cache copies artifacts instead of reflinking them", "worktrees-and-cold-target-directories")
                        .saw(format!("kache restored {} by copying and {} with reflinks.", bytes(cp_b), bytes(zc_b)))
                        .why("Reflinks (copy-on-write clones) restore a cached artifact in constant time. When the target directory is on a different filesystem from the cache, or the filesystem can't reflink, every hit is a full copy.")
                        .fix("Keep target directories and the cache store on the same reflink-capable filesystem (btrfs, XFS, APFS). `cargo faster machine` shows which filesystem each is on."),
                );
            }
        }
    }
    if let Some(last) = &inputs.kache_last {
        let sum = last.get("summary").unwrap_or(&Value::Null);
        let total = f64_at(sum, &["total_crates"]).unwrap_or(0.0);
        let pass = f64_at(sum, &["passthroughs"]).unwrap_or(0.0);
        let hit = f64_at(sum, &["hit_rate_pct"]).unwrap_or(0.0);
        let weighted = f64_at(sum, &["weighted_hit_rate_pct"]).unwrap_or(0.0);
        s.fact(
            "kache last build",
            last_build_fact(total, pass, hit, weighted),
        );
        let reasons: Vec<(String, f64)> = last
            .pointer("/bypass/reasons")
            .and_then(Value::as_array)
            .map(|rs| {
                rs.iter()
                    .filter_map(|r| {
                        Some((
                            r.get("reason")?.as_str()?.to_owned(),
                            r.get("count")?.as_f64()?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !reasons.is_empty() {
            s.details.push(format!(
                "kache passthroughs by reason\n{}",
                reasons
                    .iter()
                    .map(|(r, c)| format!("  {c:>6.0}  {r}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        let seed = reasons
            .iter()
            .find(|(r, _)| r.contains("seed"))
            .map_or(0.0, |r| r.1);
        if seed > 0.0 {
            f.push(
                Finding::new("kache-adaptive-seed", Severity::Info, "kache is seeding incremental state from another tree", "worktrees-and-cold-target-directories")
                    .saw(format!("{seed:.0} crate builds in the last build were 'adaptive seed' passthroughs."))
                    .why("Seeding copies another worktree's incremental state so the first build here can be incremental. We measured it slower than a plain cached build when the trees differ much, because rustc first has to invalidate most of the copied state. clippy never seeds.")
                    .fix("Compare a first build with seeding on and off, back to back, before relying on it."),
            );
        }
        if let Some(wait) =
            f64_at(last, &["timing", "total_permit_wait_ms"]).filter(|w| *w > 10_000.0)
        {
            s.fact("kache permit wait", secs(wait / 1000.0));
        }
        let suggestions: Vec<&str> = last
            .get("suggestions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !suggestions.is_empty() {
            s.details.push(format!(
                "kache suggests\n{}",
                suggestions
                    .iter()
                    .map(|x| format!("  - {x}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }
    if let Some(ex) = &inputs.kache_explain {
        let misses = f64_at(ex, &["misses"]).unwrap_or(0.0);
        let unexplained = f64_at(ex, &["unexplained"]).unwrap_or(0.0);
        let causes = ex
            .get("causes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !causes.is_empty() {
            let rows: Vec<String> = causes
                .iter()
                .take(6)
                .map(|c| {
                    format!(
                        "  {:>4} crates {:>8}  {}",
                        c.get("count").and_then(Value::as_u64).unwrap_or(0),
                        secs(c.get("compile_ms").and_then(Value::as_f64).unwrap_or(0.0) / 1000.0),
                        ellipsize(c.get("why").and_then(Value::as_str).unwrap_or("?"), 100)
                    )
                })
                .collect();
            s.details.push(format!(
                "why the last build missed the cache\n{}",
                rows.join("\n")
            ));
        }
        if misses > 0.0 && unexplained / misses >= 0.3 {
            f.push(
                Finding::new("kache-unexplained-misses", Severity::Info, "Many cache misses have no recorded cause", "sccache-and-kache")
                    .saw(format!("{unexplained:.0} of {misses:.0} misses in the last build are unexplained."))
                    .why("Without a cause you can't tell an edit (expected) from a flag or environment difference that splits the cache.")
                    .fix("Run `kache config set cache.explain_miss true`, rebuild, then `kache explain`."),
            );
        }
    }
    s.extend(f);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::CargoConfig;

    fn wt(n: usize, target: bool) -> Vec<Worktree> {
        (0..n)
            .map(|i| Worktree {
                path: format!("/w{i}").into(),
                has_target: target || i == 0,
                has_incremental: false,
            })
            .collect()
    }

    #[test]
    fn parses_porcelain() {
        let w = parse_worktrees(
            "worktree /a\nHEAD 1\nbranch refs/heads/main\n\nworktree /b\nHEAD 2\n",
            |p| p == std::path::Path::new("/a/target"),
        );
        assert_eq!(w.len(), 2);
        assert!(w[0].has_target && !w[1].has_target);
    }

    #[test]
    fn no_cache_many_worktrees() {
        let s = check(
            &Context::default(),
            &Inputs {
                worktrees: wt(4, false),
                ..Inputs::default()
            },
        );
        let ids: Vec<_> = s.findings.iter().map(|f| f.id).collect();
        assert_eq!(ids, ["no-cache-with-worktrees", "cold-worktrees"]);
        assert_eq!(s.findings[0].severity, Severity::High);
    }

    #[test]
    fn kache_fixture() {
        let ctx = Context {
            config: CargoConfig::from_strs(&[(
                "/h/.cargo/config.toml",
                "[build]\nrustc-wrapper = \"/nix/bin/kache\"\n",
            )]),
            ..Context::default()
        };
        let load = |n: &str| {
            serde_json::from_str(
                &std::fs::read_to_string(format!(
                    "{}/tests/fixtures/{n}",
                    env!("CARGO_MANIFEST_DIR")
                ))
                .unwrap(),
            )
            .ok()
        };
        let inputs = Inputs {
            worktrees: wt(3, true),
            kache_stats: load("kache-stats.json"),
            kache_last: load("kache-last-build.json"),
            kache_explain: load("kache-explain.json"),
            sccache_on_path: false,
        };
        let s = check(&ctx, &inputs);
        let ids: Vec<_> = s.findings.iter().map(|f| f.id).collect();
        assert!(ids.contains(&"kache-adaptive-seed"), "{ids:?}");
        assert!(ids.contains(&"kache-unexplained-misses"), "{ids:?}");
        assert!(!ids.contains(&"no-cache-with-worktrees"));
        assert!(
            s.facts
                .iter()
                .any(|f| f.key == "kache last build" && f.value.contains("1375 passthroughs"))
        );
        assert!(s.details.iter().any(|d| d.contains("adaptive seed")));
    }

    #[test]
    fn workspace_wrapper_and_incremental_off() {
        let mut ctx = Context::default();
        ctx.env
            .insert("RUSTC_WORKSPACE_WRAPPER".into(), "clippy-driver".into());
        ctx.env.insert("CARGO_INCREMENTAL".into(), "0".into());
        let ids: Vec<_> = check(&ctx, &Inputs::default())
            .findings
            .iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, ["workspace-wrapper", "incremental-off"]);
    }
}
