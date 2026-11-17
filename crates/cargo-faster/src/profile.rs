//! Self-profile analysis: where rustc spent its time in one crate, which
//! trait obligations it proved over and over, and for which function bodies.
//!
//! [`Profile`] is a flat list of intervals, loaded from a `.mm_profdata`
//! file with [`Profile::load`] or built by hand in tests. [`analyze`] turns
//! it into an [`Analysis`], and [`findings`] into advice.

use crate::finding::{Finding, Section, Severity, table};
use crate::fmt::{ellipsize, pct, secs};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

/// Queries whose first argument names the function body being checked.
const BODY_LABELS: &[&str] = &[
    "check_coroutine_obligations",
    "typeck",
    "mir_borrowck",
    "check_well_formed",
    "mir_drops_elaborated_and_const_checked",
    "optimized_mir",
];

/// Queries whose arguments we keep (the rest are dropped to save memory).
const ARG_LABELS: &[&str] = &["evaluate_obligation", "expand_proc_macro", "codegen_module"];

/// Labels that mean LLVM or codegen work (a `cargo build` profile).
const CODEGEN_PREFIXES: &[&str] = &["LLVM_", "codegen_", "monomorphization_collector", "link"];

/// One timed event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    /// Index into [`Profile::labels`].
    pub label: u32,
    /// Index into [`Profile::args`], if the event had a kept argument.
    pub arg: Option<u32>,
    /// Start, in nanoseconds since the profile started.
    pub start: u64,
    /// End, in nanoseconds since the profile started.
    pub end: u64,
    /// rustc thread id.
    pub thread: u32,
}

/// The events of one rustc invocation.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// Interned event labels (query names).
    pub labels: Vec<String>,
    /// Interned first arguments (query keys) of kept events.
    pub args: Vec<String>,
    /// Every interval event.
    pub intervals: Vec<Interval>,
    /// The rustc command line, from the profile's metadata.
    pub cmd: String,
    label_ids: HashMap<String, u32>,
    arg_ids: HashMap<String, u32>,
}

impl Profile {
    /// Load a `.mm_profdata` file written by `-Zself-profile`.
    ///
    /// # Errors
    ///
    /// When the file is missing or not a profile measureme can read.
    pub fn load(path: &Path) -> Result<Self, String> {
        let data = analyzeme::ProfilingData::new(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let start_time = data.metadata().start_time;
        let mut profile = Self {
            cmd: data.metadata().cmd.clone(),
            ..Self::default()
        };
        let ns = |t: SystemTime| {
            t.duration_since(start_time)
                .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        };
        for event in data.iter_full() {
            let analyzeme::EventPayload::Timestamp(analyzeme::Timestamp::Interval { start, end }) =
                event.payload
            else {
                continue;
            };
            let keep_arg =
                ARG_LABELS.contains(&&*event.label) || BODY_LABELS.contains(&&*event.label);
            let arg = if keep_arg {
                event.additional_data.first().map(|a| &**a)
            } else {
                None
            };
            profile.push(&event.label, arg, ns(start), ns(end), event.thread_id);
        }
        Ok(profile)
    }

    /// Add an interval (used by [`Profile::load`] and tests).
    pub fn push(&mut self, label: &str, arg: Option<&str>, start: u64, end: u64, thread: u32) {
        let label = intern(&mut self.labels, &mut self.label_ids, label);
        let arg = arg.map(|a| intern(&mut self.args, &mut self.arg_ids, a));
        self.intervals.push(Interval {
            label,
            arg,
            start,
            end,
            thread,
        });
    }

    fn label(&self, i: &Interval) -> &str {
        &self.labels[i.label as usize]
    }

    fn arg(&self, i: &Interval) -> Option<&str> {
        i.arg.map(|a| self.args[a as usize].as_str())
    }
}

fn intern(strings: &mut Vec<String>, ids: &mut HashMap<String, u32>, s: &str) -> u32 {
    if let Some(&id) = ids.get(s) {
        return id;
    }
    let id = u32::try_from(strings.len()).unwrap_or(u32::MAX);
    strings.push(s.to_owned());
    ids.insert(s.to_owned(), id);
    id
}

/// Time and count for one key.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Cost {
    /// What was timed (query name, predicate, macro, body).
    pub name: String,
    /// Seconds.
    pub secs: f64,
    /// Occurrences.
    pub count: usize,
}

/// A trait obligation and what proving it cost.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Obligation {
    /// The type being proved, e.g. `tala_agent::bot::Bot`.
    pub self_ty: String,
    /// The trait, e.g. `std::marker::Send`.
    pub trait_path: String,
    /// Self time of `evaluate_obligation` for this predicate, in seconds.
    pub secs: f64,
    /// Times it was evaluated.
    pub count: usize,
    /// Distinct function bodies that needed it.
    pub bodies: usize,
}

/// A type whose auto-trait proofs (`Send`, `Sync`) add up, through every
/// wrapper around it (`Arc<T>`, `Weak<T>`, `Option<T>`, ...).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HeavyType {
    /// The innermost type, e.g. `tala_agent::bot::Bot`.
    pub ty: String,
    /// Total self time of the proofs, in seconds.
    pub secs: f64,
    /// The largest number of times one of its predicates was evaluated.
    pub max_count: usize,
    /// Distinct function bodies that needed it.
    pub bodies: usize,
    /// The predicates, most expensive first (up to 6).
    pub predicates: Vec<String>,
    /// Bodies that paid the most for it (up to 5).
    pub top_bodies: Vec<Cost>,
}

/// What a profile says.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Analysis {
    /// Wall time of the main thread, in seconds.
    pub wall_secs: f64,
    /// Sum of all self time, in seconds (more than wall with threads).
    pub total_secs: f64,
    /// Self time by query, most expensive first.
    pub queries: Vec<Cost>,
    /// Self time of `evaluate_obligation`, in seconds.
    pub trait_solving_secs: f64,
    /// Obligations, most expensive first (up to 40).
    pub obligations: Vec<Obligation>,
    /// Trait solving by the crate of the type being proved, or
    /// `async bodies` / `impl Future`.
    pub trait_solving_by_category: Vec<Cost>,
    /// Types whose `Send`/`Sync` proofs add up.
    pub heavy_types: Vec<HeavyType>,
    /// Bodies that did the most trait solving.
    pub proving_bodies: Vec<Cost>,
    /// Time to prove obligations whose parameter environment has
    /// lifetime-outlives bounds (`'life0: 'async_trait`), in seconds: the
    /// shape `#[async_trait]` gives every method.
    pub async_trait_secs: f64,
    /// How many such obligations were evaluated.
    pub async_trait_obligations: usize,
    /// Proc macro expansion, inclusive time by macro.
    pub proc_macros: Vec<Cost>,
    /// Type checking and borrow checking, inclusive time by body.
    pub heavy_bodies: Vec<Cost>,
    /// Self time of codegen and LLVM, in seconds.
    pub codegen_secs: f64,
}

/// Nanoseconds spent per enclosing body (`None`: outside any body).
type ByBody = HashMap<Option<u32>, u64>;

/// One predicate, merged across canonical keys.
struct Pred {
    ns: u64,
    count: usize,
    bodies: ByBody,
}

/// Per root type: time, max count, time per body, `(time, predicate)`.
type HeavyAcc = (u64, usize, ByBody, Vec<(u64, String)>);

/// Aggregate a profile.
///
/// One pass over the intervals computes self time and the enclosing body;
/// the rest groups that by query, predicate, type, body and macro.
#[must_use]
#[allow(
    clippy::too_many_lines,
    clippy::many_single_char_names,
    reason = "one linear pipeline; splitting it scatters the shared tables"
)]
pub fn analyze(p: &Profile) -> Analysis {
    let mut by_thread: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, iv) in p.intervals.iter().enumerate() {
        by_thread.entry(iv.thread).or_default().push(i);
    }
    let n = p.intervals.len();
    let mut child = vec![0u64; n];
    let mut body_of: Vec<Option<u32>> = vec![None; n];
    let body_label_ids: Vec<u32> = BODY_LABELS
        .iter()
        .filter_map(|l| p.label_ids.get(*l).copied())
        .collect();
    let mut wall = 0u64;
    for idxs in by_thread.values_mut() {
        idxs.sort_by_key(|&i| (p.intervals[i].start, core::cmp::Reverse(p.intervals[i].end)));
        if let (Some(&first), Some(last)) =
            (idxs.first(), idxs.iter().map(|&i| p.intervals[i].end).max())
        {
            wall = wall.max(last - p.intervals[first].start.min(last));
        }
        // Stack of (interval index, nearest enclosing body arg).
        let mut stack: Vec<(usize, Option<u32>)> = Vec::new();
        for &i in idxs.iter() {
            let iv = p.intervals[i];
            while stack
                .last()
                .is_some_and(|&(s, _)| p.intervals[s].end <= iv.start)
            {
                stack.pop();
            }
            let enclosing = stack.last().and_then(|&(_, b)| b);
            if let Some(&(parent, _)) = stack.last() {
                child[parent] += iv.end.min(p.intervals[parent].end) - iv.start;
            }
            body_of[i] = enclosing;
            let here = if body_label_ids.contains(&iv.label) && iv.arg.is_some() {
                iv.arg
            } else {
                enclosing
            };
            stack.push((i, here));
        }
    }
    let self_ns = |i: usize| (p.intervals[i].end - p.intervals[i].start).saturating_sub(child[i]);
    let s = |ns: u64| ns as f64 / 1e9;

    let mut queries: HashMap<&str, (u64, usize)> = HashMap::new();
    let mut total = 0u64;
    let mut codegen = 0u64;
    let mut preds: HashMap<u32, (u64, usize, ByBody)> = HashMap::new();
    let mut macros: HashMap<&str, (u64, usize)> = HashMap::new();
    let mut bodies: HashMap<&str, (u64, usize)> = HashMap::new();
    for (i, iv) in p.intervals.iter().enumerate() {
        let label = p.label(iv);
        let st = self_ns(i);
        total += st;
        let q = queries.entry(label).or_default();
        q.0 += st;
        q.1 += 1;
        if CODEGEN_PREFIXES.iter().any(|c| label.starts_with(c)) {
            codegen += st;
        }
        match label {
            "evaluate_obligation" => {
                if let Some(a) = iv.arg {
                    let e = preds.entry(a).or_default();
                    e.0 += st;
                    e.1 += 1;
                    *e.2.entry(body_of[i]).or_default() += st;
                }
            }
            "expand_proc_macro" => {
                let name = p.arg(iv).unwrap_or("?");
                let e = macros.entry(name).or_default();
                e.0 += iv.end - iv.start;
                e.1 += 1;
            }
            "typeck" | "mir_borrowck" => {
                // Inclusive time, but only for the outermost of nested
                // body queries (closures are checked with their parent).
                if body_of[i].is_none()
                    && let Some(def) = p.arg(iv)
                {
                    let e = bodies.entry(def).or_default();
                    e.0 += iv.end - iv.start;
                    e.1 += 1;
                }
            }
            _ => {}
        }
    }

    let mut a = Analysis {
        wall_secs: s(wall),
        total_secs: s(total),
        codegen_secs: s(codegen),
        ..Analysis::default()
    };
    a.queries = sorted(queries.into_iter().map(|(k, (t, c))| Cost {
        name: k.to_owned(),
        secs: s(t),
        count: c,
    }));

    // Group by parsed predicate (different canonical keys can print to the
    // same predicate).
    let mut by_pred: HashMap<(String, String), Pred> = HashMap::new();
    for (arg, (ns, count, bodies)) in preds {
        let raw = &p.args[arg as usize];
        if lifetime_bound_env(raw) {
            a.async_trait_secs += s(ns);
            a.async_trait_obligations += count;
        }
        let (ty, tr) = parse_predicate(raw);
        let e = by_pred.entry((ty, tr)).or_insert(Pred {
            ns: 0,
            count: 0,
            bodies: HashMap::new(),
        });
        e.ns += ns;
        e.count += count;
        for (b, t) in bodies {
            *e.bodies.entry(b).or_default() += t;
        }
    }
    let body_name = |b: Option<u32>| {
        b.map_or_else(
            || "(outside any body)".to_owned(),
            |b| p.args[b as usize].clone(),
        )
    };
    let mut categories: HashMap<String, (u64, usize)> = HashMap::new();
    let mut proving: HashMap<Option<u32>, (u64, usize)> = HashMap::new();
    let mut heavy: HashMap<String, HeavyAcc> = HashMap::new();
    let mut obligations = Vec::new();
    for ((ty, tr), pred) in &by_pred {
        a.trait_solving_secs += s(pred.ns);
        let c = categories.entry(category(ty)).or_default();
        c.0 += pred.ns;
        c.1 += pred.count;
        for (b, t) in &pred.bodies {
            let e = proving.entry(*b).or_default();
            e.0 += t;
            e.1 += 1;
        }
        if is_auto_trait(tr) && !is_async_body(ty) {
            let root = root_type(ty);
            let e = heavy.entry(root.to_owned()).or_default();
            e.0 += pred.ns;
            e.1 = e.1.max(pred.count);
            for (b, t) in &pred.bodies {
                *e.2.entry(*b).or_default() += t;
            }
            e.3.push((pred.ns, format!("{ty}: {}", short_trait(tr))));
        }
        obligations.push(Obligation {
            self_ty: ty.clone(),
            trait_path: tr.clone(),
            secs: s(pred.ns),
            count: pred.count,
            bodies: pred.bodies.len(),
        });
    }
    obligations.sort_by(|x, y| y.secs.total_cmp(&x.secs));
    obligations.truncate(40);
    a.obligations = obligations;
    a.trait_solving_by_category = sorted(categories.into_iter().map(|(k, (t, c))| Cost {
        name: k,
        secs: s(t),
        count: c,
    }));
    a.proving_bodies = sorted(proving.into_iter().map(|(b, (t, c))| Cost {
        name: body_name(b),
        secs: s(t),
        count: c,
    }));
    a.proving_bodies.truncate(20);
    let mut heavy: Vec<HeavyType> = heavy
        .into_iter()
        .map(|(ty, (ns, max_count, bodies, mut preds))| {
            preds.sort_by_key(|x| core::cmp::Reverse(x.0));
            let mut top_bodies = sorted(bodies.iter().map(|(b, t)| Cost {
                name: body_name(*b),
                secs: s(*t),
                count: 1,
            }));
            top_bodies.truncate(5);
            HeavyType {
                ty,
                secs: s(ns),
                max_count,
                bodies: bodies.len(),
                predicates: preds.into_iter().take(6).map(|(_, p)| p).collect(),
                top_bodies,
            }
        })
        .collect();
    heavy.sort_by(|x, y| y.secs.total_cmp(&x.secs));
    heavy.truncate(10);
    a.heavy_types = heavy;
    a.proc_macros = sorted(macros.into_iter().map(|(k, (t, c))| Cost {
        name: k.to_owned(),
        secs: s(t),
        count: c,
    }));
    a.heavy_bodies = sorted(bodies.into_iter().map(|(k, (t, c))| Cost {
        name: k.to_owned(),
        secs: s(t),
        count: c,
    }));
    a.heavy_bodies.truncate(15);
    a
}

fn sorted(it: impl Iterator<Item = Cost>) -> Vec<Cost> {
    let mut v: Vec<Cost> = it.collect();
    v.sort_by(|x, y| y.secs.total_cmp(&x.secs).then_with(|| x.name.cmp(&y.name)));
    v
}

/// Split an `evaluate_obligation` key into the self type and the trait.
///
/// Keys look like `Canonical { .. value: TraitPredicate(<T as Trait>,
/// polarity:Positive) .. }`; anything else comes back whole as the type
/// with an empty trait.
#[must_use]
pub fn parse_predicate(key: &str) -> (String, String) {
    let Some(i) = key.find("TraitPredicate(<") else {
        return (key.to_owned(), String::new());
    };
    let rest = &key[i + "TraitPredicate(<".len()..];
    let inner = rest.find(", polarity").map_or(rest, |j| &rest[..j]);
    let inner = inner.strip_suffix('>').unwrap_or(inner);
    // The last top-level " as " separates the type from the trait.
    let mut depth = 0i32;
    let mut split = None;
    let bytes = inner.as_bytes();
    for (j, &b) in bytes.iter().enumerate() {
        match b {
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' | b')' | b']' | b'}' => depth -= 1,
            b' ' if depth == 0 && inner[j..].starts_with(" as ") => split = Some(j),
            _ => {}
        }
    }
    match split {
        Some(j) => (inner[..j].to_owned(), inner[j + 4..].to_owned()),
        None => (inner.to_owned(), String::new()),
    }
}

fn is_auto_trait(tr: &str) -> bool {
    matches!(
        tr,
        "std::marker::Send" | "std::marker::Sync" | "core::marker::Send" | "core::marker::Sync"
    )
}

fn short_trait(tr: &str) -> &str {
    tr.rsplit("::").next().unwrap_or(tr)
}

/// Whether a canonical `evaluate_obligation` key was proven in a parameter
/// environment with region-outlives bounds, or names `'async_trait`.
///
/// `#[async_trait]` gives each method `where 'life0: 'async_trait, ...`.
/// Canonicalization renames those regions (`OutlivesPredicate('^c_1,
/// '^c_0)`), so the printed predicate no longer says `'async_trait`, but
/// the bounds stay in the key, and every method's key is different. That
/// is why rustc can't reuse one method's answers in the next. Hand-written
/// `'a: 'b` bounds match too; they are rare in async code.
#[must_use]
pub fn lifetime_bound_env(raw: &str) -> bool {
    if raw.contains("'async_trait") {
        return true;
    }
    raw.split(", value: Binder")
        .next()
        .is_some_and(|env| env.contains("caller_bounds") && env.contains("OutlivesPredicate('"))
}

fn is_async_body(ty: &str) -> bool {
    ty.contains("coroutine witness@")
        || ty.contains("async block@")
        || ty.contains("async fn body")
        || ty.starts_with("{coroutine")
        || ty.starts_with("{async")
}

/// Wrappers whose auto traits come from their contents.
const WRAPPERS: &[&str] = &[
    "std::sync::Arc",
    "alloc::sync::Arc",
    "std::sync::Weak",
    "alloc::sync::Weak",
    "std::option::Option",
    "core::option::Option",
    "std::sync::OnceLock",
    "std::sync::Mutex",
    "std::sync::RwLock",
    "std::boxed::Box",
    "alloc::boxed::Box",
    "std::cell::RefCell",
    "std::rc::Rc",
    "std::vec::Vec",
    "alloc::vec::Vec",
    "tokio::sync::Mutex",
    "tokio::sync::RwLock",
    "tokio::sync::OnceCell",
    "std::sync::LazyLock",
    "once_cell::sync::OnceCell",
    "std::pin::Pin",
    "core::pin::Pin",
    "core::cell::UnsafeCell",
    "std::cell::UnsafeCell",
    "std::mem::ManuallyDrop",
];

/// The innermost interesting type: `std::sync::Arc<std::option::Option<a::B>>`
/// is `a::B`, `&a::B` is `a::B`.
#[must_use]
pub fn root_type(ty: &str) -> &str {
    let mut t = ty.trim();
    loop {
        if let Some(rest) = t.strip_prefix('&') {
            t = rest.strip_prefix("mut ").unwrap_or(rest).trim_start();
            continue;
        }
        let Some(open) = t.find('<') else { return t };
        if !t.ends_with('>') || !WRAPPERS.contains(&&t[..open]) {
            return t;
        }
        let inner = &t[open + 1..t.len() - 1];
        // Only single-argument wrappers (skip `Mutex<T, R>`-style
        // allocator or raw-lock parameters by taking the first argument).
        t = first_arg(inner);
    }
}

fn first_arg(s: &str) -> &str {
    let mut depth = 0i32;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' | b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => return s[..i].trim(),
            _ => {}
        }
    }
    s.trim()
}

/// `async bodies`, `impl Future`, or `type:<crate>` for the type's crate.
fn category(ty: &str) -> String {
    if is_async_body(ty) {
        let crate_name = ty.split_once('@').map_or("?", |(_, rest)| {
            rest.trim_start_matches("crates/")
                .split(['/', ':'])
                .next()
                .unwrap_or("?")
        });
        return format!("async bodies in {crate_name}");
    }
    if ty.starts_with("impl ") || ty.starts_with("{opaque") {
        return "opaque impl Future".to_owned();
    }
    let root = root_type(ty);
    let root = root
        .trim_start_matches(['(', '[', '&', '*'])
        .trim_start_matches("dyn ");
    let krate = root.split("::").next().unwrap_or(root);
    if krate.contains(['<', ' ', '(']) || krate.is_empty() {
        return "other".to_owned();
    }
    format!("types from {krate}")
}

/// Thresholds for the findings, as a share of the profiled time.
const SEND_SHARE: f64 = 2.0;

/// Turn an analysis into findings.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "a flat list of independent findings reads best in one place"
)]
pub fn findings(a: &Analysis, crate_name: &str) -> Vec<Finding> {
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
    out
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
    let mut out = Vec::new();
    out.push(format!(
        "top queries by self time\n{}",
        table(
            &["self", "%", "count", "query"],
            &a.queries
                .iter()
                .take(12)
                .map(|c| row(c, 60))
                .collect::<Vec<_>>()
        )
    ));
    if !a.trait_solving_by_category.is_empty() {
        out.push(format!(
            "trait solving by what is being proved\n{}",
            table(
                &["self", "%", "count", "category"],
                &a.trait_solving_by_category
                    .iter()
                    .take(10)
                    .map(|c| row(c, 60))
                    .collect::<Vec<_>>()
            )
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
        out.push(format!(
            "bodies doing the most trait solving\n{}",
            table(
                &["self", "%", "preds", "body"],
                &a.proving_bodies
                    .iter()
                    .take(8)
                    .map(|c| row(c, 100))
                    .collect::<Vec<_>>()
            )
        ));
    }
    if !a.proc_macros.is_empty() {
        out.push(format!(
            "proc macro expansion\n{}",
            table(
                &["time", "%", "uses", "macro"],
                &a.proc_macros
                    .iter()
                    .take(8)
                    .map(|c| row(c, 60))
                    .collect::<Vec<_>>()
            )
        ));
    }
    out
}

/// A section for one analyzed profile.
#[must_use]
pub fn section(a: &Analysis, crate_name: &str) -> Section {
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
    s.extend(findings(a, crate_name));
    s.data = serde_json::to_value(a).unwrap_or_default();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pred(ty: &str, tr: &str) -> String {
        format!(
            "Canonical {{ value: ParamEnvAnd {{ param_env: ParamEnv {{ .. }}, value: Binder {{ value: TraitPredicate(<{ty} as {tr}>, polarity:Positive), bound_vars: [] }} }} }}"
        )
    }

    #[test]
    fn parses_predicates() {
        assert_eq!(
            parse_predicate(&pred("a::Bot", "std::marker::Send")),
            ("a::Bot".into(), "std::marker::Send".into())
        );
        let (ty, tr) = parse_predicate(&pred("<a::X as b::Y>::Out", "std::marker::Sync"));
        assert_eq!(
            (ty.as_str(), tr.as_str()),
            ("<a::X as b::Y>::Out", "std::marker::Sync")
        );
        assert_eq!(parse_predicate("whatever").1, "");
    }

    #[test]
    fn finds_root_types() {
        assert_eq!(
            root_type("std::sync::Arc<std::option::Option<a::Bot>>"),
            "a::Bot"
        );
        assert_eq!(root_type("&std::sync::Weak<a::Bot>"), "a::Bot");
        assert_eq!(
            root_type("std::collections::HashMap<a::K, a::V>"),
            "std::collections::HashMap<a::K, a::V>"
        );
        assert_eq!(
            category("std::sync::Arc<matrix_sdk::Client>"),
            "types from matrix_sdk"
        );
        assert_eq!(
            category("{coroutine witness@crates/bin/src/main.rs:3:1: 9:2}"),
            "async bodies in bin"
        );
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

        let f = findings(&a, "app");
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
        let f = findings(&a, "a");
        assert!(
            f.iter()
                .any(|f| f.id == "async-trait-overhead" && f.severity == Severity::High)
        );
    }

    #[test]
    fn async_trait_severity_follows_measured_cost() {
        let sev = |at: f64| {
            let a = Analysis {
                total_secs: 40.0,
                trait_solving_secs: 30.0,
                async_trait_secs: at,
                async_trait_obligations: 100,
                ..Analysis::default()
            };
            findings(&a, "a")
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
        assert_eq!(findings(&a, "a"), []);
        let s = section(&a, "a");
        assert!(s.details[0].contains("typeck"));
    }

    #[test]
    fn threads_are_separate_stacks() {
        let mut p = Profile::default();
        p.push("outer", None, 0, 100, 0);
        p.push("inner", None, 10, 20, 1);
        let a = analyze(&p);
        let outer = a.queries.iter().find(|q| q.name == "outer").unwrap();
        assert!((outer.secs - 100e-9).abs() < 1e-12);
    }
}
