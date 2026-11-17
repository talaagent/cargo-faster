//! Real profiling runs. They compile code and take a while, so they only run
//! with `CARGO_FASTER_PROFILE_TESTS=1`.

use std::process::Command;

fn enabled() -> bool {
    std::env::var_os("CARGO_FASTER_PROFILE_TESTS").is_some_and(|v| v == "1")
}

#[test]
fn check_cost_profiles_a_tiny_crate() {
    if !enabled() {
        eprintln!("skipped: set CARGO_FASTER_PROFILE_TESTS=1 to run");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub async fn work(v: Vec<u32>) -> u32 { async {}.await; v.iter().sum() }\n\
         pub fn spawnable() -> impl std::future::Future<Output = u32> + Send { work(vec![1, 2]) }\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-faster"))
        .args(["faster", "--json", "check-cost", "--nice", "0"])
        .current_dir(root)
        .env_remove("RUSTC_WRAPPER")
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let text = v.to_string();
    assert!(text.contains("RUSTC_BOOTSTRAP=1"), "{text}");
    assert!(text.contains("tiny"), "{text}");
}
