//! Findings and reports: what a check saw, why it matters, how to fix it.

use crate::GUIDE_URL;
use core::fmt::Write as _;
use serde::Serialize;

/// How much a finding is likely to cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth knowing; no action needed.
    Info,
    /// A small or situational win.
    Low,
    /// A noticeable win on most builds.
    Medium,
    /// Likely a large share of your build time.
    High,
}

impl Severity {
    /// The lowercase name used in text output.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// One thing a check noticed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// Stable kebab-case id, e.g. `send-proofs-per-async-body`.
    pub id: &'static str,
    /// How much it likely matters.
    pub severity: Severity,
    /// One line.
    pub title: String,
    /// What we saw, with the numbers.
    pub saw: String,
    /// Why it matters.
    pub why: String,
    /// How to fix it, and what the fix costs.
    pub fix: String,
    /// The guide section that explains it.
    pub guide: String,
}

impl Finding {
    /// A finding linking to the guide section `anchor`.
    #[must_use]
    pub fn new(
        id: &'static str,
        severity: Severity,
        title: impl Into<String>,
        anchor: &str,
    ) -> Self {
        Self {
            id,
            severity,
            title: title.into(),
            saw: String::new(),
            why: String::new(),
            fix: String::new(),
            guide: format!("{GUIDE_URL}#{anchor}"),
        }
    }

    /// Set what we saw.
    #[must_use]
    pub fn saw(mut self, s: impl Into<String>) -> Self {
        self.saw = s.into();
        self
    }

    /// Set why it matters.
    #[must_use]
    pub fn why(mut self, s: impl Into<String>) -> Self {
        self.why = s.into();
        self
    }

    /// Set how to fix it.
    #[must_use]
    pub fn fix(mut self, s: impl Into<String>) -> Self {
        self.fix = s.into();
        self
    }
}

/// A named fact a section reports whether or not anything is wrong.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Fact {
    /// Short label.
    pub key: String,
    /// The value, human readable.
    pub value: String,
}

/// The result of one check: facts, findings and free-form detail lines
/// (tables).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Section {
    /// The check's name (`setup`, `cache`, ...).
    pub name: String,
    /// What was measured or read.
    pub facts: Vec<Fact>,
    /// Pre-rendered tables and lists for the text output.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    /// What to do about it.
    pub findings: Vec<Finding>,
    /// Data specific to the check, for `--json` consumers.
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

impl Section {
    /// An empty section called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Add a fact.
    pub fn fact(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.facts.push(Fact {
            key: key.into(),
            value: value.into(),
        });
    }

    /// Add findings, most severe first.
    pub fn extend(&mut self, findings: impl IntoIterator<Item = Finding>) {
        self.findings.extend(findings);
        self.findings
            .sort_by_key(|f| core::cmp::Reverse(f.severity));
    }
}

/// A whole run: one or more sections.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    /// The project the report is about.
    pub workspace: Option<String>,
    /// One per check that ran.
    pub sections: Vec<Section>,
    /// Checks that could not run, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Fact>,
}

impl Report {
    /// Every finding across sections, most severe first.
    #[must_use]
    pub fn findings(&self) -> Vec<&Finding> {
        let mut all: Vec<&Finding> = self.sections.iter().flat_map(|s| &s.findings).collect();
        all.sort_by_key(|f| core::cmp::Reverse(f.severity));
        all
    }

    /// Render for a terminal. `color` adds bold headings and severity colors.
    #[must_use]
    pub fn render_text(&self, color: bool) -> String {
        let paint = |code: &str, s: &str| {
            if color {
                format!("\x1b[{code}m{s}\x1b[0m")
            } else {
                s.to_owned()
            }
        };
        let mut out = String::new();
        if let Some(ws) = &self.workspace {
            let _ = writeln!(out, "{}", paint("1", &format!("cargo faster: {ws}")));
        }
        for section in &self.sections {
            let _ = writeln!(out, "\n{}", paint("1", &format!("== {}", section.name)));
            let width = section.facts.iter().map(|f| f.key.len()).max().unwrap_or(0);
            for fact in &section.facts {
                let _ = writeln!(out, "  {:width$}  {}", fact.key, fact.value);
            }
            for detail in &section.details {
                out.push('\n');
                for line in detail.lines() {
                    let _ = writeln!(out, "  {line}");
                }
            }
            if section.findings.is_empty() {
                let _ = writeln!(out, "\n  {}", paint("32", "nothing to fix here"));
            }
            for f in &section.findings {
                let sev = match f.severity {
                    Severity::High => paint("1;31", "high"),
                    Severity::Medium => paint("33", "medium"),
                    Severity::Low => paint("36", "low"),
                    Severity::Info => paint("2", "info"),
                };
                let _ = writeln!(out, "\n  [{sev}] {}", paint("1", &f.title));
                for (label, text) in [("saw", &f.saw), ("why", &f.why), ("fix", &f.fix)] {
                    if text.is_empty() {
                        continue;
                    }
                    let body = crate::fmt::wrap(text, 92, "         ");
                    let _ = write!(out, "    {label}: {}", body.trim_start());
                }
                let _ = writeln!(out, "    more: {}", f.guide);
            }
        }
        if !self.skipped.is_empty() {
            let _ = writeln!(out, "\n{}", paint("1", "== skipped"));
            for s in &self.skipped {
                let _ = writeln!(out, "  {}: {}", s.key, s.value);
            }
        }
        let all = self.findings();
        let actionable = all.iter().filter(|f| f.severity > Severity::Info).count();
        if self.sections.len() > 1 {
            let _ = writeln!(out, "\n{}", paint("1", "== summary"));
            if actionable == 0 {
                let _ = writeln!(out, "  no findings worth acting on");
            }
            for f in all.iter().filter(|f| f.severity > Severity::Info) {
                let _ = writeln!(out, "  [{}] {}", f.severity.name(), f.title);
            }
        }
        out
    }
}

/// Render a borderless table for the text output and `details`
/// ([`bpaf_kit::table::plain`]): two spaces between columns, numbers lined
/// up on the right, text on the left, the last column left as is.
#[must_use]
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    use bpaf_kit::table::comfy_table::{Cell, CellAlignment};
    let last = headers.len().saturating_sub(1);
    let mut t = bpaf_kit::table::plain(headers.iter().copied());
    for row in rows {
        t.add_row(row.iter().enumerate().map(|(i, cell)| {
            let c = Cell::new(cell);
            if i < last && cell.starts_with(|c: char| c.is_ascii_digit()) {
                c.set_alignment(CellAlignment::Right)
            } else {
                c
            }
        }));
    }
    for (i, column) in t.column_iter_mut().enumerate() {
        column.set_padding((0, if i == last { 0 } else { 2 }));
    }
    let mut out = String::new();
    for line in t.lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_finding() {
        let mut s = Section::new("cache");
        s.fact("wrapper", "none");
        s.extend([
            Finding::new("x", Severity::Low, "Low thing", "linking").saw("saw it"),
            Finding::new("y", Severity::High, "High thing", "debug-info").fix("do it"),
        ]);
        let r = Report {
            workspace: Some("/w".into()),
            sections: vec![s],
            skipped: vec![],
        };
        let text = r.render_text(false);
        assert!(text.find("High thing").unwrap() < text.find("Low thing").unwrap());
        assert!(text.contains("    saw: saw it\n"));
        assert!(text.contains("docs/guide.md#debug-info"));
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["sections"][0]["findings"][0]["severity"], "high");
    }

    #[test]
    fn tables_align() {
        let t = table(
            &["time", "crate"],
            &[
                vec!["1.0s".into(), "a".into()],
                vec!["10.0s".into(), "b".into()],
                vec!["2.0s".into(), "3 units".into()],
            ],
        );
        assert_eq!(t, "time   crate\n 1.0s  a\n10.0s  b\n 2.0s  3 units\n");
    }
}
