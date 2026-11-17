//! Finding and reading reports in a target directory (needs std).

use crate::Timings;
use alloc::borrow::ToOwned;
use alloc::vec::Vec;
use std::fs;
use std::path::{Path, PathBuf};

/// The `cargo-timing-*.html` reports in a target directory's
/// `cargo-timings/`, newest first (cargo puts the start time in the name).
#[must_use]
pub fn reports(target_dir: &Path) -> Vec<PathBuf> {
    let Ok(dir) = fs::read_dir(target_dir.join("cargo-timings")) else {
        return Vec::new();
    };
    let mut all: Vec<PathBuf> = dir
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("cargo-timing-") && n.ends_with(".html")
        })
        .map(|e| e.path())
        .collect();
    all.sort();
    all.reverse();
    all
}

/// The newest report in a target directory that has units in it: a build
/// that failed before compiling anything, or one cargo had nothing to do
/// for, writes a report with none. Looks at the newest 50.
#[must_use]
pub fn newest_report(target_dir: &Path) -> Option<PathBuf> {
    reports(target_dir).into_iter().take(50).find(|p| {
        fs::read_to_string(p)
            .ok()
            .and_then(|h| Timings::parse_html(&h).ok())
            .is_some_and(|t| !t.units.is_empty())
    })
}

/// The newest report older than `report` in the same directory that is a
/// `cargo check` ([`Timings::is_check`]), parsed, to compare a build with.
/// Looks at the 10 before it.
#[must_use]
pub fn check_before(report: &Path) -> Option<(PathBuf, Timings)> {
    let dir = report.parent()?;
    let name = report.file_name()?.to_owned();
    let mut older: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.starts_with("cargo-timing-") && s.ends_with(".html") && n < name
        })
        .map(|e| e.path())
        .collect();
    older.sort();
    older.into_iter().rev().take(10).find_map(|p| {
        let t = Timings::parse_html(&fs::read_to_string(&p).ok()?).ok()?;
        t.is_check().then_some((p, t))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_report_skips_empty_ones() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("cargo-timings");
        fs::create_dir(&t).unwrap();
        let one = "<script>\nconst UNIT_DATA = [\n  {\"i\": 0, \"name\": \"x\", \"version\": \"1\", \"mode\": \"todo\", \"target\": \"\", \"start\": 0.5, \"duration\": 10.0, \"rmeta_time\": 2.0, \"unlocked_units\": []}\n];\n</script>";
        let none = "<script>\nconst UNIT_DATA = [\n];\n</script>";
        let old = t.join("cargo-timing-20260101T000000.000Z.html");
        let new = t.join("cargo-timing-20260102T000000.000Z.html");
        fs::write(&old, one).unwrap();
        fs::write(&new, none).unwrap();
        assert_eq!(reports(dir.path()), [new.clone(), old.clone()]);
        assert_eq!(newest_report(dir.path()), Some(old));
        assert_eq!(reports(&dir.path().join("nope")).len(), 0);
    }
}
