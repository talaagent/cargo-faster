//! `timings --cmd/--edit` under `--dry-run`: the commands it would run, and
//! the crate's file left alone. Builds nothing, so it always runs.

use std::process::Command;

#[test]
fn timings_edit_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"tiny_core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    let lib = "pub fn x() -> u32 { 1 }\n";
    std::fs::write(root.join("src/lib.rs"), lib).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-faster"))
        .args([
            "faster",
            "--dry-run",
            "timings",
            "--cmd",
            "clippy",
            "--edit",
            "tiny-core",
            "--nice",
            "0",
            "--",
            "--profile",
            "gate",
        ])
        .current_dir(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The warm-up build, then the timed one, both --workspace, no -p.
    assert!(
        all.contains("cargo clippy --workspace --profile gate"),
        "{all}"
    );
    assert!(
        all.contains("cargo clippy --workspace --timings --profile gate"),
        "{all}"
    );
    assert!(all.contains("would append a probe"), "{all}");
    assert!(!all.contains(" -p "), "{all}");
    assert_eq!(
        std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        lib
    );
    assert!(!root.join("target").exists(), "a dry run built something");
}
