//! Analyze rustc's self-profile (`-Zself-profile`): where the compiler
//! spent its time in one crate, which function bodies and modules cost the
//! most to type check and borrow check, which trait obligations it proved
//! over and over, and how much of the profile is the profiler's own work.
//!
//! [`Profile`] is a flat list of intervals: loaded from a `.mm_profdata`
//! file with [`Profile::load`] (the `std` feature), deserialized, or built
//! by hand with [`Profile::push`]. [`analyze`] turns it into an
//! [`Analysis`]. Both are serde types.
//!
//! A profile with query arguments comes from
//!
//! ```text
//! RUSTC_BOOTSTRAP=1 rustc ... -Zself-profile=<dir> -Zself-profile-events=default,args
//! ```
//!
//! (`RUSTC_BOOTSTRAP=1` lets a stable compiler take `-Z` flags; set it on
//! that one invocation). Without `args`, per-body and per-obligation
//! numbers are empty, and the query table still works.
//!
//! # Example
//!
//! ```
//! use rustc_self_profile::{Profile, analyze};
//!
//! let mut p = Profile::default();
//! // label, first argument, start ns, end ns, thread
//! p.push("typeck", Some("app::server::handle"), 0, 3_000_000, 0);
//! p.push(
//!     "evaluate_obligation",
//!     Some("Canonical { value: TraitPredicate(<app::State as std::marker::Send>, polarity:Positive) }"),
//!     100_000,
//!     2_100_000,
//!     0,
//! );
//! p.push("typeck", Some("app::util::add"), 3_000_000, 4_000_000, 0);
//!
//! let a = analyze(&p);
//! assert_eq!(a.body_count, 2);
//! assert_eq!(a.items[0].name, "app::server::handle");
//! assert_eq!(a.modules[0].name, "app::server");
//! assert_eq!(a.obligations[0].self_ty, "app::State");
//! assert!((a.trait_solving_secs - 0.002).abs() < 1e-9);
//! ```
//!
//! # Features
//!
//! The crate is `#![no_std]` and needs `alloc`. The default `std` feature
//! adds [`Profile::load`], which reads `.mm_profdata` files with
//! [analyzeme](https://crates.io/crates/analyzeme) (std-only).

#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

#[cfg(feature = "std")]
mod load;
#[cfg(feature = "std")]
pub use load::LoadError;

/// Queries whose first argument names the function body being checked.
pub const BODY_LABELS: &[&str] = &[
    "check_coroutine_obligations",
    "typeck",
    "typeck_root",
    "mir_borrowck",
    "mir_built",
    "mir_promoted",
    "thir_body",
    "check_well_formed",
    "mir_drops_elaborated_and_const_checked",
    "optimized_mir",
];

/// Other queries whose first argument [`Profile::load`] keeps (the rest
/// are dropped to save memory).
pub const ARG_LABELS: &[&str] = &["evaluate_obligation", "expand_proc_macro", "codegen_module"];

/// Events the profiler records about itself. They exist only because of
/// `-Zself-profile` (`self_profile_alloc_query_strings` turns the recorded
/// query keys into strings at the end of the session, and grows with
/// `-Zself-profile-events=args`), so they are left out of the query table,
/// the wall time and every share, and reported once as profiler overhead.
#[must_use]
pub fn is_profiler_event(label: &str) -> bool {
    label.starts_with("self_profile")
}

/// Labels that mean LLVM or codegen work (a `cargo build` profile).
pub const CODEGEN_PREFIXES: &[&str] = &["LLVM_", "codegen_", "monomorphization_collector", "link"];

/// One timed event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
///
/// Serializes as `labels`, `args`, `intervals` and `cmd`; the lookup tables
/// [`Profile::push`] uses are rebuilt on deserialize.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "RawProfile")]
pub struct Profile {
    /// Interned event labels (query names).
    pub labels: Vec<String>,
    /// Interned first arguments (query keys) of kept events.
    pub args: Vec<String>,
    /// Every interval event.
    pub intervals: Vec<Interval>,
    /// The rustc command line, from the profile's metadata.
    pub cmd: String,
    #[serde(skip)]
    label_ids: HashMap<String, u32>,
    #[serde(skip)]
    arg_ids: HashMap<String, u32>,
}

#[derive(Deserialize)]
struct RawProfile {
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    intervals: Vec<Interval>,
    #[serde(default)]
    cmd: String,
}

impl From<RawProfile> for Profile {
    fn from(r: RawProfile) -> Self {
        let ids = |v: &[String]| {
            v.iter()
                .enumerate()
                .map(|(i, s)| (s.clone(), u32::try_from(i).unwrap_or(u32::MAX)))
                .collect()
        };
        Self {
            label_ids: ids(&r.labels),
            arg_ids: ids(&r.args),
            labels: r.labels,
            args: r.args,
            intervals: r.intervals,
            cmd: r.cmd,
        }
    }
}

impl Profile {
    /// Whether [`Profile::load`] keeps the first argument of events with
    /// this label: body queries and [`ARG_LABELS`].
    #[must_use]
    pub fn keeps_arg(label: &str) -> bool {
        ARG_LABELS.contains(&label) || BODY_LABELS.contains(&label)
    }

    /// Add an interval. Labels and arguments are interned.
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

    /// An interval's label.
    #[must_use]
    pub fn label(&self, i: &Interval) -> &str {
        &self.labels[i.label as usize]
    }

    /// An interval's kept argument.
    #[must_use]
    pub fn arg(&self, i: &Interval) -> Option<&str> {
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Cost {
    /// What was timed (query name, predicate, macro, body).
    pub name: String,
    /// Seconds.
    pub secs: f64,
    /// Occurrences.
    pub count: usize,
}

/// A trait obligation and what proving it cost.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Obligation {
    /// The type being proved, e.g. `app::State`.
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HeavyType {
    /// The innermost type, e.g. `app::State`.
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

/// What a profile says. Lists are sorted, most expensive first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Analysis {
    /// Wall time of the main thread, in seconds, without profiler overhead.
    pub wall_secs: f64,
    /// Sum of all self time, in seconds (more than wall with threads),
    /// without profiler overhead.
    pub total_secs: f64,
    /// Time the profiler spent on itself (`self_profile_alloc_query_strings`),
    /// in seconds. Not part of `wall_secs`, `total_secs` or `queries`.
    pub profiler_secs: f64,
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
    /// Self time of every query run for some function body (type checking,
    /// borrow checking, MIR building, the trait solving they asked for), in
    /// seconds. The rest is crate-wide work: expansion, lints, metadata.
    pub body_secs: f64,
    /// Distinct items (functions, methods, constants) whose bodies were
    /// checked; closures and async blocks count with their item.
    pub body_count: usize,
    /// Body time by item, most expensive first (up to 20). `count` is the
    /// number of body queries.
    pub items: Vec<Cost>,
    /// Body time by top-level module (`krate::module`, or `krate` for items
    /// at the crate root), most expensive first. `count` is the number of
    /// items.
    pub modules: Vec<Cost>,
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
    // The body an interval works for: its own, or the enclosing one.
    let mut owner: Vec<Option<u32>> = vec![None; n];
    let body_label_ids: Vec<u32> = (0..p.labels.len())
        .filter(|&l| BODY_LABELS.contains(&p.labels[l].as_str()))
        .map(|l| u32::try_from(l).unwrap_or(u32::MAX))
        .collect();
    let profiler: Vec<bool> = p.labels.iter().map(|l| is_profiler_event(l)).collect();
    let is_profiler = |iv: &Interval| profiler[iv.label as usize];
    let mut wall = 0u64;
    for idxs in by_thread.values_mut() {
        idxs.sort_by_key(|&i| (p.intervals[i].start, core::cmp::Reverse(p.intervals[i].end)));
        if let (Some(&first), Some(last)) =
            (idxs.first(), idxs.iter().map(|&i| p.intervals[i].end).max())
        {
            // The profiler's own work (near the end) is not the crate's.
            let overhead: u64 = idxs
                .iter()
                .map(|&i| p.intervals[i])
                .filter(is_profiler)
                .map(|iv| iv.end - iv.start)
                .sum();
            wall = wall.max((last - p.intervals[first].start.min(last)).saturating_sub(overhead));
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
            owner[i] = here;
            stack.push((i, here));
        }
    }
    let self_ns = |i: usize| (p.intervals[i].end - p.intervals[i].start).saturating_sub(child[i]);
    let s = |ns: u64| ns as f64 / 1e9;

    let mut queries: HashMap<&str, (u64, usize)> = HashMap::new();
    let mut total = 0u64;
    let mut codegen = 0u64;
    let mut profiler_ns = 0u64;
    let mut preds: HashMap<u32, (u64, usize, ByBody)> = HashMap::new();
    let mut macros: HashMap<&str, (u64, usize)> = HashMap::new();
    let mut bodies: HashMap<&str, (u64, usize)> = HashMap::new();
    let mut per_body: HashMap<u32, (u64, usize)> = HashMap::new();
    for (i, iv) in p.intervals.iter().enumerate() {
        let label = p.label(iv);
        let st = self_ns(i);
        if is_profiler(iv) {
            profiler_ns += st;
            continue;
        }
        total += st;
        let q = queries.entry(label).or_default();
        q.0 += st;
        q.1 += 1;
        if CODEGEN_PREFIXES.iter().any(|c| label.starts_with(c)) {
            codegen += st;
        }
        if let Some(b) = owner[i] {
            let e = per_body.entry(b).or_default();
            e.0 += st;
            e.1 += usize::from(owner[i] == iv.arg && body_label_ids.contains(&iv.label));
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
            "typeck" | "typeck_root" | "mir_borrowck" => {
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
        profiler_secs: s(profiler_ns),
        // All of evaluate_obligation, also when the profile has no
        // arguments to group it by.
        trait_solving_secs: queries.get("evaluate_obligation").map_or(0.0, |q| s(q.0)),
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

    // Body time by item, then by module.
    let mut items: HashMap<&str, (u64, usize)> = HashMap::new();
    for (b, (ns, count)) in per_body {
        a.body_secs += s(ns);
        let e = items.entry(body_item(&p.args[b as usize])).or_default();
        e.0 += ns;
        e.1 += count;
    }
    a.body_count = items.len();
    let mut modules: HashMap<&str, (u64, usize)> = HashMap::new();
    for (item, (ns, _)) in &items {
        let e = modules.entry(module_of(item)).or_default();
        e.0 += ns;
        e.1 += 1;
    }
    a.modules = sorted(modules.into_iter().map(|(k, (t, c))| Cost {
        name: k.to_owned(),
        secs: s(t),
        count: c,
    }));
    a.items = sorted(items.into_iter().map(|(k, (t, c))| Cost {
        name: k.to_owned(),
        secs: s(t),
        count: c,
    }));
    a.items.truncate(20);
    a
}

/// The item a body belongs to: `a::f::{{closure}}#1` is `a::f`. Closures,
/// async blocks and anonymous constants are checked with their item.
#[must_use]
pub fn body_item(def: &str) -> &str {
    let mut end = def.len();
    let mut at = 0;
    for seg in def.split("::") {
        let anon =
            seg.starts_with('{') && !seg.starts_with("{{impl}}") && !seg.starts_with("{impl");
        if anon && at > 0 {
            end = at - 2;
            break;
        }
        at += seg.len() + 2;
    }
    &def[..end]
}

/// The top-level module of an item: `krate::module` when the item is inside
/// a module, `krate` when it is at the crate root (`krate::f`,
/// `krate::{{impl}}::m`).
#[must_use]
pub fn module_of(item: &str) -> &str {
    let mut segs = item.splitn(3, "::");
    let krate = segs.next().unwrap_or(item);
    match (segs.next(), segs.next()) {
        (Some(m), Some(_)) if !m.starts_with('{') && !m.contains(['<', ' ']) => {
            &item[..krate.len() + 2 + m.len()]
        }
        _ => krate,
    }
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

/// The last path segment of a trait: `std::marker::Send` is `Send`.
#[must_use]
pub fn short_trait(tr: &str) -> &str {
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

/// How [`Analysis::trait_solving_by_category`] groups a self type:
/// `async bodies in <crate>`, `opaque impl Future`, `types from <crate>`,
/// or `other`.
#[must_use]
pub fn category(ty: &str) -> String {
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
            .expect("Arc<Bot> obligation");
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
    }

    #[test]
    fn async_trait_shaped_envs_are_attributed() {
        // Shapes from a real rustc 1.98 profile, shortened.
        let at = "CanonicalQueryInput { canonical: Canonical { value: ParamEnvAnd { param_env: ParamEnv { caller_bounds: [Binder { value: OutlivesPredicate('^c_1, '^c_0), bound_vars: [] }, Binder { value: OutlivesPredicate(Self/#0, '^c_0), bound_vars: [] }] }, value: Binder { value: TraitPredicate(<a::Bot as std::marker::Send>, polarity:Positive), bound_vars: [] } }, max_universe: U0, var_kinds: [Region(U0), Region(U0)] } }";
        let plain = "CanonicalQueryInput { canonical: Canonical { value: ParamEnvAnd { param_env: ParamEnv { caller_bounds: [] }, value: Binder { value: OutlivesPredicate('^c_1, '^c_0) } } } }";
        assert!(lifetime_bound_env(at));
        assert!(!lifetime_bound_env(plain));
        assert!(!lifetime_bound_env(&pred("a::Bot", "std::marker::Send")));
        let mut p = Profile::default();
        p.push("evaluate_obligation", Some(at), 0, 2_000_000_000, 0);
        p.push(
            "evaluate_obligation",
            Some(plain),
            2_000_000_000,
            3_000_000_000,
            0,
        );
        let a = analyze(&p);
        assert_eq!(a.async_trait_obligations, 1);
        assert!((a.async_trait_secs - 2.0).abs() < 1e-6);
        assert!((a.trait_solving_secs - 3.0).abs() < 1e-6);
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
    }

    #[test]
    fn threads_are_separate_stacks() {
        let mut p = Profile::default();
        p.push("outer", None, 0, 100, 0);
        p.push("inner", None, 10, 20, 1);
        let a = analyze(&p);
        let outer = a.queries.iter().find(|q| q.name == "outer").expect("outer");
        assert!((outer.secs - 100e-9).abs() < 1e-12);
    }

    #[test]
    fn names_body_items_and_modules() {
        assert_eq!(body_item("a::m::f::{closure#0}"), "a::m::f");
        assert_eq!(body_item("a::m::f::{{closure}}[1]::{{closure}}"), "a::m::f");
        assert_eq!(body_item("a::m::{{impl}}[1]::g"), "a::m::{{impl}}[1]::g");
        assert_eq!(
            body_item("a::m::{impl#2}::g::{closure#1}"),
            "a::m::{impl#2}::g"
        );
        assert_eq!(body_item("a::{constant#0}"), "a");
        assert_eq!(module_of("a::m::{{impl}}[1]::g"), "a::m");
        assert_eq!(module_of("a::m::f"), "a::m");
        assert_eq!(module_of("a::f"), "a");
        assert_eq!(module_of("a::{{impl}}::g"), "a");
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
    fn attributes_body_time_to_items_and_modules() {
        let a = analyze(&even_profile(&[("big", 1000), ("small", 200)]));
        assert_eq!(a.body_count, 1200);
        assert!((a.body_secs - 12.0).abs() < 1e-6, "{}", a.body_secs);
        assert_eq!(a.modules[0].name, "app::big");
        assert_eq!(a.modules[0].count, 1000);
        assert!((a.modules[0].secs - 10.0).abs() < 1e-6);
        assert_eq!(a.modules[1].name, "app::small");
        // The closure's borrowck counts towards its item.
        assert!((a.items[0].secs - 0.01).abs() < 1e-9, "{}", a.items[0].secs);
        assert_eq!(a.items[0].count, 2);
        assert_eq!(a.items.len(), 20);
    }
}
