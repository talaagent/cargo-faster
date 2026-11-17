//! Parse real nextest output (see `tests/fixtures/README.md`).

use nextest_libtest_json::{Event, Run, SuiteStatus, TestName, TestStatus, parse, parse_line};

const CARGO_FASTER: &str = include_str!("fixtures/cargo-faster/nextest-libtest-json-plus.jsonl");
const FAILING: &str = include_str!("fixtures/cargo-faster/failing-retried.jsonl");

#[test]
fn every_line_is_an_event() {
    for line in CARGO_FASTER.lines().chain(FAILING.lines()) {
        assert!(parse_line(line).is_some(), "{line}");
    }
}

#[test]
fn cargo_faster_workspace_run() {
    let run = Run::parse(CARGO_FASTER);
    assert_eq!(run.tests.len(), 110);
    assert_eq!(run.ignored, 0);
    assert_eq!(run.failed().count(), 0);
    let slowest = run
        .tests
        .iter()
        .max_by(|a, b| a.secs.total_cmp(&b.secs))
        .unwrap();
    assert_eq!(slowest.binary, "cargo-faster::cli");
    assert_eq!(slowest.name, "a_failed_build_exits_3");
    assert!((slowest.secs - 1.150_754_781).abs() < 1e-9);

    assert_eq!(run.suites.len(), 6);
    let lib = &run.suites[0];
    assert_eq!(lib.event, SuiteStatus::Ok);
    assert_eq!(lib.passed, Some(99));
    let bin = lib.nextest.as_ref().unwrap();
    assert_eq!(bin.krate, "cargo-faster");
    assert_eq!(bin.test_binary, "cargo_faster");
    assert_eq!(bin.kind, "lib");
    let passed: u64 = run.suites.iter().filter_map(|s| s.passed).sum();
    assert_eq!(passed, 110);
}

#[test]
fn failing_retried_run() {
    let events: Vec<Event> = parse(FAILING).collect();
    assert_eq!(events.len(), 10);
    let failed = events
        .iter()
        .find_map(|e| match e {
            Event::Test(t) if t.event == TestStatus::Failed => Some(t),
            _ => None,
        })
        .unwrap();
    let name: TestName<'_> = failed.name();
    assert_eq!((name.krate, name.binary), ("demo", "demo"));
    assert_eq!((name.test, name.attempts), ("tests::fails", Some(2)));
    assert!(failed.stdout.as_deref().unwrap().contains("math is broken"));

    let run = Run::parse(FAILING);
    assert_eq!(run.tests.len(), 2);
    assert_eq!(run.ignored, 1);
    let f: Vec<_> = run.failed().collect();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].name, "tests::fails#2");
    assert_eq!(run.suites[1].event, SuiteStatus::Failed);
    assert_eq!(run.suites[1].filtered_out, Some(1));
}

#[cfg(feature = "std")]
#[test]
fn reads_from_a_reader() {
    let events = nextest_libtest_json::parse_reader(CARGO_FASTER.as_bytes()).unwrap();
    assert_eq!(events.len(), CARGO_FASTER.lines().count());
}
