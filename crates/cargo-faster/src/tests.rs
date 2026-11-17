//! Tests: nextest, target runners, thread limits, database-backed tests,
//! and the number of test binaries.

use crate::context::{Context, Metadata, on_path};
use crate::finding::{Finding, Section, Severity};
use std::path::Path;

/// Inputs beyond the [`Context`].
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// `.config/nextest.toml`, parsed.
    pub nextest_config: Option<toml::Table>,
    /// Whether `cargo-nextest` is on PATH.
    pub nextest_installed: bool,
    /// `#[sqlx::test]` attributes found in the workspace.
    pub sqlx_tests: usize,
    /// Test binaries: lib/bin unit tests plus each integration test file.
    pub test_binaries: usize,
    /// Integration test files (`tests/*.rs`) across members.
    pub integration_files: usize,
}

impl Inputs {
    /// Gather from the workspace on disk.
    #[must_use]
    pub fn gather(ctx: &Context) -> Self {
        let root = ctx.root();
        let nextest_config = std::fs::read_to_string(root.join(".config/nextest.toml"))
            .ok()
            .and_then(|t| t.parse().ok());
        let (test_binaries, integration_files) =
            ctx.metadata.as_ref().map_or((0, 0), count_test_binaries);
        let mut sqlx_tests = 0;
        if let Some(m) = &ctx.metadata {
            for p in m.members() {
                if let Some(dir) = p.manifest_path.parent() {
                    sqlx_tests += count_in_rs(dir, "#[sqlx::test");
                }
            }
        }
        Self {
            nextest_config,
            nextest_installed: on_path(&ctx.env, "cargo-nextest").is_some(),
            sqlx_tests,
            test_binaries,
            integration_files,
        }
    }
}

/// `(test binaries, integration test files)` from metadata.
#[must_use]
pub fn count_test_binaries(m: &Metadata) -> (usize, usize) {
    let (mut bins, mut files) = (0, 0);
    for p in m.members() {
        for t in &p.targets {
            if t.kind.iter().any(|k| k == "test") {
                bins += 1;
                files += 1;
            } else if t
                .kind
                .iter()
                .any(|k| matches!(k.as_str(), "lib" | "bin" | "rlib"))
            {
                bins += 1;
            }
        }
    }
    (bins, files)
}

fn count_in_rs(dir: &Path, needle: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let p = e.path();
            let name = e.file_name();
            if p.is_dir() {
                if name == "target" || name.to_string_lossy().starts_with('.') {
                    0
                } else {
                    count_in_rs(&p, needle)
                }
            } else if p.extension().is_some_and(|x| x == "rs") {
                std::fs::read_to_string(&p).map_or(0, |s| s.matches(needle).count())
            } else {
                0
            }
        })
        .sum()
}

/// Target runners from config and environment: `(where, runner)`.
#[must_use]
pub fn runners(ctx: &Context) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in &ctx.env {
        if k.starts_with("CARGO_TARGET_") && k.ends_with("_RUNNER") && !v.is_empty() {
            out.push((k.clone(), v.clone()));
        }
    }
    for (file, t) in &ctx.config.files {
        if let Some(toml::Value::Table(targets)) = t.get("target") {
            for (triple, v) in targets {
                if let Some(r) = v.get("runner") {
                    let r = match r {
                        toml::Value::Array(a) => a
                            .iter()
                            .filter_map(toml::Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" "),
                        other => other.as_str().unwrap_or_default().to_owned(),
                    };
                    out.push((format!("target.{triple}.runner in {}", file.display()), r));
                }
            }
        }
    }
    out
}

/// Run the check.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "a flat list of independent findings reads best in one place"
)]
pub fn check(ctx: &Context, i: &Inputs) -> Section {
    let mut s = Section::new("tests");
    let mut f = Vec::new();
    let uses_nextest = i.nextest_config.is_some() || i.nextest_installed;
    s.fact(
        "nextest",
        match (i.nextest_installed, i.nextest_config.is_some()) {
            (true, true) => "installed, .config/nextest.toml present",
            (true, false) => "installed, no .config/nextest.toml",
            (false, true) => ".config/nextest.toml present, but cargo-nextest is not on PATH",
            (false, false) => "not installed",
        },
    );
    s.fact(
        "test binaries",
        format!(
            "{} ({} integration test files)",
            i.test_binaries, i.integration_files
        ),
    );
    if !uses_nextest && i.test_binaries >= 3 {
        f.push(
            Finding::new("no-nextest", Severity::Low, "Tests run with cargo test", "nextest")
                .saw(format!("cargo-nextest isn't installed and there are {} test binaries.", i.test_binaries))
                .why("`cargo test` runs test binaries one after another, so one slow binary blocks the rest. nextest runs every test from every binary in one pool, and reports slow and leaky tests.")
                .fix("cargo install cargo-nextest --locked, then `cargo nextest run --workspace`. Doc tests still need `cargo test --doc`."),
        );
    }
    let runners = runners(ctx);
    for (src, r) in &runners {
        s.fact("target runner", format!("{r} ({src})"));
    }
    if let Some((src, r)) = runners.first() {
        f.push(
            Finding::new("target-runner-wraps-tests", if uses_nextest { Severity::High } else { Severity::Medium }, format!("Every test process runs under `{}`", r.split_whitespace().next().unwrap_or(r)), "test-runners-that-wrap-every-test")
                .saw(format!("A target runner is configured ({src}): `{r}`.{}", if uses_nextest { " nextest starts one process per test, so the runner starts once per test." } else { "" }))
                .why("A runner wraps every execution of a test binary. Under nextest that's every single test: its startup cost is paid thousands of times, and if it holds a slot or a lock, tests queue behind it. In one workspace, a cache's test runner made 0.00 s tests wait 3-8 s each and took about 44% of all test-seconds.")
                .fix("Remove the runner for test runs, or scope it to the targets that need it (e.g. a cross-compilation triple). If you need a wrapper for nextest, use a nextest setup script or wrapper script that runs once per run, not per test. Measure `cargo nextest run` with and without it, back to back."),
        );
    }
    let profile_threads = i
        .nextest_config
        .as_ref()
        .and_then(|c| c.get("profile"))
        .and_then(toml::Value::as_table)
        .map(|p| {
            p.iter()
                .filter_map(|(name, v)| v.get("test-threads").map(|t| format!("{name}={t}")))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(t) = ctx.var("NEXTEST_TEST_THREADS") {
        s.fact("NEXTEST_TEST_THREADS", t);
        if !profile_threads.is_empty() {
            f.push(
                Finding::new("nextest-threads-env", Severity::Medium, "NEXTEST_TEST_THREADS overrides your nextest profile", "nextest")
                    .saw(format!("NEXTEST_TEST_THREADS={t} is set, and .config/nextest.toml sets test-threads ({}).", profile_threads.join(", ")))
                    .why("The environment variable beats the profile, so the limit someone chose for a reason (database tests, memory) silently stops applying, or a tuned profile is throttled by a stale shell setting.")
                    .fix("Unset NEXTEST_TEST_THREADS and keep the limit in the profile, or pass `--test-threads` explicitly for one-off runs."),
            );
        }
    }
    let deps: Vec<&str> = ctx
        .metadata
        .as_ref()
        .map(|m| {
            m.members()
                .flat_map(|p| p.dependencies.iter().map(|d| d.name.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let db: Vec<&str> = [
        "sqlx",
        "testcontainers",
        "postgresql_embedded",
        "pg-embed",
        "tokio-postgres",
        "diesel",
        "sea-orm",
        "pgtemp",
    ]
    .into_iter()
    .filter(|d| deps.contains(d))
    .collect();
    if !db.is_empty() {
        s.fact("database crates", db.join(", "));
        if i.sqlx_tests > 0 {
            s.fact("#[sqlx::test] tests", i.sqlx_tests.to_string());
        }
        f.push(
            Finding::new("database-tests", if i.sqlx_tests >= 50 { Severity::Medium } else { Severity::Info }, "Database-backed tests: make each database cheap", "database-backed-tests")
                .saw(format!("The workspace uses {}{}.", db.join(", "), if i.sqlx_tests > 0 { format!(", with {} #[sqlx::test] tests", i.sqlx_tests) } else { String::new() }))
                .why("A test that creates a database and runs every migration pays tens to hundreds of milliseconds before it does anything, plus fsyncs. Multiply by every test.")
                .fix("Run the test Postgres on tmpfs with fsync=off, synchronous_commit=off and full_page_writes=off. Run migrations once into a template database and create each test database with `CREATE DATABASE t TEMPLATE app_template`. Limit nextest test-threads for these tests with a test group rather than for the whole suite."),
        );
    }
    if i.integration_files >= 15 {
        f.push(
            Finding::new("many-test-binaries", Severity::Low, format!("{} integration test files, one binary each", i.integration_files), "fewer-test-binaries")
                .saw(format!("{} files under tests/ across the workspace; each is linked as its own binary.", i.integration_files))
                .why("Every integration test file is a separate crate that links the whole dependency graph. Linking dozens of big binaries can take longer than running the tests.")
                .fix("Put integration tests in one binary per crate: tests/it/main.rs with `mod foo;` for each former file."),
        );
    }
    s.extend(f);
    s
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::context::CargoConfig;

    #[test]
    fn runner_under_nextest_and_env_threads() {
        let mut ctx = Context {
            config: CargoConfig::from_strs(&[(
                "/w/.cargo/config.toml",
                "[target.x86_64-unknown-linux-gnu]\nrunner = [\"kache\", \"test-runner\"]\n",
            )]),
            ..Context::default()
        };
        ctx.env.insert("NEXTEST_TEST_THREADS".into(), "4".into());
        let i = Inputs {
            nextest_config: Some("[profile.gate]\ntest-threads = 8\n".parse().unwrap()),
            nextest_installed: true,
            test_binaries: 40,
            integration_files: 20,
            ..Inputs::default()
        };
        let s = check(&ctx, &i);
        let ids: Vec<_> = s.findings.iter().map(|f| f.id).collect();
        assert_eq!(
            ids,
            [
                "target-runner-wraps-tests",
                "nextest-threads-env",
                "many-test-binaries"
            ]
        );
        assert!(s.findings[0].title.contains("`kache`"));
    }

    #[test]
    fn plain_cargo_test() {
        let s = check(
            &Context::default(),
            &Inputs {
                test_binaries: 5,
                ..Inputs::default()
            },
        );
        assert_eq!(s.findings[0].id, "no-nextest");
    }
}
