//! `cargo faster tests --run`: time building the tests, run them with
//! nextest's libtest-json output, and report where the time goes.
//!
//! Parsing and analysis are pure ([`parse`], [`section`]); running nextest
//! lives in the binary.

use crate::finding::{Finding, Section, Severity, table};
use crate::fmt::{ellipsize, secs};
use alloc::collections::BTreeMap;

/// How many of the slowest tests and binaries to list.
pub const TOP: usize = 15;

pub use nextest_libtest_json::{Run, TestResult as TestTime};

/// Parse `cargo nextest run --message-format libtest-json[-plus]` output
/// (needs `NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1`). Lines that aren't test
/// results are ignored.
#[must_use]
pub fn parse(stdout: &str) -> Run {
    Run::parse(stdout)
}

/// A nextest duration (`"60s"`, `"2m"`, `"500ms"`, `"1m 30s"`) in seconds.
#[must_use]
pub fn duration_secs(s: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut any = false;
    for part in s.split_whitespace() {
        let split = part.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let (n, unit) = part.split_at(split);
        let n: f64 = n.parse().ok()?;
        total += n * match unit {
            "ms" => 0.001,
            "s" | "sec" | "secs" => 1.0,
            "m" | "min" | "mins" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        any = true;
    }
    any.then_some(total)
}

/// The value of `--flag v` or `--flag=v` in nextest arguments.
fn arg_value(args: &[String], flags: &[&str]) -> Option<String> {
    args.iter().enumerate().find_map(|(i, a)| {
        flags.iter().find_map(|f| {
            if a == f {
                args.get(i + 1).cloned()
            } else {
                a.strip_prefix(f)?.strip_prefix('=').map(str::to_owned)
            }
        })
    })
}

/// A `test-threads` value: a count, `num-cpus`, or negative (CPUs minus n).
fn threads_value(v: &str, cpus: usize) -> Option<usize> {
    if v == "num-cpus" {
        return Some(cpus);
    }
    let n: i64 = v.trim_matches('"').parse().ok()?;
    let cpus = i64::try_from(cpus).unwrap_or(i64::MAX);
    usize::try_from(if n < 0 { (cpus + n).max(1) } else { n }).ok()
}

/// The nextest profile a run uses: `--profile`/`-P`, else `NEXTEST_PROFILE`,
/// else `default`.
#[must_use]
pub fn profile(args: &[String], env_profile: Option<&str>) -> String {
    arg_value(args, &["--profile", "-P"])
        .or_else(|| env_profile.map(str::to_owned))
        .unwrap_or_else(|| "default".to_owned())
}

/// The threads a run gets, and why, in nextest's order: `--test-threads`
/// or `-j`, `NEXTEST_TEST_THREADS`, the profile's `test-threads` (falling
/// back to `default`), the CPU count.
#[must_use]
pub fn threads(
    args: &[String],
    env_threads: Option<&str>,
    config: Option<&toml::Table>,
    profile: &str,
    cpus: usize,
) -> (usize, String) {
    if let Some(n) =
        arg_value(args, &["--test-threads", "-j"]).and_then(|v| threads_value(&v, cpus))
    {
        return (n, "--test-threads".to_owned());
    }
    if let Some(n) = env_threads.and_then(|v| threads_value(v, cpus)) {
        return (n, "NEXTEST_TEST_THREADS".to_owned());
    }
    for p in [profile, "default"] {
        if let Some(n) = profile_key(config, p, "test-threads").and_then(|v| match v {
            toml::Value::Integer(n) => threads_value(&n.to_string(), cpus),
            toml::Value::String(s) => threads_value(s, cpus),
            _ => None,
        }) {
            return (n, format!("profile.{p}.test-threads"));
        }
    }
    (cpus, "CPUs".to_owned())
}

/// The profile's `slow-timeout` period in seconds (nextest's default is 60s).
#[must_use]
pub fn slow_timeout(config: Option<&toml::Table>, profile: &str) -> f64 {
    [profile, "default"]
        .into_iter()
        .find_map(|p| {
            let v = profile_key(config, p, "slow-timeout")?;
            let period = v.get("period").unwrap_or(v);
            duration_secs(period.as_str()?)
        })
        .unwrap_or(60.0)
}

fn profile_key<'a>(
    config: Option<&'a toml::Table>,
    profile: &str,
    key: &str,
) -> Option<&'a toml::Value> {
    config?.get("profile")?.get(profile)?.get(key)
}

/// Everything the report needs.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// Wall time of `cargo nextest run --no-run`, if it was measured.
    pub build_secs: Option<f64>,
    /// Wall time of the test run.
    pub run_secs: f64,
    /// The test threads in effect, and where that came from.
    pub threads: (usize, String),
    /// The profile's `slow-timeout` period, in seconds.
    pub slow_timeout: f64,
    /// The parsed run.
    pub run: Run,
}

/// Per-binary totals.
struct Binary<'a> {
    name: &'a str,
    tests: usize,
    total: f64,
    fastest: f64,
    slowest: f64,
}

fn binaries(tests: &[TestTime]) -> Vec<Binary<'_>> {
    let mut by: BTreeMap<&str, Binary<'_>> = BTreeMap::new();
    for t in tests {
        let b = by.entry(t.binary.as_str()).or_insert(Binary {
            name: &t.binary,
            tests: 0,
            total: 0.0,
            fastest: f64::INFINITY,
            slowest: 0.0,
        });
        b.tests += 1;
        b.total += t.secs;
        b.fastest = b.fastest.min(t.secs);
        b.slowest = b.slowest.max(t.secs);
    }
    let mut v: Vec<_> = by.into_values().collect();
    v.sort_by(|a, b| b.total.total_cmp(&a.total));
    v
}

/// Tests at half the slow-timeout or more, and the ones killed by it.
/// `slowest` is sorted slowest first.
fn timeout_finding(slowest: &[&TestTime], slow_timeout: f64) -> Option<Finding> {
    let near: Vec<&&TestTime> = slowest
        .iter()
        .take_while(|t| t.secs >= slow_timeout * 0.5)
        .collect();
    if near.is_empty() {
        return None;
    }
    // A failure past the slow-timeout is almost always nextest's
    // terminate-after killing a hung test.
    let hung = |t: &TestTime| !t.passed && t.secs >= slow_timeout;
    let hung_secs: f64 = near.iter().filter(|t| hung(t)).map(|t| t.secs).sum();
    let listed = near
        .iter()
        .take(5)
        .map(|t| {
            format!(
                "{}${} ({}{})",
                t.binary,
                t.name,
                if hung(t) { "failed after " } else { "" },
                secs(t.secs)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let finding = if hung_secs > 0.0 {
        Finding::new("tests-near-slow-timeout", Severity::Medium, format!("{} test(s) ran into the slow-timeout ({})", near.iter().filter(|t| hung(t)).count(), secs(slow_timeout)), "measuring-a-test-run")
            .saw(format!("{listed}. A failure that long is nextest's terminate-after killing a test that hung; the run waited {} for it.", secs(hung_secs)))
            .why("A hung test holds a thread until the timeout and usually ends the run last, so the whole suite waits for it. Under load, the slow tests are the ones that cross the line.")
            .fix("Find out why it hangs (`cargo nextest run TEST --no-capture`): a missing server, a lock, a deadlock. Then make it fail fast instead, with its own shorter slow-timeout and terminate-after in a per-test override in .config/nextest.toml.")
    } else {
        Finding::new("tests-near-slow-timeout", Severity::Low, format!("{} test(s) take at least half the slow-timeout ({})", near.len(), secs(slow_timeout)), "measuring-a-test-run")
            .saw(format!("The slowest: {listed}."))
            .why("A test that is already slow is the one a busy machine pushes past the timeout, and the whole run waits for it.")
            .fix("Make the test cheaper, split it, or give it its own slow-timeout with a per-test override in .config/nextest.toml.")
    };
    Some(finding)
}

/// The `test run` section.
#[must_use]
pub fn section(i: &Inputs) -> Section {
    let mut s = Section::new("test run");
    let tests = &i.run.tests;
    let failed = tests.iter().filter(|t| !t.passed).count();
    let test_secs: f64 = tests.iter().map(|t| t.secs).sum();
    let (threads, threads_src) = (&i.threads.0, &i.threads.1);
    if let Some(b) = i.build_secs {
        s.fact("build (--no-run)", secs(b));
    }
    s.fact("run", secs(i.run_secs));
    s.fact(
        "tests",
        format!(
            "{} run, {failed} failed, {} skipped",
            tests.len(),
            i.run.ignored
        ),
    );
    s.fact("test threads", format!("{threads} ({threads_src})"));
    let capacity = i.run_secs * (*threads).max(1) as f64;
    let used = if capacity > 0.0 {
        test_secs / capacity
    } else {
        0.0
    };
    s.fact(
        "test-seconds",
        format!(
            "{} over {} × {threads} threads ({:.0}% busy)",
            secs(test_secs),
            secs(i.run_secs),
            used * 100.0
        ),
    );
    let mut slowest: Vec<&TestTime> = tests.iter().collect();
    slowest.sort_by(|a, b| b.secs.total_cmp(&a.secs));
    if !slowest.is_empty() {
        let rows: Vec<Vec<String>> = slowest
            .iter()
            .take(TOP)
            .map(|t| {
                vec![
                    secs(t.secs),
                    ellipsize(&t.binary, 30),
                    ellipsize(&t.name, 70),
                ]
            })
            .collect();
        s.details.push(format!(
            "slowest tests\n{}",
            table(&["took", "binary", "test"], &rows)
        ));
    }
    let bins = binaries(tests);
    if !bins.is_empty() {
        let rows: Vec<Vec<String>> = bins
            .iter()
            .take(TOP)
            .map(|b| {
                vec![
                    secs(b.total),
                    b.tests.to_string(),
                    secs(b.slowest),
                    secs(b.fastest),
                    ellipsize(b.name, 50),
                ]
            })
            .collect();
        s.details.push(format!(
            "test binaries by total test time (fastest ≈ the binary's per-test startup)\n{}",
            table(&["total", "tests", "slowest", "fastest", "binary"], &rows)
        ));
    }
    if let Some(b) = i.build_secs
        && b >= 60.0
        && b > i.run_secs * 2.0
    {
        s.extend([Finding::new("test-build-dominates", Severity::Medium, format!("Building the tests takes {} and running them {}", secs(b), secs(i.run_secs)), "measuring-a-test-run")
            .saw(format!("`cargo nextest run --no-run` took {}; the run itself took {}.", secs(b), secs(i.run_secs)))
            .why("Faster tests won't help much here: most of the wait is compiling and linking test binaries. Every integration test file and every crate's unit tests are a separate binary that links the whole dependency graph.")
            .fix("Read the timings section below for the units on the critical path. Common fixes: fewer test binaries (one tests/ binary per crate), less debug info, a faster linker, and splitting the crate whose `lib (test)` unit dominates.")]);
    }
    s.extend(timeout_finding(&slowest, i.slow_timeout));
    let longest = slowest.first().map_or(0.0, |t| t.secs);
    if i.run_secs >= 10.0 && *threads > 1 && used < 0.5 {
        s.extend([Finding::new("test-threads-idle", Severity::Info, format!("Test threads were busy {:.0}% of the run", used * 100.0), "measuring-a-test-run")
            .saw(format!("{} of test time over {} of wall time with {threads} threads. The slowest test took {}.", secs(test_secs), secs(i.run_secs), secs(longest)))
            .why("When the threads sit idle, the run is waiting on a few long tests at the end, on a test group with a low thread limit, or on something outside the tests (a setup script, the runner).")
            .fix(if longest >= i.run_secs * 0.5 { "The slowest test is most of the run: split it or start it earlier (nextest runs tests in binary order; a priority override can move it)." } else { "Look for test groups with low max-threads and for long setup scripts in .config/nextest.toml." })]);
    }
    s
}

#[cfg(test)]
mod unit {
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
    fn parses_libtest_json() {
        let r = parse(RUN);
        assert_eq!(r.tests.len(), 3);
        assert_eq!(r.ignored, 1);
        assert_eq!(
            r.tests[0],
            TestTime::new("app::app", "db::migrates", 40.5, true)
        );
        assert!(!r.tests[2].passed);
    }

    #[test]
    fn threads_and_timeout() {
        let cfg: toml::Table = "[profile.default]\nslow-timeout = { period = \"30s\", terminate-after = 2 }\n[profile.gate]\ntest-threads = 8\nslow-timeout = \"2m\"\n".parse().unwrap();
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let p = profile(&args(&["--profile", "gate"]), None);
        assert_eq!(p, "gate");
        assert_eq!(
            threads(&args(&[]), None, Some(&cfg), &p, 32),
            (8, "profile.gate.test-threads".into())
        );
        assert_eq!(
            threads(&args(&[]), Some("4"), Some(&cfg), &p, 32),
            (4, "NEXTEST_TEST_THREADS".into())
        );
        assert_eq!(
            threads(&args(&["-j=2"]), Some("4"), Some(&cfg), &p, 32),
            (2, "--test-threads".into())
        );
        assert_eq!(
            threads(&args(&[]), None, Some(&cfg), "default", 32),
            (32, "CPUs".into())
        );
        assert_eq!(
            threads(&args(&["--test-threads", "-2"]), None, None, "default", 32).0,
            30
        );
        assert_eq!(slow_timeout(Some(&cfg), "gate"), 120.0);
        assert_eq!(slow_timeout(Some(&cfg), "ci"), 30.0);
        assert_eq!(slow_timeout(None, "default"), 60.0);
        assert_eq!(profile(&args(&[]), Some("ci")), "ci");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_secs("60s"), Some(60.0));
        assert_eq!(duration_secs("1m 30s"), Some(90.0));
        assert_eq!(duration_secs("500ms"), Some(0.5));
        assert_eq!(duration_secs("soon"), None);
    }

    #[test]
    fn report() {
        let s = section(&Inputs {
            build_secs: Some(558.0),
            run_secs: 66.0,
            threads: (8, "--test-threads".into()),
            slow_timeout: 60.0,
            run: parse(RUN),
        });
        let ids: Vec<_> = s.findings.iter().map(|f| f.id).collect();
        assert_eq!(
            ids,
            [
                "test-build-dominates",
                "tests-near-slow-timeout",
                "test-threads-idle"
            ]
        );
        assert!(s.findings[1].saw.contains("app::app$db::migrates (40.5s)"));
        assert!(s.details[0].starts_with("slowest tests"));
        let fact = |k: &str| {
            s.facts
                .iter()
                .find(|f| f.key == k)
                .map(|f| f.value.clone())
                .unwrap_or_default()
        };
        assert_eq!(fact("tests"), "3 run, 1 failed, 1 skipped");
        assert!(
            fact("test-seconds").contains("(8% busy)"),
            "{}",
            fact("test-seconds")
        );
    }

    #[test]
    fn hung_test_is_medium() {
        let run = parse(&RUN.replace(
            r#""app::it$flaky","exec_time":1.25"#,
            r#""app::it$flaky","exec_time":600.0"#,
        ));
        let s = section(&Inputs {
            build_secs: Some(30.0),
            run_secs: 660.0,
            threads: (8, "--test-threads".into()),
            slow_timeout: 60.0,
            run,
        });
        let f = s
            .findings
            .iter()
            .find(|f| f.id == "tests-near-slow-timeout")
            .unwrap();
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.title.starts_with("1 test(s) ran into"), "{}", f.title);
        assert!(
            f.saw.contains("app::it$flaky (failed after 600s)"),
            "{}",
            f.saw
        );
    }

    #[test]
    fn quick_run_is_quiet() {
        let s = section(&Inputs {
            build_secs: Some(20.0),
            run_secs: 5.0,
            threads: (8, "num-cpus".into()),
            slow_timeout: 60.0,
            run: parse(RUN.replace("40.5", "0.5").as_str()),
        });
        assert_eq!(
            s.findings.len(),
            0,
            "{:?}",
            s.findings.iter().map(|f| f.id).collect::<Vec<_>>()
        );
    }
}
