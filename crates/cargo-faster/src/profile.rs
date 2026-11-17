//! Self-profile advice: findings and the report section for one crate's
//! profile.
//!
//! Loading and analysis live in the `rustc-self-profile` crate
//! ([`Profile`], [`analyze`]); this module turns its [`Analysis`] into
//! findings.

use crate::finding::{Finding, Section, Severity, table};
use crate::fmt::{ellipsize, pct, secs};
pub use rustc_self_profile::{Analysis, Profile, analyze};
use rustc_self_profile::{Cost, HeavyType, short_trait};

/// Thresholds for the findings, as a share of the profiled time.
const SEND_SHARE: f64 = 2.0;

/// Turn an analysis into findings.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "a flat list of independent findings reads best in one place"
)]
///
/// `module_lines` are the crate's top-level source entries and their line
/// counts (`("agent", 9000)`, `("lib.rs", 120)`), as `rdeps` counts them;
/// pass `&[]` when the sources aren't at hand.
pub fn findings(a: &Analysis, crate_name: &str, module_lines: &[(String, usize)]) -> Vec<Finding> {
    let mut out = Vec::new();
    let total = a.total_secs.max(1e-9);
    let ts_share = pct(a.trait_solving_secs, total);

    // Send/Sync proofs re-done per async body.
    let heavy: Vec<&HeavyType> = a
        .heavy_types
        .iter()
        .filter(|h| {
            h.max_count >= 10
                && h.bodies >= 5
                && (h.secs >= 0.5 || pct(h.secs, total) >= SEND_SHARE)
        })
        .take(3)
        .collect();
    if let Some(top) = heavy.first() {
        let all: f64 = heavy.iter().map(|h| h.secs).sum();
        let names: Vec<String> = heavy
            .iter()
            .map(|h| format!("`{}` ({}, {} bodies)", h.ty, secs(h.secs), h.bodies))
            .collect();
        let sev = if pct(all, total) >= 10.0 {
            Severity::High
        } else {
            Severity::Medium
        };
        out.push(
            Finding::new("send-proofs-per-async-body", sev, format!("Send/Sync proofs for `{}` are repeated in every async body", short_ty(&top.ty)), "why-send-proofs-get-expensive")
                .saw(format!(
                    "Proving Send and Sync for {} took {} ({:.0}% of {crate_name}'s profile). `{}` was proven up to {} times, once per async body that holds it across an await, e.g. {}.",
                    names.join(", "), secs(all), pct(all, total), short_ty(&top.ty), top.max_count,
                    top.top_bodies.iter().filter(|b| !b.name.starts_with('(')).take(2).map(|b| format!("`{}`", ellipsize(&b.name, 80))).collect::<Vec<_>>().join(" and ")
                ))
                .why("rustc proves that every future handed to tokio::spawn (or required to be Send) is Send by walking the types it holds across .await. Results are not shared between bodies, so a big struct reached through Arc<T>, Weak<T> and every field is walked again in each one.")
                .fix(format!(
                    "Stop rustc from looking inside `{}`: hold it as `Arc<dyn Trait + Send + Sync>` (a trait object is Send/Sync by declaration, proven once), or move the heavy fields behind such an object. Where a future crosses a module boundary, return `BoxFuture<'_, T>` (`Pin<Box<dyn Future + Send>>`) so callers prove one boxed type. Cost: a vtable call or a box per call, rarely measurable next to I/O.",
                    short_ty(&top.ty)
                )),
        );
    }

    if a.trait_solving_secs > 1.0 && ts_share >= 25.0 {
        let cats: Vec<String> = a
            .trait_solving_by_category
            .iter()
            .take(3)
            .map(|c| format!("{} {}", c.name, secs(c.secs)))
            .collect();
        let body = a
            .proving_bodies
            .iter()
            .find(|b| !b.name.starts_with('('))
            .map(|b| {
                format!(
                    " The body doing the most is `{}` ({}, {:.0}%).",
                    ellipsize(&b.name, 90),
                    secs(b.secs),
                    pct(b.secs, total)
                )
            })
            .unwrap_or_default();
        out.push(
            Finding::new(
                "trait-solving-dominates",
                if ts_share >= 50.0 { Severity::High } else { Severity::Medium },
                format!("Trait solving is {ts_share:.0}% of {crate_name}'s check time"),
                "why-send-proofs-get-expensive",
            )
            .saw(format!(
                "evaluate_obligation took {} ({ts_share:.0}% of the profile). By what is being proved: {}.{body}",
                secs(a.trait_solving_secs),
                cats.join(", ")
            ))
            .why("Every generic bound, auto trait and async body is proven by the trait solver, and the answers are cached per body, not per crate. Deep types (big structs behind Arc, long generic chains, huge async state machines) are walked again in every body that needs them.")
            .fix("Start with the bodies doing the most trait solving: split them, and box the futures of their expensive branches. Then erase the types that show up most (`Arc<dyn Trait + Send + Sync>`, `BoxFuture`) at module boundaries."),
        );
    }

    if a.async_trait_secs >= 0.3 {
        let share = pct(a.async_trait_secs, total);
        let sev = if share >= 10.0 || a.async_trait_secs >= 5.0 {
            Severity::High
        } else if share >= 3.0 || a.async_trait_secs >= 1.0 {
            Severity::Medium
        } else {
            Severity::Low
        };
        out.push(
            Finding::new("async-trait-overhead", sev, format!("#[async_trait] futures cost {crate_name} {} of trait solving", secs(a.async_trait_secs)), "the-cost-of-async-trait")
                .saw(format!(
                    "{} obligations were proven in parameter environments with lifetime-outlives bounds (`where 'life0: 'async_trait`, the signature #[async_trait] gives every method) and took {} ({share:.0}% of the profile, {:.0}% of trait solving).",
                    a.async_trait_obligations,
                    secs(a.async_trait_secs),
                    pct(a.async_trait_secs, a.trait_solving_secs.max(1e-9))
                ))
                .why("#[async_trait] rewrites each method to return `Pin<Box<dyn Future + Send + 'async_trait>>` with `'life0: 'async_trait` bounds. rustc caches trait-solving answers by predicate and parameter environment, and those bounds make every method's environment different, so each body proves the same Send facts about the same types again. Native async fn in traits has no such bounds, and the answers are shared. This is time spent on the macro's shape, not on your types.")
                .fix(format!(
                    "Replace #[async_trait] with native `async fn` in traits (Rust 1.75+); use `fn m(&self) -> impl Future<Output = T> + Send` where callers spawn; use dynosaur for traits held as `dyn`. Then ban the macro in clippy.toml:\n{}\nTradeoff: a mechanical diff across every impl.",
                    crate::deps::CLIPPY_BAN
                )),
        );
    }

    let macro_secs: f64 = a.proc_macros.iter().map(|m| m.secs).sum();
    if macro_secs >= 1.0 || (macro_secs >= 0.3 && pct(macro_secs, total) >= 10.0) {
        let top: Vec<String> = a
            .proc_macros
            .iter()
            .take(3)
            .map(|m| format!("{} ({} over {} uses)", m.name, secs(m.secs), m.count))
            .collect();
        out.push(
            Finding::new("slow-proc-macros", if pct(macro_secs, total) >= 15.0 { Severity::Medium } else { Severity::Low }, "Procedural macros take a noticeable share", "procedural-macros")
                .saw(format!("Macro expansion took {} ({:.0}%): {}.", secs(macro_secs), pct(macro_secs, total), top.join(", ")))
                .why("Proc macros run on every build of the crate, and their output must then be type checked too. Heavy derives on many types, or macros that query a database at compile time, add up.")
                .fix("Derive only what you use, move rarely-changing derived types into their own crate (so edits elsewhere don't re-expand them), and use offline modes for compile-time-checked SQL (`SQLX_OFFLINE=true`)."),
        );
    }

    if a.codegen_secs >= 2.0 && pct(a.codegen_secs, total) >= 40.0 {
        out.push(
            Finding::new("codegen-dominates", Severity::Medium, "Code generation dominates: look for big generic instantiations", "generic-code-and-monomorphization")
                .saw(format!("Codegen and LLVM took {} ({:.0}%).", secs(a.codegen_secs), pct(a.codegen_secs, total)))
                .why("Every generic function is compiled once per set of type arguments it's used with. One generic helper used with fifty types is fifty functions for LLVM.")
                .fix("Run `cargo llvm-lines` to find the functions with the most copies, then move the type-independent part into a non-generic inner function (`fn foo<T: AsRef<str>>(t: T) { inner(t.as_ref()) }`)."),
        );
    }

    if let Some(top) = a.heavy_bodies.first()
        && top.secs >= 2.0
        && pct(top.secs, a.wall_secs.max(1e-9)) >= 10.0
    {
        out.push(
            Finding::new("heavy-function-body", if pct(top.secs, a.wall_secs.max(1e-9)) >= 20.0 { Severity::Medium } else { Severity::Low }, "One function body takes a large share of type checking", "why-send-proofs-get-expensive")
                .saw(format!("Type and borrow checking `{}` took {} ({:.0}% of the wall time).", ellipsize(&top.name, 100), secs(top.secs), pct(top.secs, a.wall_secs)))
                .why("rustc checks one body at a time, and a long async fn with many awaits produces one huge state machine whose every captured type is checked together.")
                .fix("Split it into smaller functions, and box the futures of the expensive branches (`Box::pin(...)` or `.boxed()`), so each piece is checked on its own."),
        );
    }

    // Nothing above explains most of the time: it is the size of the crate.
    let explained = out.iter().any(|f| {
        matches!(
            f.id,
            "trait-solving-dominates"
                | "send-proofs-per-async-body"
                | "heavy-function-body"
                | "codegen-dominates"
        )
    });
    if !explained && let Some(f) = size_finding(a, crate_name, module_lines) {
        out.push(f);
    }
    out
}

/// When no body, type or macro stands out but the crate still takes long:
/// the cost is many ordinary bodies, and the lever is splitting the crate.
fn size_finding(
    a: &Analysis,
    crate_name: &str,
    module_lines: &[(String, usize)],
) -> Option<Finding> {
    let total = a.total_secs.max(1e-9);
    let body_share = pct(a.body_secs, total);
    let top_item = a
        .items
        .first()
        .map_or(0.0, |i| pct(i.secs, a.body_secs.max(1e-9)));
    if a.wall_secs < 10.0 || a.body_count < 100 || body_share < 50.0 || top_item >= 10.0 {
        return None;
    }
    let lines = |module: &str| {
        let name = module.rsplit("::").next().unwrap_or(module);
        module_lines
            .iter()
            .find(|(m, _)| m == name || m.strip_suffix(".rs") == Some(name))
            .map(|(_, n)| format!(", {n} lines"))
            .unwrap_or_default()
    };
    let top: Vec<&Cost> = a
        .modules
        .iter()
        .filter(|m| m.name.contains("::"))
        .take(3)
        .collect();
    if top.is_empty() {
        return None;
    }
    let mods: Vec<String> = top
        .iter()
        .map(|m| {
            format!(
                "`{}` {:.0}% ({}, {} items{})",
                m.name,
                pct(m.secs, total),
                secs(m.secs),
                m.count,
                lines(&m.name)
            )
        })
        .collect();
    let names: Vec<String> = top.iter().map(|m| format!("`{}`", m.name)).collect();
    Some(
        Finding::new(
            "check-time-is-crate-size",
            if a.wall_secs >= 20.0 { Severity::Medium } else { Severity::Low },
            format!(
                "{crate_name}'s {} check is spread over {} items: split it by module",
                secs(a.wall_secs),
                a.body_count
            ),
            "splitting-a-big-crate",
        )
        .saw(format!(
            "Trait solving is small ({}, {:.0}%) and no single body stands out (the biggest is {top_item:.1}% of body time). The cost is {} items whose bodies took {} of type checking, borrow checking and MIR building ({body_share:.0}% of the profile). By top-level module: {}.",
            secs(a.trait_solving_secs),
            pct(a.trait_solving_secs, total),
            a.body_count,
            secs(a.body_secs),
            mods.join(", ")
        ))
        .why("rustc's frontend checks a crate's bodies one after another on one thread, and an edit anywhere in the crate checks all of them again. With no hot spot, the time is proportional to the amount of code, and the lever is making each crate smaller.")
        .fix(format!(
            "Move the most expensive modules that don't need the rest of the crate into crates of their own, starting with {}. They then check in parallel with the rest, and edits elsewhere skip them. `cargo faster rdeps {crate_name}` shows the module sizes and what each crate rebuilds. Tradeoff: the pieces talk through a public API, and dependency cycles between modules have to be cut first.",
            names.join(", ")
        )),
    )
}

fn short_ty(ty: &str) -> String {
    ellipsize(ty, 70)
}

/// Text tables for an analysis.
#[must_use]
pub fn details(a: &Analysis) -> Vec<String> {
    let total = a.total_secs.max(1e-9);
    let row = |c: &Cost, name_max: usize| {
        vec![
            secs(c.secs),
            format!("{:.1}%", pct(c.secs, total)),
            c.count.to_string(),
            ellipsize(&c.name, name_max),
        ]
    };
    let costs = |title: String, head: &[&str], v: &[Cost], n: usize, name_max: usize| {
        let rows: Vec<Vec<String>> = v.iter().take(n).map(|c| row(c, name_max)).collect();
        format!("{title}\n{}", table(head, &rows))
    };
    let mut out = Vec::new();
    out.push(costs(
        "top queries by self time".to_owned(),
        &["self", "%", "count", "query"],
        &a.queries,
        12,
        60,
    ));
    if !a.trait_solving_by_category.is_empty() {
        out.push(costs(
            "trait solving by what is being proved".to_owned(),
            &["self", "%", "count", "category"],
            &a.trait_solving_by_category,
            10,
            60,
        ));
    }
    if !a.obligations.is_empty() {
        let rows: Vec<Vec<String>> = a
            .obligations
            .iter()
            .take(12)
            .map(|o| {
                vec![
                    secs(o.secs),
                    o.count.to_string(),
                    o.bodies.to_string(),
                    format!(
                        "{}: {}",
                        ellipsize(&o.self_ty, 90),
                        short_trait(&o.trait_path)
                    ),
                ]
            })
            .collect();
        out.push(format!(
            "most expensive obligations\n{}",
            table(&["self", "count", "bodies", "predicate"], &rows)
        ));
    }
    if !a.proving_bodies.is_empty() {
        out.push(costs(
            "bodies doing the most trait solving".to_owned(),
            &["self", "%", "preds", "body"],
            &a.proving_bodies,
            8,
            100,
        ));
    }
    if a.modules.len() > 1 {
        out.push(costs(
            format!(
                "time by module (bodies only; {} of the profile)",
                secs(a.body_secs)
            ),
            &["self", "%", "items", "module"],
            &a.modules,
            10,
            80,
        ));
    }
    if !a.items.is_empty() {
        out.push(costs(
            "bodies by total frontend time".to_owned(),
            &["time", "%", "queries", "item"],
            &a.items,
            10,
            100,
        ));
    }
    if !a.proc_macros.is_empty() {
        out.push(costs(
            "proc macro expansion".to_owned(),
            &["time", "%", "uses", "macro"],
            &a.proc_macros,
            8,
            60,
        ));
    }
    out
}

/// A section for one analyzed profile.
#[must_use]
pub fn section(a: &Analysis, crate_name: &str, module_lines: &[(String, usize)]) -> Section {
    let mut s = Section::new(format!("check-cost: {crate_name}"));
    s.fact(
        "profiled time",
        format!(
            "{} wall, {} across threads",
            secs(a.wall_secs),
            secs(a.total_secs)
        ),
    );
    s.fact(
        "trait solving",
        format!(
            "{} ({:.0}%)",
            secs(a.trait_solving_secs),
            pct(a.trait_solving_secs, a.total_secs)
        ),
    );
    let macros: f64 = a.proc_macros.iter().map(|m| m.secs).sum();
    s.fact(
        "proc macros",
        format!("{} ({:.0}%)", secs(macros), pct(macros, a.total_secs)),
    );
    if a.profiler_secs > 0.0 {
        s.fact(
            "profiler overhead",
            format!(
                "{} (self_profile_alloc_query_strings; left out of the times above and below)",
                secs(a.profiler_secs)
            ),
        );
    }
    if a.codegen_secs > 0.0 {
        s.fact(
            "codegen",
            format!(
                "{} ({:.0}%)",
                secs(a.codegen_secs),
                pct(a.codegen_secs, a.total_secs)
            ),
        );
    }
    s.details = details(a);
    s.extend(findings(a, crate_name, module_lines));
    s.data = serde_json::to_value(a).unwrap_or_default();
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustc_self_profile::lifetime_bound_env;

    fn pred(ty: &str, tr: &str) -> String {
        format!(
            "Canonical {{ value: ParamEnvAnd {{ param_env: ParamEnv {{ .. }}, value: Binder {{ value: TraitPredicate(<{ty} as {tr}>, polarity:Positive), bound_vars: [] }} }} }}"
        )
    }

    /// 60 async bodies, each proving `Arc<Bot>: Send` and `Bot: Sync`.
    fn send_heavy_profile() -> Profile {
        let mut p = Profile::default();
        let mut t = 0;
        for i in 0..60 {
            let body = format!("app::handlers::handler_{i}::{{closure#0}}");
            p.push(
                "check_coroutine_obligations",
                Some(&body),
                t,
                t + 50_000_000,
                0,
            );
            p.push(
                "evaluate_obligation",
                Some(&pred("std::sync::Arc<app::Bot>", "std::marker::Send")),
                t + 1_000_000,
                t + 21_000_000,
                0,
            );
            p.push(
                "evaluate_obligation",
                Some(&pred("app::Bot", "std::marker::Sync")),
                t + 2_000_000,
                t + 12_000_000,
                0,
            );
            p.push(
                "evaluate_obligation",
                Some(&pred(
                    &format!("{{coroutine witness@src/h.rs:{i}:1: 9:2}}"),
                    "std::marker::Send",
                )),
                t + 22_000_000,
                t + 30_000_000,
                0,
            );
            t += 50_000_000;
        }
        p.push(
            "expand_proc_macro",
            Some("serde_derive::Serialize"),
            t,
            t + 100_000_000,
            0,
        );
        p.push(
            "typeck",
            Some("app::big"),
            t + 100_000_000,
            t + 200_000_000,
            0,
        );
        p
    }

    #[test]
    fn attributes_send_proofs_to_types_and_bodies() {
        let a = analyze(&send_heavy_profile());
        assert!((a.wall_secs - 3.2).abs() < 1e-6, "{}", a.wall_secs);
        // Arc<Bot>: Send self time excludes the nested Bot: Sync proof.
        let arc = a
            .obligations
            .iter()
            .find(|o| o.self_ty == "std::sync::Arc<app::Bot>")
            .unwrap();
        assert_eq!(arc.count, 60);
        assert_eq!(arc.bodies, 60);
        assert!((arc.secs - 0.6).abs() < 1e-6, "{}", arc.secs);
        let bot = &a.heavy_types[0];
        assert_eq!(bot.ty, "app::Bot");
        assert!((bot.secs - 1.2).abs() < 1e-6);
        assert_eq!(bot.bodies, 60);
        assert!(a.heavy_types.iter().all(|h| !h.ty.contains("coroutine")));
        assert!(
            a.proving_bodies[0]
                .name
                .starts_with("app::handlers::handler_")
        );
        assert_eq!(a.proc_macros[0].name, "serde_derive::Serialize");
        assert_eq!(a.heavy_bodies[0].name, "app::big");
        assert!(
            a.trait_solving_by_category
                .iter()
                .any(|c| c.name == "types from app")
        );

        let f = findings(&a, "app", &[]);
        assert_eq!(f[0].id, "send-proofs-per-async-body");
        assert_eq!(f[0].severity, Severity::High);
        assert!(f[0].saw.contains("`app::Bot`"), "{}", f[0].saw);
        assert!(f[0].saw.contains("60 times"), "{}", f[0].saw);
    }

    #[test]
    fn async_trait_shaped_envs_are_attributed() {
        // Shapes from a real rustc 1.98 profile, shortened.
        let at = "CanonicalQueryInput { canonical: Canonical { value: ParamEnvAnd { param_env: ParamEnv { caller_bounds: [Binder { value: OutlivesPredicate('^c_1, '^c_0), bound_vars: [] }, Binder { value: OutlivesPredicate(Self/#0, '^c_0), bound_vars: [] }] }, value: Binder { value: TraitPredicate(<a::Bot as std::marker::Send>, polarity:Positive), bound_vars: [] } }, max_universe: U0, var_kinds: [Region(U0), Region(U0)] } }";
        let plain = "CanonicalQueryInput { canonical: Canonical { value: ParamEnvAnd { param_env: ParamEnv { caller_bounds: [] }, value: Binder { value: OutlivesPredicate('^c_1, '^c_0) } } } }";
        assert!(lifetime_bound_env(at));
        assert!(!lifetime_bound_env(plain));
        assert!(!lifetime_bound_env(&pred("a::Bot", "std::marker::Send")));
        assert!(lifetime_bound_env(&pred(
            "Pin<Box<dyn Future + Send + 'async_trait>>",
            "std::marker::Send"
        )));

        let mut p = Profile::default();
        p.push("evaluate_obligation", Some(at), 0, 2_000_000_000, 0);
        p.push(
            "evaluate_obligation",
            Some(at),
            2_000_000_000,
            3_000_000_000,
            0,
        );
        p.push(
            "evaluate_obligation",
            Some(plain),
            3_000_000_000,
            4_000_000_000,
            0,
        );
        let a = analyze(&p);
        assert_eq!(a.async_trait_obligations, 2);
        assert!((a.async_trait_secs - 3.0).abs() < 1e-6);
        assert!((a.trait_solving_secs - 4.0).abs() < 1e-6);
        let f = findings(&a, "a", &[]);
        assert!(
            f.iter()
                .any(|f| f.id == "async-trait-overhead" && f.severity == Severity::High)
        );
    }

    #[test]
    fn async_trait_severity_follows_measured_cost() {
        let sev = |at: f64| {
            let mut a = Analysis::default();
            a.total_secs = 40.0;
            a.trait_solving_secs = 30.0;
            a.async_trait_secs = at;
            a.async_trait_obligations = 100;
            findings(&a, "a", &[])
                .into_iter()
                .find(|f| f.id == "async-trait-overhead")
                .map(|f| f.severity)
        };
        assert_eq!(sev(0.1), None);
        assert_eq!(sev(0.5), Some(Severity::Low));
        assert_eq!(sev(1.5), Some(Severity::Medium));
        assert_eq!(sev(6.0), Some(Severity::High));
    }

    #[test]
    fn quiet_profile_has_no_findings() {
        let mut p = Profile::default();
        p.push("typeck", Some("a::f"), 0, 1_000_000, 0);
        p.push(
            "evaluate_obligation",
            Some(&pred("u8", "std::marker::Send")),
            10,
            20,
            0,
        );
        let a = analyze(&p);
        assert_eq!(findings(&a, "a", &[]), []);
        let s = section(&a, "a", &[]);
        assert!(s.details[0].contains("typeck"));
    }

    #[test]
    fn profiler_overhead_is_not_a_query() {
        let mut p = Profile::default();
        p.push("typeck_root", Some("a::f"), 0, 3_000_000_000, 0);
        // Without arguments: trait solving still counts.
        p.push("evaluate_obligation", None, 1_000_000_000, 2_000_000_000, 0);
        p.push(
            "self_profile_alloc_query_strings",
            None,
            3_000_000_000,
            4_000_000_000,
            0,
        );
        let a = analyze(&p);
        assert!((a.profiler_secs - 1.0).abs() < 1e-6, "{}", a.profiler_secs);
        assert!((a.wall_secs - 3.0).abs() < 1e-6, "{}", a.wall_secs);
        assert!((a.total_secs - 3.0).abs() < 1e-6, "{}", a.total_secs);
        assert!((a.trait_solving_secs - 1.0).abs() < 1e-6);
        assert!(
            a.queries
                .iter()
                .all(|q| !q.name.starts_with("self_profile"))
        );
        let s = section(&a, "a", &[]);
        let overhead = s
            .facts
            .iter()
            .find(|f| f.key == "profiler overhead")
            .unwrap();
        assert!(overhead.value.starts_with("1.00s"), "{}", overhead.value);
        assert!(!s.details[0].contains("self_profile"), "{}", s.details[0]);
    }

    /// `n` bodies of 10ms each per module, closures folded into their item.
    fn even_profile(mods: &[(&str, usize)]) -> Profile {
        let mut p = Profile::default();
        let mut t = 0;
        for (m, n) in mods {
            for i in 0..*n {
                let f = format!("app::{m}::f{i}");
                p.push("typeck_root", Some(&f), t, t + 6_000_000, 0);
                p.push(
                    "mir_borrowck",
                    Some(&format!("{f}::{{closure#0}}")),
                    t + 6_000_000,
                    t + 10_000_000,
                    0,
                );
                t += 10_000_000;
            }
        }
        p
    }

    #[test]
    fn spread_out_check_time_says_split_the_crate() {
        let a = analyze(&even_profile(&[("big", 1500), ("small", 600)]));
        let lines = [("big".to_owned(), 9000), ("small.rs".to_owned(), 2000)];
        let f = findings(&a, "app", &lines);
        let size = f
            .iter()
            .find(|f| f.id == "check-time-is-crate-size")
            .unwrap();
        assert_eq!(size.severity, Severity::Medium);
        let text = format!("{size:?}");
        assert!(text.contains("`app::big` 71%"), "{text}");
        assert!(text.contains("9000 lines"), "{text}");
        assert!(text.contains("2000 lines"), "{text}");

        // Small crates and crates with one hot body don't get it.
        let small = analyze(&even_profile(&[("big", 50)]));
        assert_eq!(findings(&small, "app", &[]).len(), 0);
        let mut p = even_profile(&[("big", 1500)]);
        p.push(
            "typeck_root",
            Some("app::hot"),
            15_000_000_000,
            21_000_000_000,
            0,
        );
        let hot = analyze(&p);
        let ids: Vec<_> = findings(&hot, "app", &[]).iter().map(|f| f.id).collect();
        assert!(!ids.contains(&"check-time-is-crate-size"), "{ids:?}");
    }
}
