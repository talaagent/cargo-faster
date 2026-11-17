//! Parse the HTML reports `cargo build --timings` writes (to
//! `target/cargo-timings/`) into typed units, and analyze them: the
//! critical path, crates that ran mostly alone, long code generation,
//! linking, build scripts, time by kind of unit, slowdowns against an
//! earlier report and the workspace crates a build recompiled.
//!
//! [`Timings::parse_html`] reads a report into [`Timings`];
//! [`analyze`] and [`analyze_with`] turn it into an [`Analysis`]. Both are
//! serde types; the analysis serializes with seconds in `_s` fields.
//!
//! Reports from cargo 1.88 on (with `sections`) and older ones (with
//! `rmeta_time` and `unlocked_units`) both parse.
//!
//! # Example
//!
//! ```
//! use cargo_timings_parse::{Kind, Timings, analyze};
//!
//! let html = r#"<script>
//! const UNIT_DATA = [
//!   {"i": 0, "name": "core-lib", "version": "0.1.0", "mode": "todo", "target": "",
//!    "start": 0.0, "duration": 4.0, "rmeta_time": 1.5, "unblocked_units": [1]},
//!   {"i": 1, "name": "app-cli", "version": "0.1.0", "mode": "todo", "target": " app \"bin\"",
//!    "start": 4.1, "duration": 2.0, "unblocked_units": []}
//! ];
//! </script>"#;
//! let t = Timings::parse_html(html)?;
//! assert_eq!(t.units[1].kind(), Kind::Link);
//!
//! let a = analyze(&t);
//! assert!((a.wall - 6.1).abs() < 1e-9);
//! let path: Vec<&str> = a.critical_path.iter().map(|s| s.unit.as_str()).collect();
//! assert_eq!(path, ["core-lib", "app-cli app \"bin\""]);
//! # Ok::<(), cargo_timings_parse::ParseError>(())
//! ```
//!
//! # Features
//!
//! - `std` (default): [`reports`], [`newest_report`] and [`check_before`],
//!   which find and read reports in a target directory. Without it the
//!   crate is `no_std` (it needs `alloc`).

#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod analysis;
#[cfg(feature = "std")]
mod files;

pub use analysis::{
    Analysis, Extra, KindTime, Linked, PathStep, Rebuilt, Serial, Slowdown, UnitSecs, UnitTime,
    Waited, Workspace, analyze, analyze_with, critical_path, path_edges,
};
#[cfg(feature = "std")]
pub use files::{check_before, newest_report, reports};

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use serde::{Deserialize, Serialize};

/// One unit (a crate target in one mode) from the report.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    /// Unit index (references in `unblocked_*` use it).
    #[serde(default)]
    pub i: Option<usize>,
    /// Package name.
    pub name: String,
    /// Package version.
    #[serde(default)]
    pub version: String,
    /// `todo` for compiling, `run-custom-build` for build scripts.
    #[serde(default)]
    pub mode: String,
    /// e.g. ` (check)`, ` (check-test)`, ` build-script (run)`.
    #[serde(default)]
    pub target: String,
    /// Start, seconds since the build began.
    pub start: f64,
    /// Duration in seconds (0 when the unit was fresh).
    pub duration: f64,
    /// Units this one unblocks when it finishes.
    #[serde(default, alias = "unlocked_units")]
    pub unblocked_units: Vec<usize>,
    /// Units this one unblocks once its metadata is ready.
    #[serde(default, alias = "unlocked_rmeta_units")]
    pub unblocked_rmeta_units: Vec<usize>,
    /// Cargo 1.88+ sections: `[["frontend", {start, end}], ["codegen", ..]]`.
    #[serde(default)]
    pub sections: Option<Vec<(String, Span)>>,
    /// Older cargo: when the metadata was ready.
    #[serde(default)]
    pub rmeta_time: Option<f64>,
}

/// What a unit produces.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// `cargo check`: metadata only, no code generation.
    Check,
    /// A library, built (code generated, not linked).
    Lib,
    /// A binary, test, example or bench: code generated and linked.
    Link,
    /// A build script, compiled or run.
    BuildScript,
}

impl Kind {
    /// A label for tables.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Check => "check (metadata only)",
            Self::Lib => "libraries (built)",
            Self::Link => "binaries and tests (codegen + link)",
            Self::BuildScript => "build scripts",
        }
    }
}

/// A section of a unit, relative to its start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Span {
    /// Start.
    pub start: f64,
    /// End.
    pub end: f64,
}

impl Unit {
    /// When the unit finished.
    #[must_use]
    pub fn end(&self) -> f64 {
        self.start + self.duration
    }

    /// When its metadata was ready (dependents can start checking).
    #[must_use]
    pub fn rmeta_end(&self) -> f64 {
        if let Some(s) = self.section("frontend") {
            return self.start + s.end;
        }
        self.start + self.rmeta_time.unwrap_or(self.duration)
    }

    /// A named section (`frontend`, `codegen`), when the report has them.
    #[must_use]
    pub fn section(&self, name: &str) -> Option<Span> {
        self.sections
            .as_ref()?
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| *s)
    }

    /// Seconds after the metadata was ready (code generation, or for a
    /// check unit the analysis after it), if the report has them.
    #[must_use]
    pub fn codegen(&self) -> Option<f64> {
        if let Some(s) = self.section("codegen") {
            return Some(s.end - s.start);
        }
        self.rmeta_time.map(|r| self.duration - r)
    }

    /// `name target` for display, e.g. `app (check-test)`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}{}", self.name, self.target)
    }

    /// What the unit is, for JSON output.
    #[must_use]
    pub fn of(&self) -> UnitOf {
        UnitOf {
            krate: self.name.clone(),
            version: self.version.clone(),
            target: self.target.trim().to_owned(),
            mode: self.mode.clone(),
            kind: self.kind(),
        }
    }

    /// Whether it compiles or runs a build script.
    #[must_use]
    pub fn is_build_script(&self) -> bool {
        self.mode == "run-custom-build" || self.target.contains("build-script")
    }

    /// What the unit produces.
    #[must_use]
    pub fn kind(&self) -> Kind {
        if self.is_build_script() {
            Kind::BuildScript
        } else if self.target.contains("(check") {
            Kind::Check
        } else if ["\"bin\"", "\"test\"", "\"example\"", "\"bench\"", "(test)"]
            .iter()
            .any(|k| self.target.contains(k))
        {
            Kind::Link
        } else {
            Kind::Lib
        }
    }

    /// The rustc crate name it compiles: the target's name for a `bin`,
    /// `test`, `example` or `bench` target, else the package name, with
    /// `-` as `_`.
    #[must_use]
    pub fn crate_name(&self) -> String {
        let t = self.target.trim_start();
        let named = ["\"bin\"", "\"test\"", "\"example\"", "\"bench\""]
            .iter()
            .any(|k| t.contains(k));
        let name = if named {
            t.split(' ').next().unwrap_or(&self.name)
        } else {
            &self.name
        };
        name.replace('-', "_")
    }
}

/// What a unit is: its package, target and mode, as in the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UnitOf {
    /// The package name.
    #[serde(rename = "crate")]
    pub krate: String,
    /// The package version.
    pub version: String,
    /// Cargo's target label (`app "bin" (check)`), empty for a library
    /// build.
    pub target: String,
    /// Cargo's mode (`todo`, `run-custom-build`).
    pub mode: String,
    /// What it produces.
    pub kind: Kind,
}

/// Cargo's `jobs` and the CPU count, from the report's "Max concurrency"
/// line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jobs {
    /// `-j`, or `build.jobs`.
    pub jobs: u32,
    /// CPUs cargo saw.
    pub ncpu: u32,
}

/// A parsed timings report.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    /// Every unit, fresh ones included.
    pub units: Vec<Unit>,
    /// `(seconds, percent)` CPU samples, if present.
    #[serde(default)]
    pub cpu: Vec<(f64, f64)>,
    /// When the build started, Unix milliseconds.
    #[serde(default)]
    pub start_unix_ms: Option<f64>,
    /// `jobs` and `ncpu` from the report's header.
    #[serde(default)]
    pub jobs: Option<Jobs>,
}

/// A file that isn't a timings report.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseError {
    /// No `const UNIT_DATA = [...]` in it.
    NoUnitData,
    /// `UNIT_DATA` isn't the JSON cargo writes.
    UnitData(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoUnitData => {
                f.write_str("no UNIT_DATA in this file: is it a cargo --timings report?")
            }
            Self::UnitData(e) => write!(f, "parsing UNIT_DATA: {e}"),
        }
    }
}

impl core::error::Error for ParseError {}

impl Timings {
    /// Parse the HTML report cargo writes to `target/cargo-timings/`.
    ///
    /// # Errors
    ///
    /// When the file doesn't contain cargo's `UNIT_DATA`, or it isn't
    /// valid.
    pub fn parse_html(html: &str) -> Result<Self, ParseError> {
        let data = js_const(html, "UNIT_DATA").ok_or(ParseError::NoUnitData)?;
        let units: Vec<Unit> =
            serde_json::from_str(data).map_err(|e| ParseError::UnitData(format!("{e}")))?;
        let cpu = js_const(html, "CPU_USAGE")
            .and_then(|c| serde_json::from_str::<Vec<(f64, f64)>>(c).ok())
            .unwrap_or_default();
        Ok(Self {
            units,
            cpu,
            start_unix_ms: summary_cell(html, "Build start:").and_then(parse_timestamp),
            jobs: summary_cell(html, "Max concurrency:").and_then(jobs),
        })
    }

    /// Whether the report is a `cargo check`: units were checked and none
    /// linked. (A check still builds proc macros and build scripts' library
    /// dependencies, so those may be in it.)
    #[must_use]
    pub fn is_check(&self) -> bool {
        self.ran().any(|u| u.kind() == Kind::Check) && self.ran().all(|u| u.kind() != Kind::Link)
    }

    /// The units that actually compiled.
    pub fn ran(&self) -> impl Iterator<Item = &Unit> {
        self.units.iter().filter(|u| u.duration > 0.0)
    }
}

/// The JSON array after `const NAME = `, up to the matching `];`.
fn js_const<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    let start = html.find(&format!("const {name} = "))? + name.len() + 9;
    let rest = &html[start..];
    let end = rest
        .find("\n];")
        .map(|e| e + 2)
        .or_else(|| rest.find("];").map(|e| e + 1))?;
    Some(&rest[..end])
}

/// The value cell after `<td>LABEL</td>` in the report's summary table.
fn summary_cell<'a>(html: &'a str, label: &str) -> Option<&'a str> {
    let rest = &html[html.find(&format!("<td>{label}</td>"))? + label.len() + 9..];
    let rest = &rest[rest.find("<td>")? + 4..];
    Some(rest[..rest.find("</td>")?].trim())
}

/// `23 (jobs=16 ncpu=32)` → `Jobs { jobs: 16, ncpu: 32 }`.
fn jobs(cell: &str) -> Option<Jobs> {
    let num = |k: &str| -> Option<u32> {
        let r = &cell[cell.find(k)? + k.len()..];
        r[..r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len())]
            .parse()
            .ok()
    };
    Some(Jobs {
        jobs: num("jobs=")?,
        ncpu: num("ncpu=")?,
    })
}

/// An RFC 3339 UTC timestamp (`2026-11-17T09:57:58.540254618Z`, or with
/// `+00:00`), as cargo writes the build start, in Unix milliseconds. Other
/// offsets give `None`.
#[allow(
    clippy::many_single_char_names,
    reason = "the civil-date formula's names"
)]
#[must_use]
pub fn parse_timestamp(ts: &str) -> Option<f64> {
    let ts = ts.strip_suffix('Z').or_else(|| ts.strip_suffix("+00:00"))?;
    let (date, time) = ts.split_once('T')?;
    let mut d = date.splitn(3, '-').map(str::parse::<i64>);
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.splitn(3, ':');
    let h: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next()?.parse().ok()?;
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let whole = ((days * 24 + h) * 60 + min) as f64;
    Some((whole * 60.0 + sec) * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn parses_html_and_old_format() {
        let html = "<script>\nconst UNIT_DATA = [\n  {\"i\": 0, \"name\": \"x\", \"version\": \"1\", \"mode\": \"todo\", \"target\": \"\", \"start\": 0.5, \"duration\": 10.0, \"rmeta_time\": 2.0, \"unlocked_units\": []}\n];\nconst CPU_USAGE = [[0.1, 40.0]];\n</script>";
        let t = Timings::parse_html(html).unwrap();
        assert_eq!(t.units.len(), 1);
        assert_eq!(t.cpu, [(0.1, 40.0)]);
        assert!((t.units[0].codegen().unwrap() - 8.0).abs() < 1e-9);
        assert!((t.units[0].rmeta_end() - 2.5).abs() < 1e-9);
        assert_eq!(
            Timings::parse_html("<html></html>"),
            Err(ParseError::NoUnitData)
        );
        let bad = Timings::parse_html("const UNIT_DATA = [{\"x\": 1}];").unwrap_err();
        assert!(format!("{bad}").starts_with("parsing UNIT_DATA: "), "{bad}");
    }

    #[test]
    fn report_header() {
        assert_eq!(
            jobs("23 (jobs=16 ncpu=32)"),
            Some(Jobs { jobs: 16, ncpu: 32 })
        );
        // 2026-11-17T09:57:58.5Z, checked with `date -d ... +%s`.
        let ms = parse_timestamp("2026-11-17T09:57:58.540254618Z").unwrap();
        assert!((ms - 1_794_909_478_540.254_6).abs() < 1.0);
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_timestamp("1970-01-01T00:00:01+00:00"), Some(1000.0));
        assert!(parse_timestamp("2026-11-17 09:57:58").is_none());
        assert!(parse_timestamp("2026-11-17T09:57:58+01:00").is_none());
    }

    #[test]
    fn kinds() {
        let k = |target: &str| {
            Unit {
                target: target.into(),
                ..Unit::default()
            }
            .kind()
        };
        assert_eq!(k(" (check)"), Kind::Check);
        assert_eq!(k(" x \"test\" (check-test)"), Kind::Check);
        assert_eq!(k(""), Kind::Lib);
        assert_eq!(k(" app \"bin\""), Kind::Link);
        assert_eq!(k(" (test)"), Kind::Link);
        assert_eq!(k(" build-script (run)"), Kind::BuildScript);
        let run = Unit {
            mode: "run-custom-build".into(),
            ..Unit::default()
        };
        assert!(run.is_build_script());
    }

    #[test]
    fn crate_names() {
        let u = |name: &str, target: &str| Unit {
            name: name.into(),
            target: target.into(),
            ..Unit::default()
        };
        assert_eq!(u("my-core", " (check)").crate_name(), "my_core");
        assert_eq!(u("my-app-bin", " my-app \"bin\"").crate_name(), "my_app");
        assert_eq!(u("x", "").crate_name(), "x");
    }

    #[test]
    fn serde_round_trip() {
        let t = Timings {
            units: vec![Unit {
                name: "x".into(),
                duration: 1.0,
                sections: Some(vec![(
                    "frontend".into(),
                    Span {
                        start: 0.0,
                        end: 0.5,
                    },
                )]),
                ..Unit::default()
            }],
            jobs: Some(Jobs { jobs: 4, ncpu: 8 }),
            ..Timings::default()
        };
        let s = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Timings>(&s).unwrap(), t);
        let of = t.units[0].of();
        let v = serde_json::to_value(&of).unwrap();
        assert_eq!(v["crate"], "x");
        assert_eq!(v["kind"], "lib");
        assert_eq!(serde_json::from_value::<UnitOf>(v).unwrap(), of);
    }
}
