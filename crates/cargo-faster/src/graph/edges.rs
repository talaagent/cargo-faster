//! Dependency edges that are cheap to cut: a crate that uses only a few
//! items from a big workspace crate it depends on.
//!
//! The item count reads the dependent's source for paths that start with
//! the dependency's name (`b::x::Y`, `use b::{x, y::Z}`). An item is the
//! path up to its first capitalized segment (a type, trait or constant:
//! `b::config::Config::new` is `config::Config`), or the whole path (a
//! function or macro). Globs, aliases (`use b as c`) and modules imported
//! and then used by their own name make the count unreliable, so those
//! edges are left out.

use super::{count_lines, src_dir, transitive};
use crate::context::{Metadata, Package};
use crate::finding::{Finding, Severity, table};
use alloc::collections::{BTreeMap, BTreeSet};
use std::collections::HashMap;
use std::path::Path;

/// A dependent that uses at most this many items is cheap to cut loose.
pub const FEW_ITEMS: usize = 5;

/// A dependency this big is worth cutting.
pub const CUT_LINES: usize = 5_000;

/// A cut that saves this many rebuilds on an edit is worth reporting.
pub const MIN_SAVED: usize = 3;

/// A dependency that spent this long on the critical path is worth cutting.
pub const MIN_PATH_S: f64 = 2.0;

/// What a crate's source uses from one dependency.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Used {
    /// Distinct items, as paths inside the dependency.
    pub items: BTreeSet<String>,
    /// A glob or an alias hides what's used.
    pub opaque: bool,
}

/// `from -> to`: `from` depends on `to`.
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    /// The dependent.
    pub from: String,
    /// The dependency.
    pub to: String,
    /// What `from` uses from `to`.
    pub items: Vec<String>,
    /// The count is unreliable (see [`Used::opaque`]).
    pub opaque: bool,
    /// Lines of Rust in `to`.
    pub to_lines: usize,
    /// Workspace crates an edit to `to` rebuilds now.
    pub before: usize,
    /// And without this edge.
    pub after: usize,
    /// Seconds `to` spent on the critical path, from a timings report.
    pub critical_s: Option<f64>,
}

impl Edge {
    /// Rebuilds an edit to `to` saves without this edge.
    #[must_use]
    pub const fn saved(&self) -> usize {
        self.before.saturating_sub(self.after)
    }

    /// Few items from a big crate, and the cut saves rebuilds or critical
    /// path time.
    #[must_use]
    pub fn cheap(&self) -> bool {
        !self.opaque
            && !self.items.is_empty()
            && self.items.len() <= FEW_ITEMS
            && self.to_lines >= CUT_LINES
            && (self.saved() >= MIN_SAVED || self.critical_s.is_some_and(|s| s >= MIN_PATH_S))
    }
}

/// Source with comments, strings and char literals blanked out.
fn strip(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let at = |i: usize| b.get(i).copied().unwrap_or('\0');
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        let c = at(i);
        let prev = if i == 0 { ' ' } else { at(i - 1) };
        if c == '/' && at(i + 1) == '/' {
            while i < b.len() && at(i) != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && at(i + 1) == '*' {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if at(i) == '/' && at(i + 1) == '*' {
                    depth += 1;
                    i += 2;
                } else if at(i) == '*' && at(i + 1) == '/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out.push(' ');
            continue;
        }
        let raw_start = !ident(prev) || (prev == 'b' && (i < 2 || !ident(at(i - 2))));
        if c == 'r' && raw_start && matches!(at(i + 1), '"' | '#') {
            let mut j = i + 1;
            let mut hashes = 0;
            while at(j) == '#' {
                hashes += 1;
                j += 1;
            }
            if at(j) == '"' {
                j += 1;
                while j < b.len() {
                    if at(j) == '"' && (1..=hashes).all(|k| at(j + k) == '#') {
                        j += 1 + hashes;
                        break;
                    }
                    j += 1;
                }
                out.push(' ');
                i = j;
                continue;
            }
        }
        if c == '"' {
            i += 1;
            while i < b.len() && at(i) != '"' {
                if at(i) == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push(' ');
            continue;
        }
        if c == '\'' {
            if at(i + 1) == '\\' {
                i += 2;
                while i < b.len() && at(i) != '\'' {
                    i += 1;
                }
                i += 1;
            } else if at(i + 2) == '\'' {
                i += 3;
            } else {
                // A lifetime.
                i += 1;
            }
            out.push(' ');
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Id(String),
    /// `::`
    Sep,
    P(char),
}

fn tokens(src: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_alphanumeric() || c == '_' {
            let mut s = String::from(c);
            while let Some(&n) = chars.peek().filter(|n| n.is_alphanumeric() || **n == '_') {
                s.push(n);
                chars.next();
            }
            out.push(Tok::Id(s));
        } else if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            out.push(Tok::Sep);
        } else if !c.is_whitespace() {
            out.push(Tok::P(c));
        }
    }
    out
}

/// The item a path names: up to its first capitalized segment.
fn item(segs: &[String]) -> String {
    let end = segs
        .iter()
        .position(|s| s.starts_with(|c: char| c.is_uppercase()))
        .map_or(segs.len(), |i| i + 1);
    segs[..end].join("::")
}

fn is_id(t: Option<&Tok>, s: &str) -> bool {
    matches!(t, Some(Tok::Id(x)) if x == s)
}

/// One use tree, from `*i` (just after `prefix::`).
fn tree(toks: &[Tok], i: &mut usize, prefix: &[String], used: &mut Used) {
    let mut segs = prefix.to_vec();
    loop {
        match toks.get(*i) {
            Some(Tok::Id(s)) if s == "self" => {
                *i += 1;
                break;
            }
            Some(Tok::Id(s)) => {
                segs.push(s.clone());
                *i += 1;
                if toks.get(*i) == Some(&Tok::Sep) {
                    *i += 1;
                } else {
                    break;
                }
            }
            Some(Tok::P('*')) => {
                used.opaque = true;
                *i += 1;
                return;
            }
            Some(Tok::P('{')) => {
                *i += 1;
                loop {
                    if toks.get(*i) == Some(&Tok::P('}')) {
                        *i += 1;
                        return;
                    }
                    tree(toks, i, &segs, used);
                    match toks.get(*i) {
                        Some(Tok::P(',')) => *i += 1,
                        Some(Tok::P('}')) => {
                            *i += 1;
                            return;
                        }
                        _ => return,
                    }
                }
            }
            _ => break,
        }
    }
    if is_id(toks.get(*i), "as") {
        *i += 2;
    }
    if !segs.is_empty() {
        used.items.insert(item(&segs));
    }
}

/// What `src` uses from the crate it calls `krate`.
#[must_use]
pub fn used_items(src: &str, krate: &str) -> Used {
    let toks = tokens(&strip(src));
    let mut used = Used::default();
    let mut i = 0;
    while i < toks.len() {
        let after_sep = i > 0 && toks[i - 1] == Tok::Sep;
        // `::krate` is absolute; `a::krate` is something else.
        let absolute = after_sep && !matches!(i.checked_sub(2).map(|j| &toks[j]), Some(Tok::Id(_)));
        if is_id(toks.get(i), krate) && (!after_sep || absolute) {
            match toks.get(i + 1) {
                Some(Tok::Sep) => {
                    i += 2;
                    tree(&toks, &mut i, &[], &mut used);
                    continue;
                }
                Some(Tok::Id(a)) if a == "as" => used.opaque = true,
                _ => {}
            }
        }
        i += 1;
    }
    // `use krate::module;` (or `pub use` it) and then `module::f()` or
    // `crate::module::f()`: count what is used through the module.
    let modules: Vec<String> = used
        .items
        .iter()
        .filter(|it| {
            !it.rsplit("::")
                .next()
                .unwrap_or(it)
                .starts_with(|c: char| c.is_uppercase())
        })
        .cloned()
        .collect();
    for it in modules {
        let segs: Vec<String> = it.split("::").map(String::from).collect();
        let Some(m) = segs.last() else { continue };
        let mut through = Used::default();
        for j in 0..toks.len() {
            if !is_id(toks.get(j), m) || toks.get(j + 1) != Some(&Tok::Sep) {
                continue;
            }
            let head = j == 0 || toks[j - 1] != Tok::Sep;
            let from_root =
                j >= 2 && is_id(toks.get(j - 2), "crate") && (j < 3 || toks[j - 3] != Tok::Sep);
            if head || from_root {
                let mut k = j + 2;
                tree(&toks, &mut k, &segs, &mut through);
            }
        }
        if !through.items.is_empty() || through.opaque {
            used.items.remove(&it);
            used.items.extend(through.items);
            used.opaque |= through.opaque;
        }
    }
    used
}

/// All `.rs` source under `dir`.
fn read_sources(dir: &Path) -> String {
    fn walk(p: &Path, out: &mut String) {
        if p.is_dir() {
            if let Ok(d) = std::fs::read_dir(p) {
                for e in d.flatten() {
                    walk(&e.path(), out);
                }
            }
        } else if p.extension().is_some_and(|e| e == "rs")
            && let Ok(s) = std::fs::read_to_string(p)
        {
            out.push_str(&s);
            out.push('\n');
        }
    }
    let mut out = String::new();
    walk(dir, &mut out);
    out
}

/// Rebuilds an edit to `to` causes when `from` no longer depends on it.
#[must_use]
pub fn fanout_without(rdeps: &BTreeMap<String, BTreeSet<String>>, from: &str, to: &str) -> usize {
    let mut cut = rdeps.clone();
    if let Some(users) = cut.get_mut(to) {
        users.remove(from);
    }
    transitive(&cut, to).len()
}

/// The name `from`'s code uses for `to`, when `from` depends on `to` as a
/// normal dependency.
fn extern_name(m: &Metadata, from: &Package, to: &Package) -> Option<String> {
    let node = m.resolve.as_ref()?.nodes.iter().find(|n| n.id == from.id)?;
    let dep = node.deps.iter().find(|d| d.pkg == to.id)?;
    if !dep.dep_kinds.is_empty() && !dep.dep_kinds.iter().any(|k| k.kind.is_none()) {
        return None;
    }
    if !dep.name.is_empty() {
        return Some(dep.name.clone());
    }
    let lib = to
        .targets
        .iter()
        .find(|t| t.kind.iter().any(|k| k == "lib" || k == "proc-macro"))
        .map_or(to.name.as_str(), |t| t.name.as_str());
    Some(lib.replace('-', "_"))
}

/// Measure the edges `(from, to, seconds to spent on the critical path)`.
/// Reads the dependents' sources; edges that aren't normal workspace
/// dependencies are skipped.
#[must_use]
pub fn gather(m: &Metadata, pairs: &[(String, String, Option<f64>)]) -> Vec<Edge> {
    let (normal, _) = super::reverse_deps(m);
    let members: HashMap<&str, &Package> = m.members().map(|p| (p.name.as_str(), p)).collect();
    let mut sources: HashMap<&str, String> = HashMap::new();
    let mut lines: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::new();
    for (from, to, critical_s) in pairs {
        let (Some(&f), Some(&t)) = (members.get(from.as_str()), members.get(to.as_str())) else {
            continue;
        };
        let Some(name) = extern_name(m, f, t) else {
            continue;
        };
        let src = sources
            .entry(f.name.as_str())
            .or_insert_with(|| read_sources(&src_dir(f)));
        let used = used_items(src, &name);
        let to_lines = *lines
            .entry(t.name.as_str())
            .or_insert_with(|| count_lines(&src_dir(t)).0);
        out.push(Edge {
            from: from.clone(),
            to: to.clone(),
            items: used.items.into_iter().collect(),
            opaque: used.opaque,
            to_lines,
            before: transitive(&normal, to).len(),
            after: fanout_without(&normal, from, to),
            critical_s: *critical_s,
        });
    }
    out
}

/// The edges into each of `crates` (by name) from their direct dependents.
#[must_use]
pub fn into_crates(m: &Metadata, crates: &[&str]) -> Vec<(String, String, Option<f64>)> {
    let (normal, _) = super::reverse_deps(m);
    crates
        .iter()
        .flat_map(|to| {
            normal
                .get(*to)
                .into_iter()
                .flatten()
                .map(|from| (from.clone(), (*to).to_owned(), None))
        })
        .collect()
}

fn lines_k(n: usize) -> String {
    if n >= 10_000 {
        format!("{}k lines", n / 1000)
    } else {
        format!("{n} lines")
    }
}

/// Findings for the cheap edges, most rebuilds saved first.
#[must_use]
pub fn findings(edges: &[Edge], limit: usize) -> Vec<Finding> {
    let mut cheap: Vec<&Edge> = edges.iter().filter(|e| e.cheap()).collect();
    cheap.sort_by(|a, b| {
        b.saved()
            .cmp(&a.saved())
            .then_with(|| {
                b.critical_s
                    .unwrap_or(0.0)
                    .total_cmp(&a.critical_s.unwrap_or(0.0))
            })
            .then_with(|| b.to_lines.cmp(&a.to_lines))
            .then_with(|| (&a.from, &a.to).cmp(&(&b.from, &b.to)))
    });
    cheap
        .into_iter()
        .take(limit)
        .map(|e| {
            let n = e.items.len();
            let items = if n == 1 { "item" } else { "items" };
            let path = e
                .critical_s
                .map(|s| format!(", {s:.1}s on the critical path"))
                .unwrap_or_default();
            let severity = if (e.saved() >= 10 && e.saved() * 2 >= e.before)
                || e.critical_s.is_some_and(|s| s >= 5.0)
            {
                Severity::Medium
            } else {
                Severity::Low
            };
            let rust = e.to.replace('-', "_");
            let list: Vec<String> = e.items.iter().map(|i| format!("`{rust}::{i}`")).collect();
            Finding::new(
                "cheap-to-cut-edge",
                severity,
                format!("{} uses {n} {items} from {} ({}{path})", e.from, e.to, lines_k(e.to_lines)),
                "cutting-a-dependency-edge",
            )
            .saw(format!(
                "{} depends on {} ({} lines) but uses only {}. An edit to {} rebuilds {} workspace crates; without this edge it would rebuild {}.",
                e.from, e.to, e.to_lines, list.join(", "), e.to, e.before, e.after
            ))
            .why(format!(
                "A dependency makes {} wait for all of {} to compile and rebuild whenever it changes, however little of it {} uses.",
                e.from, e.to, e.from
            ))
            .fix(format!(
                "Move {} down into a small crate that both can depend on (or one {} already depends on), then drop {} from {}'s [dependencies]. The tradeoff is one more crate, or code living away from its neighbors.",
                if n == 1 { "it" } else { "them" }, e.from, e.to, e.from
            ))
        })
        .collect()
}

/// A table of edges: every one given, or only the cheap ones.
#[must_use]
pub fn detail(edges: &[Edge], all: bool) -> Option<String> {
    let rows: Vec<Vec<String>> = edges
        .iter()
        .filter(|e| all || e.cheap())
        .map(|e| {
            vec![
                if e.opaque {
                    "?".to_owned()
                } else {
                    e.items.len().to_string()
                },
                format!("{} -> {}", e.before, e.after),
                format!("{} -> {}", e.from, e.to),
            ]
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    Some(format!(
        "{}\n{}",
        if all {
            "what each dependent uses (? = a glob or an alias)"
        } else {
            "dependency edges that are cheap to cut"
        },
        table(&["items", "rebuilds without it", "edge"], &rows)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_paths_and_use_trees() {
        let src = r#"
            use app_store::{Store, config::{self, Limits as L}, open};
            // app_store::Commented is ignored
            /* and app_store::Blocked too */
            fn f() -> app_store::Store {
                let s = "app_store::InString";
                let c = 'x';
                app_store::open(&L::default());
                ::app_store::Error::Missing;
                app_store::m!();
                other::app_store::Nope;
                app_store::Store::new()
            }
        "#;
        let u = used_items(src, "app_store");
        assert!(!u.opaque, "{u:?}");
        assert_eq!(
            u.items.into_iter().collect::<Vec<_>>(),
            ["Error", "Store", "config", "config::Limits", "m", "open"]
        );
    }

    #[test]
    fn globs_and_aliases_are_opaque() {
        assert!(used_items("use b::*;", "b").opaque);
        assert!(used_items("use b::{x::*, Y};", "b").opaque);
        assert!(used_items("use b as c;", "b").opaque);
        assert!(used_items("use b::m; use m::*;", "b").opaque);
        assert!(used_items("fn f() {}", "b").items.is_empty());
        // Raw strings and lifetimes don't confuse it.
        let u = used_items("fn f<'a>(x: &'a str) { r#\"b::No\"#; b::Yes }", "b");
        assert_eq!(u.items.into_iter().collect::<Vec<_>>(), ["Yes"]);
    }

    #[test]
    fn imported_modules_count_what_is_used_through_them() {
        let names = |u: Used| u.items.into_iter().collect::<Vec<_>>();
        let u = used_items("use b::util; fn f() { util::go() }", "b");
        assert_eq!(names(u), ["util::go"]);
        // A function, not a module: stays as it is.
        assert_eq!(
            names(used_items("use b::util; fn f() { util() }", "b")),
            ["util"]
        );
        // Re-exported at the root and reached through `crate::` (the
        // app-connections pattern), in paths and in use trees.
        let src = "pub use b::{config, orgs, pg}; pub use b::config_kdl;
            use crate::config::Config;
            fn f(_: impl Fn() -> Option<crate::orgs::Role>) { crate::pg::shared(); }
            fn g() { crate::pg::testing::db(); let pg = 1; config_kdl::parse(); }
            use crate::{orgs::Role, config::{Config as C, Other}};";
        let u = used_items(src, "b");
        assert!(!u.opaque);
        assert_eq!(
            names(u),
            [
                "config::Config",
                "config::Other",
                "config_kdl::parse",
                "orgs::Role",
                "pg::shared",
                "pg::testing::db"
            ]
        );
    }

    fn edge(items: &[&str], to_lines: usize, before: usize, after: usize) -> Edge {
        Edge {
            from: "app-config".into(),
            to: "app-session-store".into(),
            items: items.iter().map(|s| (*s).to_owned()).collect(),
            opaque: false,
            to_lines,
            before,
            after,
            critical_s: None,
        }
    }

    #[test]
    fn cheap_edges_become_findings() {
        let e = Edge {
            critical_s: Some(8.0),
            ..edge(&["SessionId", "store::open"], 24_000, 79, 28)
        };
        let f = findings(&[e], 5);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].id, "cheap-to-cut-edge");
        assert_eq!(f[0].severity, Severity::Medium);
        assert_eq!(
            f[0].title,
            "app-config uses 2 items from app-session-store (24k lines, 8.0s on the critical path)"
        );
        assert!(
            f[0].saw
                .contains("rebuilds 79 workspace crates; without this edge it would rebuild 28")
        );
        assert!(f[0].saw.contains("`app_session_store::SessionId`"));
        // Too many items, too small, nothing saved, or unreliable: no finding.
        let many = edge(&["A", "B", "C", "D", "E", "F"], 24_000, 79, 28);
        let small = edge(&["A"], 800, 79, 28);
        let same = edge(&["A"], 24_000, 79, 79);
        let opaque = Edge {
            opaque: true,
            ..edge(&["A"], 24_000, 79, 28)
        };
        assert_eq!(findings(&[many, small, same.clone(), opaque], 5).len(), 0);
        // On the critical path, even when other crates still reach it.
        let slow = Edge {
            critical_s: Some(3.0),
            ..same
        };
        assert_eq!(
            findings(core::slice::from_ref(&slow), 5)[0].severity,
            Severity::Low
        );
        assert!(detail(&[slow], false).unwrap().contains("79 -> 79"));
    }

    const META: &str = r#"{
      "workspace_root": "/w", "target_directory": "/w/target",
      "workspace_members": ["store 0.1", "config 0.1", "app 0.1", "web 0.1"],
      "packages": [
        {"id": "store 0.1", "name": "store", "version": "0.1.0", "manifest_path": "/w/store/Cargo.toml",
         "targets": [{"name": "store", "kind": ["lib"], "src_path": "/w/store/src/lib.rs"}]},
        {"id": "config 0.1", "name": "config", "version": "0.1.0", "manifest_path": "/w/config/Cargo.toml"},
        {"id": "app 0.1", "name": "app", "version": "0.1.0", "manifest_path": "/w/app/Cargo.toml"},
        {"id": "web 0.1", "name": "web", "version": "0.1.0", "manifest_path": "/w/web/Cargo.toml"}
      ],
      "resolve": {"nodes": [
        {"id": "config 0.1", "deps": [{"name": "kv", "pkg": "store 0.1", "dep_kinds": [{"kind": null}]}]},
        {"id": "app 0.1", "deps": [{"name": "config", "pkg": "config 0.1", "dep_kinds": [{"kind": null}]},
                                   {"name": "store", "pkg": "store 0.1", "dep_kinds": [{"kind": "dev"}]}]},
        {"id": "web 0.1", "deps": [{"name": "config", "pkg": "config 0.1", "dep_kinds": [{"kind": null}]}]}
      ]}
    }"#;

    #[test]
    fn gathers_from_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let meta = META.replace("/w", &root.display().to_string());
        let m = Metadata::parse(&meta).unwrap();
        for (krate, src) in [
            ("store", "pub struct Store;\n".repeat(CUT_LINES)),
            // `store` is renamed to `kv` in config's Cargo.toml.
            (
                "config",
                "use kv::Store;\npub fn f() -> kv::Error { todo!() }\n".to_owned(),
            ),
            ("app", "fn main() {}\n".to_owned()),
            ("web", "fn main() {}\n".to_owned()),
        ] {
            std::fs::create_dir_all(root.join(krate).join("src")).unwrap();
            std::fs::write(root.join(krate).join("src/lib.rs"), src).unwrap();
        }
        let pairs = into_crates(&m, &["store"]);
        assert_eq!(pairs, [("config".to_owned(), "store".to_owned(), None)]);
        let e = gather(&m, &pairs);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].items, ["Error", "Store"]);
        assert_eq!(e[0].to_lines, CUT_LINES);
        assert_eq!((e[0].before, e[0].after), (3, 0));
        assert_eq!(
            findings(&e, 5)[0].title,
            "config uses 2 items from store (5000 lines)"
        );
        // A dev-dependency isn't an edge an edit rebuilds through.
        assert_eq!(gather(&m, &[("app".into(), "store".into(), None)]).len(), 0);
    }
}
