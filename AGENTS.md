# cargo-faster: contributor and agent guide

`cargo faster` finds out why a Rust build is slow and how to make it
faster. It reads cargo configuration, the environment, `cargo metadata`,
`cargo build --timings` output, rustc self-profiles, cache tool stats and
machine state, and turns them into findings: what it saw, why it matters,
how to fix it, and a link into `docs/guide.md`. CONTRIBUTING.md covers the
pull request flow and releases.

## Commands

Use the nix dev shell. It pins rustc and provides cargo-nextest, cargo-deny
and measureme's `summarize`:

```sh
nix develop -c cargo fmt --all --check
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo nextest run --workspace
CARGO_FASTER_PROFILE_TESTS=1 nix develop -c cargo nextest run --workspace
RUSTDOCFLAGS="-D warnings" nix develop -c cargo doc --workspace --no-deps
nix develop -c ./scripts/no-std-check.sh
nix develop -c cargo deny check
nix develop -c cargo publish --workspace --dry-run --allow-dirty
nix build .#cargo-faster && ./result/bin/cargo-faster --version
```

`CARGO_FASTER_PROFILE_TESTS=1` enables the integration tests that build a
tiny crate and self-profile it (they need `RUSTC_BOOTSTRAP=1`, which the
tool sets itself). Everything else runs on fixtures and is fast.

## Layout

The parser crates under `crates/` are reusable on their own: public serde
types, no cargo-faster concepts (findings, severity), `#![no_std]` + `alloc`
with file IO behind a default `std` feature (`scripts/no-std-check.sh`
builds them for `wasm32v1-none`). Each keeps real fixtures from runs on this
repository under `tests/fixtures/cargo-faster/`.

- `crates/nextest-libtest-json`: nextest's experimental libtest-json(-plus)
  output as typed events and a run summary.
- `crates/rustc-self-profile`: rustc `-Zself-profile` analysis: queries,
  per-body/item/module typeck and borrowck time, repeated trait
  obligations, proc macros, profiler overhead (`load` via analyzeme is
  std-only).
- `crates/cargo-timings-parse`: `cargo build --timings` HTML as typed
  units, and their analysis: critical path, serial crates, codegen,
  linking, slowdown against an earlier report, rebuilt workspace crates
  (finding reports in a target directory is std-only).

`crates/cargo-faster` is a library (`src/lib.rs`, unstable API, for tests)
and the `cargo-faster` binary (`src/main.rs`, the bpaf CLI).

- `finding.rs`: `Finding`, `Severity`, `Report`; text and JSON rendering.
- `setup.rs`: cargo config and profile audit (linker, debuginfo,
  codegen-units, dependency opt-level, rustflags, jobs).
- `cache.rs`: RUSTC_WRAPPER (sccache, kache), incremental, target dirs and
  worktrees, `kache stats --json`.
- `machine.rs`: load, memory, competing cargo/rustc processes, disk space,
  filesystems.
- `timings.rs`: findings over a `cargo-timings-parse` analysis, joined
  with the cache wrapper's records (`timings/wrapper.rs`, kache) and the
  workspace; `timings/run.rs` drives `timings --run`.
- `graph.rs`: workspace reverse dependencies and big crates, from
  `cargo metadata`.
- `deps.rs`: heavy features, duplicate versions, proc macros.
- `testrun.rs`: `tests --run` findings over a `nextest-libtest-json` run.
- `tests.rs`: nextest config, target runners, `NEXTEST_TEST_THREADS`,
  database-backed tests.
- `profile.rs`: self-profile findings over a `rustc-self-profile`
  analysis; `checkcost.rs` runs the profile.
- `context.rs`: the project context (metadata, config, env) the checks
  read, and the `probe`/`build` helpers every subprocess goes through
  (`bpaf_kit::cmd::Cmd`, so builds echo and honor `--dry-run`).
- `error.rs`: `Error` and its stable `code()`s.

## Conventions

- Checks are pure functions from gathered inputs to `Vec<Finding>`, so
  they are tested with fixture data. Gathering (spawning cargo, reading
  `/proc`) stays at the edges.
- Every finding has a stable kebab-case `id`, says what it saw with the
  real numbers, why it matters, how to fix it and its tradeoff, and links a
  guide anchor that exists (a test checks the anchors).
- The tool runs on stable. Measurements that need `-Z` flags set
  `RUSTC_BOOTSTRAP=1` on that one rustc invocation only, and say so in
  their output.
- Never pass `-p` to cargo when measuring a workspace: with caching
  wrappers like kache, a `-p` build resolves features differently and
  splits the cache keys. Never set `RUSTC_WORKSPACE_WRAPPER`; it changes
  `-C metadata` and rebuilds the workspace.
- No `unwrap` outside tests; `core`/`alloc` paths over `std` where they
  exist (clippy enforces both).
- Changelog entries come from commit messages (release-plz), so write
  commit subjects that read well in CHANGELOG.md.
