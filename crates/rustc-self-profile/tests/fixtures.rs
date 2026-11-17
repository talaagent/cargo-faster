//! Tests on a real self-profile: `cargo check` of this repository's
//! `nextest-libtest-json` crate, trimmed (see `tests/fixtures/README.md`).

use rustc_self_profile::{Analysis, Profile, analyze};

fn fixture() -> Profile {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cargo-faster/nextest-libtest-json-check.json"
    );
    let text = std::fs::read_to_string(path).expect("fixture");
    serde_json::from_str(&text).expect("profile json")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn reads_the_trimmed_profile() {
    let p = fixture();
    assert_eq!(p.intervals.len(), 1098);
    assert_eq!(p.labels.len(), 107);
    assert_eq!(p.args.len(), 339);
    assert!(p.cmd.starts_with("rustc --crate-name nextest_libtest_json"));
    assert!(p.cmd.contains("-Zself-profile"));
    let first = &p.intervals[0];
    assert_eq!(p.label(first), "target_machine_factory_inner");
}

#[test]
fn analyzes_a_real_check() {
    let a = analyze(&fixture());
    assert!(close(a.wall_secs, 0.113_291_123));
    assert!(close(a.total_secs, 0.112_768_898));
    assert!(close(a.profiler_secs, 0.012_625_682));

    let q = &a.queries[0];
    assert_eq!((q.name.as_str(), q.count), ("mir_borrowck", 135));
    assert!(close(q.secs, 0.026_525));
    assert_eq!(a.queries[1].name, "typeck_root");

    assert_eq!(a.body_count, 243);
    assert!(close(a.body_secs, 0.076_053_813));
    assert_eq!(a.modules[0].name, "nextest_libtest_json");
    assert_eq!(a.modules[0].count, 118);
    assert_eq!(
        a.items[0].name,
        "nextest_libtest_json::_[5]::{{impl}}::deserialize::{{impl}}[2]::visit_map"
    );

    let derives: Vec<(&str, usize)> = a
        .proc_macros
        .iter()
        .map(|c| (c.name.as_str(), c.count))
        .collect();
    assert_eq!(
        derives,
        [("#[derive(Deserialize)]", 8), ("#[derive(Serialize)]", 8)]
    );

    let o = &a.obligations[0];
    assert_eq!(o.self_ty, "__A");
    assert_eq!(o.trait_path, "_::_serde::de::SeqAccess<'_>");
    assert_eq!((o.count, o.bodies), (18, 5));
    assert_eq!(a.trait_solving_by_category[0].name, "types from __A");
    assert_eq!(a.heavy_types.len(), 0);
    assert_eq!(a.async_trait_obligations, 0);
    assert!(a.codegen_secs < 0.001, "a check barely codegens");
}

#[test]
fn analysis_round_trips_through_json() {
    let a = analyze(&fixture());
    let json = serde_json::to_string(&a).expect("serialize");
    let back: Analysis = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, a);
}

#[test]
fn profile_round_trips_and_rebuilds_its_indexes() {
    let p = fixture();
    let json = serde_json::to_string(&p).expect("serialize");
    let mut back: Profile = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.intervals, p.intervals);
    // Pushing a known label reuses its id rather than adding a new one.
    back.push("typeck_root", Some("x::y"), 0, 1, 9);
    assert_eq!(back.labels.len(), p.labels.len());
    assert_eq!(
        back.label(back.intervals.last().expect("pushed")),
        "typeck_root"
    );
}

#[test]
fn load_reports_a_missing_profile() {
    let err = Profile::load(std::path::Path::new("/nonexistent/app-1")).expect_err("no file");
    assert!(
        err.to_string().starts_with("reading /nonexistent/app-1: "),
        "{err}"
    );
}
