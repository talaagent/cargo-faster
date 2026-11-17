//! The command line itself: usage errors, help, exit statuses. Builds
//! nothing, so it always runs.

use std::process::{Command, Output};

#[allow(clippy::unwrap_used, reason = "a test helper")]
fn run(args: &[&str]) -> (Output, String) {
    run_with("", args)
}

#[allow(clippy::unwrap_used, reason = "a test helper")]
fn run_with(lib: &str, args: &[&str]) -> (Output, String) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), lib).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-faster"))
        .arg("faster")
        .args(args)
        .current_dir(dir.path())
        .env("CARGO_TARGET_DIR", dir.path().join("target"))
        .env_remove("CARGO_PROFILE_DEV_DEBUG")
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out, all)
}

#[test]
fn usage_errors_exit_2() {
    let (out, all) = run(&["--bogus"]);
    assert_eq!(out.status.code(), Some(2), "{all}");
    assert!(all.starts_with("cargo faster: error[usage]: "), "{all}");
    let (out, all) = run(&["timings", "--body"]);
    assert_eq!(out.status.code(), Some(2), "{all}");
    assert!(all.contains("needs --edit"), "{all}");
    let (out, all) = run(&["--fail-on", "urgent", "setup"]);
    assert_eq!(out.status.code(), Some(2), "{all}");
    assert!(all.contains("high, medium, low"), "{all}");
    let (out, all) = run(&["rdeps", "-p", "a", "b"]);
    assert_eq!(out.status.code(), Some(2), "{all}");
}

#[test]
fn help_says_cargo_faster_and_lists_global_options() {
    let (out, all) = run(&["--help"]);
    assert_eq!(out.status.code(), Some(0), "{all}");
    assert!(all.contains("Usage: cargo faster ["), "{all}");
    assert!(all.contains("Exit status"), "{all}");
    for cmd in [
        "report",
        "setup",
        "cache",
        "machine",
        "deps",
        "tests",
        "rdeps",
        "timings",
        "check-cost",
    ] {
        let (out, all) = run(&[cmd, "--help"]);
        assert_eq!(out.status.code(), Some(0), "{cmd}: {all}");
        assert!(all.contains(&format!("Usage: cargo faster {cmd}")), "{all}");
        assert!(all.contains("Global options"), "{cmd}: {all}");
    }
    let (_, all) = run(&["check-cost", "--help"]);
    assert!(all.contains("--profdata=FILE"), "{all}");
    let (out, all) = run(&["--version"]);
    assert_eq!(out.status.code(), Some(0), "{all}");
    assert!(all.starts_with("cargo-faster "), "{all}");
}

#[test]
fn unknown_crate_and_rdeps_package() {
    let (out, all) = run(&["rdeps", "-p", "tinny"]);
    assert_eq!(out.status.code(), Some(1), "{all}");
    assert!(all.contains("error[unknown_value]"), "{all}");
    assert!(all.contains("tiny"), "{all}");
    let (out, all) = run(&["rdeps", "-p", "tiny"]);
    assert_eq!(out.status.code(), Some(0), "{all}");
}

#[test]
fn fail_on() {
    // A fresh crate's dev profile has full debug info: a medium finding.
    let (out, all) = run(&["setup", "--fail-on", "medium"]);
    assert!(
        all.contains("[medium] Dev builds emit full debug info"),
        "{all}"
    );
    assert_eq!(out.status.code(), Some(1), "{all}");
    assert!(all.contains("(--fail-on medium)"), "{all}");
    let (out, all) = run(&["setup", "--fail-on", "high"]);
    assert!(!all.contains("[high]"), "{all}");
    assert_eq!(out.status.code(), Some(0), "{all}");
    let (out, all) = run(&["setup"]);
    assert_eq!(out.status.code(), Some(0), "{all}");
}

#[test]
fn a_failed_build_exits_3() {
    // `cargo check` of a one-file crate that doesn't compile: quick.
    let lib = "pub fn x() -> u32 { \"no\" }\n";
    let (out, all) = run_with(lib, &["timings", "--run", "--nice", "0"]);
    assert_eq!(out.status.code(), Some(3), "{all}");
    assert!(all.contains("[high] The build failed (exit 101)"), "{all}");
    assert!(all.contains("build failed (exit 101)"), "{all}");
    assert!(all.contains("error[build_failed]"), "{all}");
    let (out, all) = run_with(
        lib,
        &["timings", "--run", "--nice", "0", "--allow-failed-build"],
    );
    assert_eq!(out.status.code(), Some(0), "{all}");
}
