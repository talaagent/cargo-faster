//! Tests: nextest, target runners, thread limits, database-backed tests,
//! and the number of test binaries.

use crate::context::{Context, Metadata, on_path};
use crate::finding::{Finding, Section, Severity};
use alloc::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    /// The text of each target runner that is a readable script, keyed by
    /// the runner as configured.
    pub runner_scripts: BTreeMap<String, String>,
    /// Lines in the repository's scripts that mention a test setting (a
    /// `CARGO_TARGET_<TRIPLE>_RUNNER` override, ...).
    pub script_hits: Vec<ScriptHit>,
}

/// One line of a repository script (`scripts/`, a Makefile, a justfile, a
/// CI workflow) that mentions a setting the checks care about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptHit {
    /// The file, relative to the workspace root.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// The line, trimmed.
    pub text: String,
    /// For a line that sets `CARGO_TARGET_<TRIPLE>_RUNNER` (directly or
    /// through a variable holding that name), the runner it sets, with
    /// `$PWD/` and quotes stripped; `None` when it can't be read.
    pub runner: Option<String>,
}

impl ScriptHit {
    /// `file:line`.
    #[must_use]
    pub fn location(&self) -> String {
        format!("{}:{}", self.file, self.line)
    }

    /// Whether the line sets a target runner through the environment.
    #[must_use]
    pub fn sets_runner(&self) -> bool {
        mentions_runner_var(&self.text) || self.runner.is_some()
    }
}

fn mentions_runner_var(l: &str) -> bool {
    l.find("CARGO_TARGET_")
        .is_some_and(|i| l[i..].contains("_RUNNER"))
}

/// The value assigned after `key=` in a shell line, unquoted, with `$PWD/`
/// stripped; `None` if it's empty or another variable.
fn assigned_value(line: &str, key: &str) -> Option<String> {
    let rest = &line[line.find(key)? + key.len()..];
    let rest = rest.strip_prefix('=')?;
    let v = rest
        .trim_start_matches(['"', '\''])
        .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .next()?;
    let v = ["$PWD/", "${PWD}/", "$(pwd)/", "./"]
        .iter()
        .find_map(|p| v.strip_prefix(p))
        .unwrap_or(v);
    (!v.is_empty() && !v.starts_with('$')).then(|| v.to_owned())
}

/// The lines of a script that set a target runner through the environment:
/// `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=...`, or a variable that
/// holds that name (`runner_var=CARGO_TARGET_..._RUNNER` and later
/// `$runner_var=...`, as in `env "$runner_var=..." cargo nextest run`).
#[must_use]
pub fn script_hits(file: &str, text: &str) -> Vec<ScriptHit> {
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .map(|(n, l)| (n + 1, l.trim()))
        .filter(|(_, l)| !l.starts_with('#') && !l.is_empty())
        .collect();
    // Variables whose value is a runner variable's name.
    let mut vars = Vec::new();
    for (_, l) in &lines {
        if mentions_runner_var(l)
            && let Some((lhs, _)) = l.split_once('=')
        {
            let name = lhs.trim().trim_start_matches("export ").trim();
            if !name.is_empty()
                && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !name.starts_with("CARGO_TARGET_")
            {
                vars.push(name.to_owned());
            }
        }
    }
    let mut out = Vec::new();
    for (n, l) in &lines {
        let hit = |runner| ScriptHit {
            file: file.to_owned(),
            line: *n,
            text: (*l).to_owned(),
            runner,
        };
        let used = vars.iter().find_map(|v| {
            [format!("${{{v}}}"), format!("${v}")]
                .into_iter()
                .find(|k| l.contains(&format!("{k}=")))
        });
        if let Some(k) = used {
            out.push(hit(assigned_value(l, &k)));
        } else if mentions_runner_var(l) && !vars.iter().any(|v| l.contains(&format!("{v}="))) {
            let key = l.find("CARGO_TARGET_").and_then(|i| {
                l[i..]
                    .find("_RUNNER")
                    .map(|j| &l[i..i + j + "_RUNNER".len()])
            });
            out.push(hit(key.and_then(|k| assigned_value(l, k))));
        }
    }
    out
}

/// Whether a runner script steps aside under nextest: it checks
/// `NEXTEST_EXECUTION_MODE` (or another `NEXTEST_*` variable nextest sets for
/// each test) and execs the test directly.
#[must_use]
pub fn bypasses_nextest(script: &str) -> bool {
    script.lines().any(|l| {
        let l = l.trim();
        !l.starts_with('#')
            && [
                "NEXTEST_EXECUTION_MODE",
                "NEXTEST_RUN_ID",
                "NEXTEST_TEST_NAME",
            ]
            .iter()
            .any(|v| l.contains(v))
    })
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
        let mut runner_scripts = BTreeMap::new();
        for (src, r) in runners(ctx) {
            if let Some(path) = runner_program(ctx, &src, &r)
                && let Some(text) = read_small(&path)
            {
                runner_scripts.insert(r, text);
            }
        }
        let script_hits = gather_script_hits(root);
        for h in &script_hits {
            if let Some(v) = &h.runner
                && let Some(text) = read_small(&root.join(v))
            {
                runner_scripts.insert(v.clone(), text);
            }
        }
        Self {
            nextest_config,
            nextest_installed: on_path(&ctx.env, "cargo-nextest").is_some(),
            sqlx_tests,
            test_binaries,
            integration_files,
            runner_scripts,
            script_hits,
        }
    }
}

/// A text file under 256 KiB.
fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 256 * 1024 {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// The runner's program on disk. Cargo resolves a config runner with a `/`
/// relative to the directory holding `.cargo/`, an environment one relative
/// to the current directory, and a bare name on `PATH`.
fn runner_program(ctx: &Context, src: &str, runner: &str) -> Option<PathBuf> {
    let prog = runner.split_whitespace().next()?;
    if !prog.contains('/') {
        return crate::context::on_path(&ctx.env, prog);
    }
    let base = src
        .split_once(" in ")
        .and_then(|(_, f)| Path::new(f).parent()?.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| ctx.cwd.clone());
    Some(base.join(prog))
}

/// Scan the scripts that usually drive test runs: `scripts/` (two levels),
/// `Makefile`, `justfile` and `.github/workflows`.
fn gather_script_hits(root: &Path) -> Vec<ScriptHit> {
    let mut files = Vec::new();
    for name in [
        "Makefile",
        "makefile",
        "GNUmakefile",
        "justfile",
        "Justfile",
        ".justfile",
    ] {
        files.push(root.join(name));
    }
    for (dir, depth) in [("scripts", 2), (".github/workflows", 1)] {
        list_files(&root.join(dir), depth, &mut files);
    }
    let mut out = Vec::new();
    for f in files {
        if let Some(text) = read_small(&f) {
            let rel = f.strip_prefix(root).unwrap_or(&f).display().to_string();
            out.extend(script_hits(&rel, &text));
        }
    }
    out
}

fn list_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            if depth > 1
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(['.', '_']))
            {
                list_files(&p, depth - 1, out);
            }
        } else {
            out.push(p);
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
    let overrides: Vec<&ScriptHit> = i.script_hits.iter().filter(|h| h.sets_runner()).collect();
    for h in &overrides {
        s.fact("runner override", format!("{} ({})", h.location(), h.text));
    }
    // Runners the scripts set that don't step aside under nextest (or
    // that we couldn't read).
    let busy_overrides: Vec<String> = overrides
        .iter()
        .filter(|h| {
            !h.runner
                .as_ref()
                .and_then(|v| i.runner_scripts.get(v))
                .is_some_and(|t| bypasses_nextest(t))
        })
        .map(|h| {
            h.runner
                .as_ref()
                .map_or_else(|| h.location(), |v| format!("`{v}` ({})", h.location()))
        })
        .collect();
    let override_note = if overrides.is_empty() {
        String::new()
    } else {
        format!(
            " Scripts also set a runner through the environment, which beats the config for those runs: {}.{}",
            overrides
                .iter()
                .map(|h| h.location())
                .collect::<Vec<_>>()
                .join(", "),
            if busy_overrides.is_empty() {
                " Those runners step aside under nextest too.".to_owned()
            } else {
                format!(
                    " These don't check NEXTEST_EXECUTION_MODE, so they run once per test in those runs: {}.",
                    busy_overrides.join(", ")
                )
            }
        )
    };
    if let Some((src, r)) = runners.first() {
        let prog = r.split_whitespace().next().unwrap_or(r);
        let bypass = i.runner_scripts.get(r).is_some_and(|t| bypasses_nextest(t));
        let finding = if bypass && uses_nextest {
            Finding::new("target-runner-wraps-tests", if busy_overrides.is_empty() { Severity::Info } else { Severity::Low }, format!("`{prog}` steps aside under nextest"), "test-runners-that-wrap-every-test")
                .saw(format!("A target runner is configured ({src}): `{r}`. It checks NEXTEST_EXECUTION_MODE and runs the test directly under nextest, so each test pays one extra exec (about a millisecond), not the runner's work.{override_note}"))
                .why("A runner wraps every execution of a test binary, and nextest starts one process per test. A runner that execs the test straight away under nextest costs next to nothing; one that does real work (takes a slot, a lock, a fifo) is paid once per test.")
                .fix(format!("Nothing for this runner.{} To see what any runner costs, run one quick test a few times through `cargo nextest run` with and without it, back to back.", if busy_overrides.is_empty() { "" } else { " Measure the runners the scripts set, or have them step aside under nextest too." }))
        } else {
            Finding::new("target-runner-wraps-tests", if uses_nextest { Severity::High } else { Severity::Medium }, format!("Every test process runs under `{prog}`"), "test-runners-that-wrap-every-test")
                .saw(format!("A target runner is configured ({src}): `{r}`.{}{override_note}", if uses_nextest { " nextest starts one process per test, so the runner starts once per test." } else { "" }))
                .why("A runner wraps every execution of a test binary. Under nextest that's every single test: its startup cost is paid thousands of times, and if it holds a slot or a lock, tests queue behind it. In one workspace, a cache's test runner made 0.00 s tests wait 3-8 s each and took about 44% of all test-seconds.")
                .fix("Remove the runner for test runs, or scope it to the targets that need it (e.g. a cross-compilation triple). If the runner is a script, have it step aside under nextest: `[ \"$NEXTEST_EXECUTION_MODE\" = process-per-test ] && exec \"$@\"`. If you need a wrapper for nextest, use a nextest setup script or wrapper script that runs once per run, not per test. Measure `cargo nextest run` with and without it, back to back.")
        };
        f.push(finding);
    } else if let Some(h) = overrides.first()
        && !busy_overrides.is_empty()
    {
        f.push(
            Finding::new("target-runner-in-scripts", Severity::Low, format!("{} sets a target runner for its test runs", h.file), "test-runners-that-wrap-every-test")
                .saw(format!("No runner is configured, but scripts set one through the environment, and it doesn't check NEXTEST_EXECUTION_MODE: {}.", busy_overrides.join(", ")))
                .why("A runner wraps every execution of a test binary, and nextest starts one process per test, so whatever the runner does is paid once per test in those runs.")
                .fix("Make sure the runner execs the test straight away under nextest (`NEXTEST_EXECUTION_MODE=process-per-test`), or measure one quick test through `cargo nextest run` with and without it."),
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

    const KACHE_RUNNER: &str = "#!/bin/sh\nif [ \"${NEXTEST_EXECUTION_MODE:-}\" = process-per-test ]; then\n  exec \"$@\"\nfi\nexec kache test-runner \"$@\"\n";

    fn runner_ctx() -> Context {
        Context {
            config: CargoConfig::from_strs(&[(
                "/w/.cargo/config.toml",
                "[target.x86_64-unknown-linux-gnu]\nrunner = \"scripts/kache-test-runner\"\n",
            )]),
            ..Context::default()
        }
    }

    fn nextest_inputs() -> Inputs {
        Inputs {
            nextest_installed: true,
            ..Inputs::default()
        }
    }

    #[test]
    fn runner_that_steps_aside_under_nextest_is_info() {
        let mut i = nextest_inputs();
        i.runner_scripts
            .insert("scripts/kache-test-runner".into(), KACHE_RUNNER.into());
        let s = check(&runner_ctx(), &i);
        assert_eq!(s.findings[0].id, "target-runner-wraps-tests");
        assert_eq!(s.findings[0].severity, Severity::Info);
        assert!(s.findings[0].title.contains("steps aside"));
    }

    #[test]
    fn runner_without_bypass_stays_high() {
        let mut i = nextest_inputs();
        i.runner_scripts.insert(
            "scripts/kache-test-runner".into(),
            "#!/bin/sh\n# NEXTEST_EXECUTION_MODE is ignored here\nexec kache test-runner \"$@\"\n"
                .into(),
        );
        let s = check(&runner_ctx(), &i);
        assert_eq!(s.findings[0].severity, Severity::High);
    }

    #[test]
    fn script_runner_override() {
        let gate = "host=$(rustc -vV | sed -n 's/host: //p')\nrunner_var=CARGO_TARGET_$(tr 'a-z-' 'A-Z_' <<<\"$host\")_RUNNER\n# CARGO_TARGET_X_RUNNER=no in a comment\n  \"cargo nextest|$runner_var=$PWD/scripts/test-runner $cargo nextest run --test-threads 8\"\n";
        let hits = script_hits("scripts/gate", gate);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].line, 4);
        assert_eq!(hits[0].runner.as_deref(), Some("scripts/test-runner"));
        let direct = script_hits(
            "Makefile",
            "test:\n\tCARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=\"./ci/run\" cargo nextest run\n",
        );
        assert_eq!(direct[0].runner.as_deref(), Some("ci/run"));
        let set = hits[0].clone();

        // The config runner steps aside, the gate's doesn't: low, and named.
        let mut i = nextest_inputs();
        i.runner_scripts
            .insert("scripts/kache-test-runner".into(), KACHE_RUNNER.into());
        i.runner_scripts.insert(
            "scripts/test-runner".into(),
            "#!/bin/bash\nmkfifo \"$f\"\nsetsid \"$@\" &\n".into(),
        );
        i.script_hits = vec![set.clone()];
        let s = check(&runner_ctx(), &i);
        assert_eq!(s.findings[0].severity, Severity::Low);
        assert!(
            s.findings[0]
                .saw
                .contains("`scripts/test-runner` (scripts/gate:4)")
        );
        assert!(s.facts.iter().any(|f| f.key == "runner override"));

        // No config runner: the script's runner alone is reported.
        let s = check(&Context::default(), &i);
        assert_eq!(s.findings[0].id, "target-runner-in-scripts");
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
