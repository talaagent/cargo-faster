//! Every finding links to a section of docs/guide.md; check the anchors exist.

extern crate alloc;

use alloc::collections::BTreeSet;
use std::path::Path;

/// GitHub's heading slug: lowercase, punctuation dropped, spaces to dashes.
fn slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

fn guide_anchors(guide: &str) -> BTreeSet<String> {
    let mut fenced = false;
    let mut out = BTreeSet::new();
    for line in guide.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced && line.starts_with('#') {
            out.insert(slug(line.trim_start_matches('#')));
        }
    }
    out
}

/// The last string literal in each `Finding::new(...)` call.
fn finding_anchors(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (start, _) in src.match_indices("Finding::new(") {
        let mut depth = 0_i32;
        let mut lits = Vec::new();
        let mut chars = src[start + "Finding::new".len()..].chars();
        while let Some(c) = chars.next() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                '"' => {
                    let mut lit = String::new();
                    while let Some(c) = chars.next() {
                        match c {
                            '\\' => {
                                chars.next();
                            }
                            '"' => break,
                            c => lit.push(c),
                        }
                    }
                    lits.push(lit);
                }
                _ => {}
            }
        }
        if let Some(last) = lits.pop() {
            out.push(last);
        }
    }
    out
}

#[test]
fn every_finding_links_to_a_guide_section() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let guide = std::fs::read_to_string(root.join("../../docs/guide.md")).unwrap();
    let anchors = guide_anchors(&guide);
    let mut checked = 0;
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().is_some_and(|n| n == "finding.rs") {
            continue; // its tests use made-up anchors
        }
        let src = std::fs::read_to_string(&path).unwrap();
        for a in finding_anchors(&src) {
            checked += 1;
            if !anchors.contains(&a) {
                missing.push(format!("{}: #{a}", path.display()));
            }
        }
    }
    assert!(
        checked > 30,
        "found only {checked} findings; parser broken?"
    );
    assert!(
        missing.is_empty(),
        "anchors missing from docs/guide.md:\n{}",
        missing.join("\n")
    );
}

/// `guide.md#anchor` links (and bare `#anchor` links inside the guide).
fn doc_links(doc: &str, in_guide: bool) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in doc.match_indices("](") {
        let rest = &doc[i + 2..];
        let Some(end) = rest.find(')') else { continue };
        let target = &rest[..end];
        if let Some(a) = target.strip_prefix("guide.md#") {
            out.push(a.to_owned());
        } else if let Some(a) = target.strip_prefix('#')
            && in_guide
        {
            out.push(a.to_owned());
        }
    }
    out
}

#[test]
fn doc_links_point_at_guide_sections() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let guide = std::fs::read_to_string(root.join("docs/guide.md")).unwrap();
    let anchors = guide_anchors(&guide);
    let mut missing = Vec::new();
    for (file, in_guide) in [
        ("docs/guide.md", true),
        ("docs/cargo-faster.md", false),
        ("README.md", false),
    ] {
        let doc = std::fs::read_to_string(root.join(file)).unwrap();
        for a in doc_links(&doc, in_guide) {
            if !anchors.contains(&a) {
                missing.push(format!("{file}: #{a}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "broken guide links:\n{}",
        missing.join("\n")
    );
}

#[test]
fn skill_has_frontmatter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let skill =
        std::fs::read_to_string(root.join("skills/debug-slow-rust-builds/SKILL.md")).unwrap();
    let front = skill
        .strip_prefix("---\n")
        .and_then(|s| s.split_once("\n---\n"))
        .map(|(f, _)| f)
        .expect("YAML frontmatter");
    assert!(front.lines().any(|l| l == "name: debug-slow-rust-builds"));
    assert!(
        front
            .lines()
            .any(|l| l.starts_with("description: ") && l.contains("Use when"))
    );
}
