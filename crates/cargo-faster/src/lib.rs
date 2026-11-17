//! `cargo faster`: find out why a Rust build is slow and how to make it
//! faster.
//!
//! This library is the engine behind the `cargo-faster` binary. Its API is
//! not stable; it is public so the checks can be tested against fixture
//! data. Each check module turns gathered inputs (cargo configuration, the
//! environment, `cargo metadata`, timings, self-profiles, machine state)
//! into [`finding::Finding`]s.

extern crate alloc;

pub mod asynctrait;
pub mod cache;
pub mod checkcost;
pub mod context;
pub mod deps;
pub mod error;
pub mod finding;
pub mod fmt;
pub mod graph;
pub mod machine;
pub mod profile;
pub mod setup;
pub mod testrun;
pub mod tests;
pub mod timings;

/// The guide every finding links into, as of this release's tag, so a
/// link keeps pointing at the text that matches the finding.
pub const GUIDE_URL: &str = concat!(
    "https://github.com/talaagent/cargo-faster/blob/v",
    env!("CARGO_PKG_VERSION"),
    "/docs/guide.md"
);

/// The version of the `--json` output's shape. It goes up when a stable
/// field (see docs/cargo-faster.md) is removed, renamed or changes
/// meaning; new fields don't change it.
pub const SCHEMA_VERSION: u32 = 1;

/// Every guide anchor a finding may link to. A test checks that each one
/// exists in `docs/guide.md`.
pub const GUIDE_ANCHORS: &[&str] = &[
    "self-profiling-one-crate",
    "why-send-proofs-get-expensive",
    "erasing-a-heavy-type",
    "boxing-futures-at-a-boundary",
    "the-cost-of-async-trait",
    "the-build-timeline",
    "the-critical-path",
    "rebuild-fan-out",
    "splitting-a-big-crate",
    "dependencies-and-features",
    "dev-dependency-features-split-the-build",
    "incremental-compilation",
    "sccache-and-kache",
    "worktrees-and-cold-target-directories",
    "linking",
    "debug-info",
    "optimizing-dependencies-in-dev-builds",
    "codegen-units",
    "sharing-generics",
    "the-parallel-frontend",
    "nextest",
    "test-runners-that-wrap-every-test",
    "database-backed-tests",
    "fewer-test-binaries",
    "test-binaries-without-tests",
    "measuring-a-test-run",
    "the-machine",
    "procedural-macros",
    "generic-code-and-monomorphization",
    "case-study-a-312-second-check",
];
