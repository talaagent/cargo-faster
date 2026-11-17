//! Parse the output of `cargo nextest run --message-format libtest-json`
//! (or `libtest-json-plus`) into typed events and a run summary.
//!
//! The format is **experimental** in nextest: it needs
//! `NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1` in the environment, and its
//! fields may change between nextest releases. This crate parses what
//! nextest 0.9 writes; unknown fields are ignored and unknown event names
//! parse as [`TestStatus::Other`] / [`SuiteStatus::Other`].
//!
//! Each line of the output is one JSON object, either about a test binary
//! (a *suite*) or about one test:
//!
//! ```text
//! {"type":"suite","event":"started","test_count":2,"nextest":{"crate":"app","test_binary":"app","kind":"lib"}}
//! {"type":"test","event":"started","name":"app::app$tests::adds"}
//! {"type":"test","event":"ok","name":"app::app$tests::adds","exec_time":0.004}
//! {"type":"test","event":"failed","name":"app::app$tests::fails","exec_time":0.01,"stdout":"..."}
//! {"type":"suite","event":"failed","passed":1,"failed":1,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.02,"nextest":{...}}
//! ```
//!
//! `libtest-json-plus` adds the `nextest` object to suite events (the
//! package, binary and kind of the test binary); plain `libtest-json`
//! leaves it out. Test names are `crate::binary$test`, with `@stress-N`
//! after the binary in stress runs and `#N` after the test when it was
//! retried.
//!
//! # Example
//!
//! ```
//! use nextest_libtest_json::{Event, Run, TestStatus};
//!
//! let out = r#"{"type":"suite","event":"started","test_count":2}
//! {"type":"test","event":"started","name":"app::app$tests::adds"}
//! {"type":"test","event":"ok","name":"app::app$tests::adds","exec_time":0.004}
//! {"type":"test","event":"ignored","name":"app::app$tests::later"}
//! {"type":"suite","event":"ok","passed":1,"failed":0,"ignored":1,"measured":0,"filtered_out":0,"exec_time":0.01}
//! "#;
//!
//! // Every event, in order (lines that aren't events are skipped).
//! let events: Vec<Event> = nextest_libtest_json::parse(out).collect();
//! assert_eq!(events.len(), 5);
//! let Event::Test(t) = &events[2] else { panic!() };
//! assert_eq!(t.event, TestStatus::Ok);
//! assert_eq!(t.name().binary_id, "app::app");
//! assert_eq!(t.name().test, "tests::adds");
//!
//! // Or just the results.
//! let run = Run::parse(out);
//! assert_eq!(run.tests.len(), 1);
//! assert_eq!(run.tests[0].name, "tests::adds");
//! assert_eq!(run.ignored, 1);
//! ```
//!
//! # Features
//!
//! The crate is `#![no_std]` (it needs `alloc`). The default `std` feature
//! adds [`parse_reader`] for reading from a [`std::io::BufRead`].

#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// One line of libtest-json output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[non_exhaustive]
pub enum Event {
    /// About a test binary: it started, or it finished with these totals.
    Suite(SuiteEvent),
    /// About one test.
    Test(TestEvent),
}

/// What happened to a test binary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SuiteStatus {
    /// The binary started running tests.
    Started,
    /// Every test that ran passed.
    Ok,
    /// At least one test failed.
    Failed,
    /// An event name this crate doesn't know.
    #[serde(other)]
    #[default]
    Other,
}

/// A `"type":"suite"` event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SuiteEvent {
    /// `started`, `ok` or `failed`.
    pub event: SuiteStatus,
    /// Tests that will run (`started` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_count: Option<u64>,
    /// Tests that passed (when finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<u64>,
    /// Tests that failed (when finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<u64>,
    /// Tests that were ignored (when finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored: Option<u64>,
    /// Benchmarks measured: always 0 from nextest (when finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<u64>,
    /// Tests left out by a filter, or not run because the run was
    /// cancelled (when finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered_out: Option<u64>,
    /// Sum of the binary's test times, in seconds (when finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_time: Option<f64>,
    /// What the binary is (`libtest-json-plus` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nextest: Option<Binary>,
}

/// The `nextest` object `libtest-json-plus` adds to suite events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Binary {
    /// The package the binary belongs to.
    #[serde(rename = "crate")]
    pub krate: String,
    /// The binary's name (`my_crate` for the library's unit tests, the
    /// file stem for an integration test).
    pub test_binary: String,
    /// `lib`, `test`, `bin`, `bench`, `example`, `proc-macro`, ...
    pub kind: String,
    /// The sub-run of a stress run (`--stress-count`), from 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stress_index: Option<u64>,
    /// How many sub-runs the stress run has, when it is a fixed count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stress_total: Option<u64>,
}

/// What happened to a test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TestStatus {
    /// It started.
    Started,
    /// It passed (also a flaky test that passed on a retry).
    Ok,
    /// It failed, timed out, or could not be run.
    Failed,
    /// It is `#[ignore]`d and didn't run.
    Ignored,
    /// libtest's "still running after 60s" warning. nextest doesn't write
    /// it; a slow-timeout failure is `failed` with `reason`
    /// `time limit exceeded`.
    Timeout,
    /// An event name this crate doesn't know.
    #[serde(other)]
    #[default]
    Other,
}

/// A `"type":"test"` event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TestEvent {
    /// `started`, `ok`, `failed` or `ignored`.
    pub event: TestStatus,
    /// `crate::binary$test`; see [`TestEvent::name`].
    #[serde(default)]
    pub name: String,
    /// Wall time of the test's process, in seconds (finished tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_time: Option<f64>,
    /// The failed test's output, stdout and stderr together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// Why a test counts as failed without output of its own:
    /// `time limit exceeded`, `flaky test treated as failure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// libtest's extra message (an ignore reason, a `should_panic`
    /// mismatch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl TestEvent {
    /// The parts of the test's name.
    #[must_use]
    pub fn name(&self) -> TestName<'_> {
        TestName::parse(&self.name)
    }

    /// Whether the test finished: passed or failed.
    #[must_use]
    pub fn finished(&self) -> bool {
        matches!(
            self.event,
            TestStatus::Ok | TestStatus::Failed | TestStatus::Timeout
        )
    }
}

/// The parts of a nextest test name,
/// `crate::binary[@stress-N]$test[#attempts]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TestName<'a> {
    /// `crate::binary`, with any `@stress-N` suffix: everything before the
    /// `$` (empty when there is none).
    pub binary_id: &'a str,
    /// The package (before the first `::` of `binary_id`).
    pub krate: &'a str,
    /// The test binary (after it, without `@stress-N`).
    pub binary: &'a str,
    /// The stress sub-run, from `@stress-N`.
    pub stress_index: Option<u64>,
    /// The test's path inside the binary, as libtest names it, without
    /// the `#N` retry suffix.
    pub test: &'a str,
    /// How many times the test ran, from the `#N` suffix nextest adds to
    /// retried tests.
    pub attempts: Option<u32>,
}

impl<'a> TestName<'a> {
    /// Split `crate::binary[@stress-N]$test[#N]`. A name without `$` is
    /// all test path.
    #[must_use]
    pub fn parse(full: &'a str) -> Self {
        let (binary_id, rest) = full.split_once('$').unwrap_or(("", full));
        let (test, attempts) = match rest.rsplit_once('#') {
            Some((t, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
                (t, n.parse().ok())
            }
            _ => (rest, None),
        };
        let (id, stress_index) = match binary_id.rsplit_once("@stress-") {
            Some((id, n)) => match n.parse() {
                Ok(n) => (id, Some(n)),
                Err(_) => (binary_id, None),
            },
            None => (binary_id, None),
        };
        let (krate, binary) = id.split_once("::").unwrap_or((id, id));
        Self {
            binary_id,
            krate,
            binary,
            stress_index,
            test,
            attempts,
        }
    }
}

/// Parse one line. `None` for a line that isn't a libtest-json event
/// (cargo's or a build script's output, a blank line, invalid JSON).
#[must_use]
pub fn parse_line(line: &str) -> Option<Event> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    serde_json::from_str(line).ok()
}

/// Every event in a run's output, in order. Lines that aren't events are
/// skipped.
pub fn parse(output: &str) -> impl Iterator<Item = Event> + '_ {
    output.lines().filter_map(parse_line)
}

/// Every event read from `reader` (nextest's stdout, a saved file). Lines
/// that aren't events are skipped.
///
/// # Errors
///
/// When reading fails.
#[cfg(feature = "std")]
pub fn parse_reader(reader: impl std::io::BufRead) -> std::io::Result<Vec<Event>> {
    let mut events = Vec::new();
    for line in reader.lines() {
        events.extend(parse_line(&line?));
    }
    Ok(events)
}

/// One finished test.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TestResult {
    /// `crate::binary`, as nextest names it (with `@stress-N` in a stress
    /// run).
    pub binary: String,
    /// The test's path inside the binary (with nextest's `#N` suffix when
    /// it was retried).
    pub name: String,
    /// Wall time of the test's process, in seconds.
    pub secs: f64,
    /// Whether it passed.
    pub passed: bool,
}

impl TestResult {
    /// A result, for building runs by hand.
    #[must_use]
    pub fn new(
        binary: impl Into<String>,
        name: impl Into<String>,
        secs: f64,
        passed: bool,
    ) -> Self {
        Self {
            binary: binary.into(),
            name: name.into(),
            secs,
            passed,
        }
    }
}

/// What a run reported: every finished test, the ignored count, and each
/// test binary's totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Run {
    /// Every test that finished, in the order they finished.
    pub tests: Vec<TestResult>,
    /// Tests that were ignored.
    pub ignored: usize,
    /// The finished suite events (one per test binary, per stress
    /// sub-run).
    pub suites: Vec<SuiteEvent>,
}

impl Run {
    /// Summarize a run's output (see [`parse`]).
    #[must_use]
    pub fn parse(output: &str) -> Self {
        parse(output).collect()
    }

    /// Add one event.
    pub fn push(&mut self, event: Event) {
        match event {
            Event::Test(t) if t.finished() => {
                let (binary, name) = t.name.split_once('$').unwrap_or(("", &t.name));
                self.tests.push(TestResult {
                    binary: binary.to_owned(),
                    name: name.to_owned(),
                    secs: t.exec_time.unwrap_or_default(),
                    passed: t.event == TestStatus::Ok,
                });
            }
            Event::Test(t) if t.event == TestStatus::Ignored => self.ignored += 1,
            Event::Suite(s) if matches!(s.event, SuiteStatus::Ok | SuiteStatus::Failed) => {
                self.suites.push(s);
            }
            _ => {}
        }
    }

    /// Tests that failed.
    pub fn failed(&self) -> impl Iterator<Item = &TestResult> {
        self.tests.iter().filter(|t| !t.passed)
    }

    /// The sum of every test's time, in seconds.
    #[must_use]
    pub fn test_secs(&self) -> f64 {
        self.tests.iter().map(|t| t.secs).sum()
    }
}

impl FromIterator<Event> for Run {
    fn from_iter<I: IntoIterator<Item = Event>>(iter: I) -> Self {
        let mut run = Self::default();
        iter.into_iter().for_each(|e| run.push(e));
        run
    }
}

impl Extend<Event> for Run {
    fn extend<I: IntoIterator<Item = Event>>(&mut self, iter: I) {
        iter.into_iter().for_each(|e| self.push(e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: &str = r#"{"type":"suite","event":"started","test_count":3,"nextest":{"crate":"app","test_binary":"app","kind":"lib"}}
{"type":"test","event":"started","name":"app::app$db::migrates"}
{"type":"test","event":"ok","name":"app::app$db::migrates","exec_time":40.5}
{"type":"test","event":"ok","name":"app::app$util::adds","exec_time":0.004}
{"type":"test","event":"failed","name":"app::it$flaky","exec_time":1.25,"stdout":"boom"}
{"type":"test","event":"ignored","name":"app::it$later"}
{"type":"suite","event":"ok","passed":2,"failed":0,"ignored":0,"measured":0,"filtered_out":0,"exec_time":40.6}
not json
"#;

    #[test]
    fn summarizes_a_run() {
        let r = Run::parse(RUN);
        assert_eq!(r.tests.len(), 3);
        assert_eq!(r.ignored, 1);
        assert_eq!(
            r.tests[0],
            TestResult::new("app::app", "db::migrates", 40.5, true)
        );
        assert!(!r.tests[2].passed);
        assert_eq!(r.failed().count(), 1);
        assert_eq!(r.suites.len(), 1);
        assert_eq!(r.suites[0].passed, Some(2));
    }

    #[test]
    fn events_round_trip() {
        for e in parse(RUN) {
            let json = serde_json::to_string(&e).unwrap();
            assert_eq!(parse_line(&json), Some(e));
        }
        let Some(Event::Suite(s)) = parse(RUN).next() else {
            panic!()
        };
        let b = s.nextest.unwrap();
        assert_eq!((b.krate.as_str(), b.kind.as_str()), ("app", "lib"));
    }

    #[test]
    fn unknown_events_and_fields() {
        let e = parse_line(r#"{"type":"test","event":"bench","name":"a::b$c","new":1}"#);
        let Some(Event::Test(t)) = e else { panic!() };
        assert_eq!(t.event, TestStatus::Other);
        assert_eq!(parse_line(r#"{"type":"other"}"#), None);
        assert_eq!(parse_line("   Compiling app"), None);
    }

    #[test]
    fn names() {
        let n = TestName::parse("app::it@stress-3$mod::t#2");
        assert_eq!(n.binary_id, "app::it@stress-3");
        assert_eq!((n.krate, n.binary), ("app", "it"));
        assert_eq!(n.stress_index, Some(3));
        assert_eq!((n.test, n.attempts), ("mod::t", Some(2)));
        let n = TestName::parse("plain");
        assert_eq!((n.binary_id, n.test, n.attempts), ("", "plain", None));
        assert_eq!(TestName::parse("a::b$x#y").test, "x#y");
    }
}
