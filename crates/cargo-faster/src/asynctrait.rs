//! `#[async_trait]` uses in the workspace's sources.
//!
//! A crate can depend on async-trait without using it, or use it on two
//! hundred impls. The manifest can't tell those apart, so we read the
//! sources: every `#[async_trait]` (or `#[async_trait::async_trait]`,
//! `#[async_trait(?Send)]`) attribute, whether it sits on a trait or an
//! impl, and which of the traits it defines are used as `dyn Trait`.
//!
//! The scanner is lexical, not a parser: it blanks out comments and string
//! literals, then looks at attributes and the item that follows them. That
//! is enough for counting and keeps syn out of the build.

use alloc::collections::{BTreeMap, BTreeSet};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::context::Metadata;

/// What one source file holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileScan {
    /// `#[async_trait]` attributes.
    pub uses: usize,
    /// Traits defined with `#[async_trait]`.
    pub traits: Vec<String>,
    /// `#[async_trait]` impls, by the trait they implement.
    pub impls: Vec<String>,
}

/// `#[async_trait]` uses in one workspace crate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CrateUses {
    /// The package name.
    pub name: String,
    /// `#[async_trait]` attributes in its sources.
    pub uses: usize,
    /// Traits it defines with `#[async_trait]`.
    pub traits: usize,
    /// `#[async_trait]` impls.
    pub impls: usize,
}

/// `#[async_trait]` across the workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Scan {
    /// Crates with at least one use, most uses first.
    pub crates: Vec<CrateUses>,
    /// All `#[async_trait]` attributes.
    pub uses: usize,
    /// Traits defined with `#[async_trait]` in the workspace.
    pub traits: Vec<String>,
    /// Those traits that some workspace source names as `dyn Trait`.
    pub dyn_traits: Vec<String>,
    /// `#[async_trait]` impls of traits defined outside the workspace.
    /// They stay until upstream moves to native async fn.
    pub foreign_impls: usize,
    /// Source files read.
    pub files: usize,
}

/// Blank out comments, string and char literals, keeping byte offsets.
#[must_use]
pub fn strip(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in out.iter_mut().take(to).skip(from) {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p);
                blank(&mut out, i, end);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let (mut j, mut depth) = (i + 2, 1);
                while j < b.len() && depth > 0 {
                    if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                        depth += 1;
                        j += 2;
                    } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                blank(&mut out, i, j);
                i = j;
            }
            b'r' if !ident_byte(prev(b, i)) && raw_start(b, i).is_some() => {
                let (hashes, open) = raw_start(b, i).unwrap_or((0, i));
                let close: Vec<u8> = core::iter::once(b'"')
                    .chain(core::iter::repeat_n(b'#', hashes))
                    .collect();
                let end = b[open + 1..]
                    .windows(close.len())
                    .position(|w| w == close.as_slice())
                    .map_or(b.len(), |p| open + 1 + p + close.len());
                blank(&mut out, i, end);
                i = end;
            }
            b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let end = (j + 1).min(b.len());
                blank(&mut out, i, end);
                i = end;
            }
            b'\'' => {
                // A char literal ('a', '\n', '\u{1F600}') or a lifetime ('a).
                let end = if b.get(i + 1) == Some(&b'\\') {
                    b[i + 2..]
                        .iter()
                        .position(|&c| c == b'\'')
                        .map(|p| i + 2 + p + 1)
                } else {
                    let ch = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                    (b.get(i + 1 + ch) == Some(&b'\'')).then_some(i + 2 + ch)
                };
                match end {
                    Some(e) => {
                        blank(&mut out, i, e);
                        i = e;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn prev(b: &[u8], i: usize) -> u8 {
    if i == 0 { b' ' } else { b[i - 1] }
}

fn ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `r"`, `r#"`, `br"`: the number of hashes and the offset of the quote.
fn raw_start(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut j = i + 1;
    while b.get(j) == Some(&b'#') {
        j += 1;
    }
    (b.get(j) == Some(&b'"')).then_some((j - i - 1, j))
}

/// Count `#[async_trait]` uses in one file's source.
#[must_use]
pub fn scan_source(src: &str) -> FileScan {
    let s = strip(src);
    let mut f = FileScan::default();
    let mut at = 0;
    while let Some(p) = s[at..].find("#[") {
        let start = at + p + 2;
        let Some(close) = matching_bracket(&s, start) else {
            break;
        };
        at = close + 1;
        let path: String = s[start..close]
            .split('(')
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if path != "async_trait" && !path.ends_with("::async_trait") {
            continue;
        }
        f.uses += 1;
        match item_after(&s[at..]) {
            Some(Item::Trait(name)) => f.traits.push(name),
            Some(Item::Impl(name)) => f.impls.push(name),
            None => {}
        }
    }
    f
}

/// The index of the `]` closing a `#[` whose contents start at `from`.
fn matching_bracket(s: &str, from: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (i, c) in s[from..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(from + i);
                }
            }
            _ => {}
        }
    }
    None
}

enum Item {
    Trait(String),
    Impl(String),
}

/// The trait or impl an attribute is on: the text up to the item's `{`.
fn item_after(rest: &str) -> Option<Item> {
    let head = &rest[..rest.find(['{', ';']).unwrap_or(rest.len())];
    let words: Vec<&str> = head
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect();
    if let Some(i) = words.iter().position(|w| *w == "trait") {
        return words.get(i + 1).map(|n| Item::Trait((*n).to_owned()));
    }
    let impl_at = find_word(head, "impl")?;
    let for_at = find_word(&head[impl_at..], "for").map(|p| impl_at + p)?;
    let mut tr = head[impl_at + 4..for_at].trim();
    if tr.starts_with('<') {
        tr = skip_generics(tr).trim();
    }
    let tr = tr.split('<').next().unwrap_or(tr);
    let name = tr.rsplit("::").next().unwrap_or(tr).trim();
    Some(Item::Impl(name.trim_start_matches('!').to_owned()))
}

/// Byte offset of `w` as a whole word in `s`.
fn find_word(s: &str, w: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut at = 0;
    while let Some(p) = s[at..].find(w) {
        let i = at + p;
        let before = i == 0 || !ident_byte(b[i - 1]);
        let after = b.get(i + w.len()).is_none_or(|&c| !ident_byte(c));
        if before && after {
            return Some(i);
        }
        at = i + w.len();
    }
    None
}

/// Skip a leading `<...>` (nested).
fn skip_generics(s: &str) -> &str {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return &s[i + 1..];
                }
            }
            _ => {}
        }
    }
    ""
}

/// Whether the stripped source names `trait_name` as `dyn [path::]Trait`.
#[must_use]
pub fn mentions_dyn(stripped: &str, trait_name: &str) -> bool {
    let mut at = 0;
    while let Some(p) = find_word(&stripped[at..], "dyn") {
        let rest = stripped[at + p + 3..].trim_start();
        let path: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
            .collect();
        if path.rsplit("::").next() == Some(trait_name) {
            return true;
        }
        at += p + 3;
    }
    false
}

/// Directories never scanned: build output, vendored code, VCS state.
const SKIP_DIRS: &[&str] = &[
    "target",
    "vendor",
    "vendored",
    "third_party",
    "third-party",
    "node_modules",
];

/// The `.rs` files of the package rooted at `dir`, not descending into
/// nested packages (their own `Cargo.toml`), hidden directories, build
/// output or vendored code.
#[must_use]
pub fn package_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if name.starts_with('.')
                    || SKIP_DIRS.contains(&name.as_str())
                    || p.join("Cargo.toml").exists()
                {
                    continue;
                }
                stack.push(p);
            } else if ft.is_file() && p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Scan every workspace member's sources.
#[must_use]
pub fn scan_workspace(m: &Metadata) -> Scan {
    let mut files: Vec<(String, String)> = Vec::new();
    for p in m.members() {
        let Some(dir) = p.manifest_path.parent() else {
            continue;
        };
        for f in package_sources(dir) {
            if let Ok(src) = std::fs::read_to_string(&f) {
                files.push((p.name.clone(), src));
            }
        }
    }
    aggregate(&files)
}

/// Aggregate `(crate, source)` pairs into a [`Scan`].
#[must_use]
pub fn aggregate(files: &[(String, String)]) -> Scan {
    let mut by_crate: BTreeMap<&str, CrateUses> = BTreeMap::new();
    let mut traits: BTreeSet<String> = BTreeSet::new();
    let mut impls: Vec<String> = Vec::new();
    let mut stripped = Vec::with_capacity(files.len());
    for (krate, src) in files {
        let f = scan_source(src);
        if f.uses > 0 {
            let c = by_crate.entry(krate).or_insert_with(|| CrateUses {
                name: krate.clone(),
                ..CrateUses::default()
            });
            c.uses += f.uses;
            c.traits += f.traits.len();
            c.impls += f.impls.len();
            traits.extend(f.traits);
            impls.extend(f.impls);
        }
        stripped.push(strip(src));
    }
    let dyn_traits = traits
        .iter()
        .filter(|t| stripped.iter().any(|s| mentions_dyn(s, t)))
        .cloned()
        .collect();
    let foreign_impls = impls.iter().filter(|i| !traits.contains(*i)).count();
    let mut crates: Vec<CrateUses> = by_crate.into_values().collect();
    crates.sort_by(|a, b| b.uses.cmp(&a.uses).then_with(|| a.name.cmp(&b.name)));
    Scan {
        uses: crates.iter().map(|c| c.uses).sum(),
        crates,
        traits: traits.into_iter().collect(),
        dyn_traits,
        foreign_impls,
        files: files.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r##"
use async_trait::async_trait;

// #[async_trait] in a comment doesn't count
/* nor #[async_trait] here /* nested */ */
const S: &str = "#[async_trait]";
const R: &str = r#"#[async_trait] "quoted""#;
const C: char = '"';

#[async_trait]
pub trait Store: Send + Sync {
    async fn get(&self, k: &str) -> Option<String>;
}

#[async_trait::async_trait]
pub(crate) trait Hooks<'a> {
    async fn run(&self);
}

#[async_trait]
impl Store for Mem {
    async fn get(&self, _k: &str) -> Option<String> { None }
}

#[derive(Debug)]
struct Mem;

#[cfg(test)]
#[async_trait(?Send)]
impl<'a, T: Clone> crate::hooks::Hooks<'a> for Wrapper<T> {
    async fn run(&self) {}
}

fn held(s: Arc<dyn Store>, h: &dyn crate::Other) {}
fn lifetime<'a>(x: &'a str) -> &'a str { x }
"##;

    #[test]
    fn counts_attributes_not_text() {
        let f = scan_source(SRC);
        assert_eq!(f.uses, 4);
        assert_eq!(f.traits, ["Store", "Hooks"]);
        assert_eq!(f.impls, ["Store", "Hooks"]);
    }

    #[test]
    fn strip_keeps_offsets_and_lifetimes() {
        let s = strip(SRC);
        assert_eq!(s.len(), SRC.len());
        assert!(s.contains("fn lifetime<'a>(x: &'a str)"));
        assert!(!s.contains("quoted"));
    }

    #[test]
    fn finds_dyn_traits() {
        let s = strip(SRC);
        assert!(mentions_dyn(&s, "Store"));
        assert!(!mentions_dyn(&s, "Hooks"));
        assert!(mentions_dyn("Box<dyn hooks::Hooks + Send>", "Hooks"));
        assert!(!mentions_dyn("Box<dyn StoreExt>", "Store"));
    }

    #[test]
    fn aggregates_per_crate() {
        let files = vec![
            ("app".to_owned(), SRC.to_owned()),
            (
                "lib".to_owned(),
                "#[async_trait]\nimpl Store for Db {}\n".to_owned(),
            ),
            (
                "dep-only".to_owned(),
                "use async_trait::async_trait;\n".to_owned(),
            ),
        ];
        let s = aggregate(&files);
        assert_eq!(s.uses, 5);
        assert_eq!(s.files, 3);
        assert_eq!(
            s.crates,
            [
                CrateUses {
                    name: "app".into(),
                    uses: 4,
                    traits: 2,
                    impls: 2
                },
                CrateUses {
                    name: "lib".into(),
                    uses: 1,
                    traits: 0,
                    impls: 1
                },
            ]
        );
        assert_eq!(s.traits, ["Hooks", "Store"]);
        assert_eq!(s.dyn_traits, ["Store"]);
        assert_eq!(s.foreign_impls, 0);
        let up = aggregate(&[(
            "a".to_owned(),
            "#[async_trait]\nimpl up::Driver for D {}\n".to_owned(),
        )]);
        assert_eq!((up.uses, up.foreign_impls), (1, 1));
    }

    #[test]
    fn walks_package_sources() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        for (p, body) in [
            ("src/lib.rs", "#[async_trait]\ntrait A {}\n"),
            ("src/m/x.rs", "#[async_trait]\nimpl A for B {}\n"),
            (
                "target/debug/build/out.rs",
                "#[async_trait]\ntrait Gen {}\n",
            ),
            ("vendor/v/src/lib.rs", "#[async_trait]\ntrait V {}\n"),
            ("sub/Cargo.toml", ""),
            ("sub/src/lib.rs", "#[async_trait]\ntrait Sub {}\n"),
        ] {
            let p = root.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let files = package_sources(root);
        let rel: Vec<_> = files
            .iter()
            .map(|f| f.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(rel, ["src/lib.rs", "src/m/x.rs"]);
    }
}
