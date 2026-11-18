//! `#[async_trait]` uses in the workspace's sources.
//!
//! A crate can depend on async-trait without using it, or use it on two
//! hundred impls. The manifest can't tell those apart, so we read the
//! sources: every `#[async_trait]` (or `#[async_trait::async_trait]`,
//! `#[async_trait(?Send)]`) attribute, whether it sits on a trait or an
//! impl, and which of the traits it defines are used as `dyn Trait`.
//!
//! Each file is parsed with syn and walked with [`syn::visit`], so comments,
//! strings and nested modules are handled the way rustc sees them. Code
//! inside macro invocations (`macro_rules!` bodies, `quote!`) isn't parsed
//! and isn't counted. A file syn can't parse is listed in
//! [`Scan::unparsed`], not guessed at.

use alloc::collections::{BTreeMap, BTreeSet};
use serde::Serialize;
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

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
    /// Source files syn couldn't parse, so not counted.
    pub unparsed: Vec<PathBuf>,
}

/// One workspace source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The package it belongs to.
    pub krate: String,
    /// Where it is, for reporting files that don't parse.
    pub path: PathBuf,
    /// Its contents.
    pub text: String,
}

/// Whether `attr` is `#[async_trait]`, `#[async_trait(?Send)]` or
/// `#[path::to::async_trait]`.
fn is_async_trait(attr: &syn::Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "async_trait")
}

fn last_ident(path: &syn::Path) -> Option<String> {
    path.segments
        .last()
        .map(|segment| segment.ident.to_string())
}

/// The syn walk over one file.
#[derive(Default)]
struct Visitor {
    scan: FileScan,
    /// The traits the file names as `dyn [path::]Trait`, by last segment.
    dyn_names: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Visitor {
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if is_async_trait(attr) {
            self.scan.uses += 1;
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        if item.attrs.iter().any(is_async_trait) {
            self.scan.traits.push(item.ident.to_string());
        }
        visit::visit_item_trait(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if item.attrs.iter().any(is_async_trait)
            && let Some(name) = item.trait_.as_ref().and_then(|(path, _)| last_ident(path))
        {
            self.scan.impls.push(name);
        }
        visit::visit_item_impl(self, item);
    }

    fn visit_type_trait_object(&mut self, object: &'ast syn::TypeTraitObject) {
        for bound in &object.bounds {
            if let syn::TypeParamBound::Trait(bound) = bound
                && let Some(name) = last_ident(&bound.path)
            {
                self.dyn_names.insert(name);
            }
        }
        visit::visit_type_trait_object(self, object);
    }
}

fn visit_source(src: &str) -> syn::Result<Visitor> {
    let file = syn::parse_file(src)?;
    let mut visitor = Visitor::default();
    visitor.visit_file(&file);
    Ok(visitor)
}

/// Count `#[async_trait]` uses in one file's source, or the parse error.
pub fn scan_source(src: &str) -> syn::Result<FileScan> {
    visit_source(src).map(|visitor| visitor.scan)
}

/// The traits a file's source names as `dyn [path::]Trait`, by last
/// segment, or the parse error.
pub fn dyn_names(src: &str) -> syn::Result<BTreeSet<String>> {
    visit_source(src).map(|visitor| visitor.dyn_names)
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
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if name.starts_with('.')
                    || SKIP_DIRS.contains(&name.as_str())
                    || path.join("Cargo.toml").exists()
                {
                    continue;
                }
                stack.push(path);
            } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Scan every workspace member's sources.
#[must_use]
pub fn scan_workspace(m: &Metadata) -> Scan {
    let mut files: Vec<Source> = Vec::new();
    for package in m.members() {
        let Some(dir) = package.manifest_path.parent() else {
            continue;
        };
        for path in package_sources(dir) {
            if let Ok(text) = std::fs::read_to_string(&path) {
                let path = path
                    .strip_prefix(&m.workspace_root)
                    .map_or_else(|_| path.clone(), Path::to_path_buf);
                files.push(Source {
                    krate: package.name.clone(),
                    path,
                    text,
                });
            }
        }
    }
    aggregate(&files)
}

/// Aggregate source files into a [`Scan`].
#[must_use]
pub fn aggregate(files: &[Source]) -> Scan {
    let mut by_crate: BTreeMap<&str, CrateUses> = BTreeMap::new();
    let mut traits: BTreeSet<String> = BTreeSet::new();
    let mut impls: Vec<String> = Vec::new();
    let mut dyn_names: BTreeSet<String> = BTreeSet::new();
    let mut unparsed = Vec::new();
    for source in files {
        let Ok(Visitor {
            scan,
            dyn_names: names,
        }) = visit_source(&source.text)
        else {
            unparsed.push(source.path.clone());
            continue;
        };
        dyn_names.extend(names);
        if scan.uses == 0 {
            continue;
        }
        let uses = by_crate.entry(&source.krate).or_insert_with(|| CrateUses {
            name: source.krate.clone(),
            ..CrateUses::default()
        });
        uses.uses += scan.uses;
        uses.traits += scan.traits.len();
        uses.impls += scan.impls.len();
        traits.extend(scan.traits);
        impls.extend(scan.impls);
    }
    let dyn_traits = traits.intersection(&dyn_names).cloned().collect();
    let foreign_impls = impls.iter().filter(|name| !traits.contains(*name)).count();
    let mut crates: Vec<CrateUses> = by_crate.into_values().collect();
    crates.sort_by(|left, right| {
        right
            .uses
            .cmp(&left.uses)
            .then_with(|| left.name.cmp(&right.name))
    });
    Scan {
        uses: crates.iter().map(|krate| krate.uses).sum(),
        crates,
        traits: traits.into_iter().collect(),
        dyn_traits,
        foreign_impls,
        files: files.len(),
        unparsed,
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

mod nested {
    #[cfg(test)]
    #[async_trait(?Send)]
    impl<'a, T: Clone> crate::hooks::Hooks<'a> for Wrapper<T> {
        async fn run(&self) {}
    }
}

fn held(s: Arc<dyn Store>, h: &dyn crate::Other) {}
fn lifetime<'a>(x: &'a str) -> &'a str { x }
"##;

    fn source(krate: &str, text: &str) -> Source {
        Source {
            krate: krate.to_owned(),
            path: PathBuf::from(format!("{krate}/src/lib.rs")),
            text: text.to_owned(),
        }
    }

    #[test]
    fn counts_attributes_not_text() {
        let scan = scan_source(SRC).unwrap();
        assert_eq!(scan.uses, 4);
        assert_eq!(scan.traits, ["Store", "Hooks"]);
        assert_eq!(scan.impls, ["Store", "Hooks"]);
    }

    #[test]
    fn finds_dyn_traits() {
        let names = dyn_names(SRC).unwrap();
        assert!(names.contains("Store"));
        assert!(names.contains("Other"));
        assert!(!names.contains("Hooks"));
        let boxed = dyn_names("type B = Box<dyn hooks::Hooks + Send>;").unwrap();
        assert!(boxed.contains("Hooks"));
        let ext = dyn_names("type B = Box<dyn StoreExt>;").unwrap();
        assert!(!ext.contains("Store"));
    }

    #[test]
    fn aggregates_per_crate() {
        let files = [
            source("app", SRC),
            source("lib", "#[async_trait]\nimpl Store for Db {}\n"),
            source("dep-only", "use async_trait::async_trait;\n"),
        ];
        let scan = aggregate(&files);
        assert_eq!(scan.uses, 5);
        assert_eq!(scan.files, 3);
        assert_eq!(
            scan.crates,
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
        assert_eq!(scan.traits, ["Hooks", "Store"]);
        assert_eq!(scan.dyn_traits, ["Store"]);
        assert_eq!(scan.foreign_impls, 0);
        assert_eq!(scan.unparsed, Vec::<PathBuf>::new());
        let upstream = aggregate(&[source("a", "#[async_trait]\nimpl up::Driver for D {}\n")]);
        assert_eq!((upstream.uses, upstream.foreign_impls), (1, 1));
    }

    #[test]
    fn reports_files_that_dont_parse() {
        let scan = aggregate(&[
            source("ok", "#[async_trait]\nimpl Store for Db {}\n"),
            source("broken", "#[async_trait]\nimpl Store for {\n"),
        ]);
        assert_eq!((scan.uses, scan.files), (1, 2));
        assert_eq!(scan.unparsed, [PathBuf::from("broken/src/lib.rs")]);
    }

    #[test]
    fn walks_package_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (path, body) in [
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
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let files = package_sources(root);
        let rel: Vec<_> = files
            .iter()
            .map(|file| {
                file.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(rel, ["src/lib.rs", "src/m/x.rs"]);
    }
}
