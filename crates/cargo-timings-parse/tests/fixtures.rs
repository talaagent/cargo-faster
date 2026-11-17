//! Real reports, from building this repository (see `tests/fixtures/README.md`).

use cargo_timings_parse::{
    Extra, Jobs, Kind, Timings, Waited, Workspace, analyze, analyze_with, check_before, path_edges,
};
extern crate alloc;

use alloc::collections::BTreeSet;

fn read(name: &str) -> Timings {
    let path = format!(
        "{}/tests/fixtures/cargo-faster/{name}.html",
        env!("CARGO_MANIFEST_DIR")
    );
    let html = std::fs::read_to_string(path).expect("fixture");
    Timings::parse_html(&html).expect("a timings report")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// The workspace when the reports were made.
fn workspace() -> Workspace {
    let none = BTreeSet::new;
    Workspace {
        deps: [
            (
                "cargo-faster".to_owned(),
                [
                    "nextest-libtest-json".to_owned(),
                    "rustc-self-profile".to_owned(),
                ]
                .into(),
            ),
            ("nextest-libtest-json".to_owned(), none()),
            ("rustc-self-profile".to_owned(), none()),
        ]
        .into(),
    }
}

/// `cargo check --workspace --all-targets --timings`, from scratch.
#[test]
fn cold_check() {
    let t = read("check");
    assert_eq!(t.units.len(), 113);
    assert_eq!(t.jobs, Some(Jobs { jobs: 16, ncpu: 32 }));
    assert_eq!(t.cpu.len(), 42);
    // Build start: 2026-11-17T11:52:51.648447438Z.
    assert!((t.start_unix_ms.unwrap() - 1_794_916_371_648.447).abs() < 1.0);
    // Proc macros and their dependencies are built even by a check.
    assert!(t.is_check());

    let a = analyze(&t);
    assert_eq!((a.compiled, a.units_fresh, a.total_units), (108, 5, 113));
    assert!(close(a.wall, 8.51));
    assert!(close(a.path_gaps, 0.51));
    let kinds: Vec<(Kind, usize)> = a.kinds.iter().map(|k| (k.kind, k.units)).collect();
    assert_eq!(
        kinds,
        [(Kind::Check, 74), (Kind::Lib, 13), (Kind::BuildScript, 21)]
    );
    let path: Vec<&str> = a.critical_path.iter().map(|s| s.unit.as_str()).collect();
    assert_eq!(
        path,
        [
            "unicode-ident",
            "proc-macro2",
            "quote",
            "syn",
            "bpaf_derive",
            "bpaf (check)",
            "bpaf-kit (check)",
            "cargo-faster (check-test)"
        ]
    );
    assert_eq!(a.critical_path[1].waited, Waited::Meta);
    assert_eq!(a.critical_path[4].waited, Waited::Full);
    let syn = &a.slowest[0];
    assert_eq!(
        (syn.unit.as_str(), syn.of.version.as_str()),
        ("syn", "2.0.119")
    );
    assert!(close(syn.duration, 3.69));
    assert_eq!(a.linked.units, 0);
}

/// `cargo build --workspace --timings` right after it: the libraries
/// the check didn't build, and the binary.
#[test]
fn build_after_check() {
    let t = read("build");
    assert_eq!(t.units.len(), 62);
    assert!(!t.is_check());
    let a = analyze(&t);
    assert_eq!((a.compiled, a.units_fresh), (62, 0));
    assert!(close(a.wall, 7.38));
    let last = a.critical_path.last().unwrap();
    assert_eq!(last.unit, "cargo-faster cargo-faster \"bin\"");
    assert_eq!(last.of.kind, Kind::Link);
    assert_eq!(a.critical_path.len(), 7);
    assert_eq!(a.critical_path[0].unit, "regex-syntax");
    assert_eq!(a.linked.units, 1);
    assert!(close(a.linked.tail, 1.05));
    assert_eq!(a.linked.slowest[0].unit, last.unit);
}

/// The same build compared with the check before it and the workspace.
#[test]
fn build_compared_with_check() {
    let build = read("build");
    let check = read("check");
    let ws = workspace();
    let a = analyze_with(
        &build,
        Extra {
            baseline: Some(&check),
            workspace: Some(&ws),
        },
    );

    // Libraries up to their metadata, against the same crates' checks.
    let slow = a.slowdown.as_ref().unwrap();
    assert!(slow.baseline_check);
    assert!(close(slow.baseline_wall, 8.51));
    assert_eq!(slow.compared, 5);
    assert!(close(slow.median, 0.885 / 1.1));
    assert_eq!(slow.top[0].unit, "regex-automata");
    assert_eq!(slow.top[0].before, Some(1.37));

    let rebuilt = a.rebuilt.as_ref().unwrap();
    assert_eq!(rebuilt.crates, 3);
    assert_eq!(
        rebuilt.roots,
        ["nextest-libtest-json", "rustc-self-profile"]
    );
    assert!(close(rebuilt.busy, 4.93));
    // cargo-faster waited for bpaf-kit, not a workspace crate.
    assert_eq!(path_edges(&a, &ws).len(), 0);

    let json = serde_json::to_value(&a).unwrap();
    assert_eq!(json["critical_path"][1]["waited"], "meta");
    assert_eq!(json["linked"]["slowest"][0]["took_s"], 1.05);
    assert_eq!(json["slowdown"]["top"][0]["before_s"], 1.37);
    assert_eq!(json["jobs"]["ncpu"], 32);
}

#[test]
fn finds_the_check_before_a_build() {
    let dir = tempfile::tempdir().unwrap();
    let reports = dir.path().join("cargo-timings");
    std::fs::create_dir(&reports).unwrap();
    let src = format!("{}/tests/fixtures/cargo-faster", env!("CARGO_MANIFEST_DIR"));
    let check = reports.join("cargo-timing-20261117T115251648Z-9af24f2b821175fd.html");
    let build = reports.join("cargo-timing-20261117T115300172Z-9af24f2b821175fd.html");
    std::fs::copy(format!("{src}/check.html"), &check).unwrap();
    std::fs::copy(format!("{src}/build.html"), &build).unwrap();
    std::fs::write(
        reports.join("cargo-timing-20261117T115400000Z-9af24f2b821175fd.html"),
        "<script>\nconst UNIT_DATA = [\n];\n</script>",
    )
    .unwrap();
    assert_eq!(
        cargo_timings_parse::newest_report(dir.path()),
        Some(build.clone())
    );
    let (found, t) = check_before(&build).unwrap();
    assert_eq!(found, check);
    assert_eq!(t.units.len(), 113);
    assert!(check_before(&check).is_none());
}
