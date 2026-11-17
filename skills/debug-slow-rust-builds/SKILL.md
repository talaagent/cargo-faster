---
name: debug-slow-rust-builds
description: Find out why a Rust build, check, clippy run or test suite is slow and fix it, using cargo-faster. Use when asked why "cargo build/check/clippy/test is slow", "compile times are bad", "this crate takes minutes to check", "tests take forever", "is the cache working", or to "speed up the build" of a Rust workspace.
---

# Debug slow Rust builds

`cargo faster` does the measuring; you read the results, pick the fix, and prove it worked. Every
finding it prints has `saw` (numbers), `why`, `fix` (with tradeoffs), and a link into
`docs/guide.md` in the cargo-faster repo. Read the linked section before you change anything
non-trivial.

If `cargo faster` isn't installed: `cargo install --locked cargo-faster`, or
`nix run github:talaagent/cargo-faster -- faster ...`.

## Workflow

1. **Get the overall picture.** It builds nothing and takes seconds.

   ```sh
   cargo faster            # or: cargo faster --json
   ```

   Fix cheap `high` findings first, especially from `tests` (a target runner wrapping every
   test, `NEXTEST_TEST_THREADS` overriding the profile) and `setup` (full debug info, an old
   linker). These are often the whole problem, and none of them involve the compiler.

2. **Check the machine before you trust any number.**

   ```sh
   cargo faster machine
   ```

   Note the load and the number of other `rustc` processes. If load is above the CPU count,
   every timing is inflated, and only back-to-back comparisons mean anything.

3. **Find the slow units.** Reproduce the slow case first: the same command and the same kind of
   edit the user complained about (an edit to which crate? a cold worktree or a warm one?). Then:

   ```sh
   cargo faster timings --run          # runs `cargo check --workspace --timings` under nice
   cargo faster timings path/to/cargo-timing.html   # or read an existing report
   cargo faster tests --run            # slow tests? times the test build and the run separately
   ```

   Look at the critical path. `check` units are the frontend (type checking, trait solving):
   go to step 4. Units that spend most of their time in codegen point to generics and
   monomorphization (`cargo llvm-lines`). Build scripts and proc macros show up too.

   For a slow `cargo build` (not a check), run `cargo check --workspace --timings` first and
   then the build with `--timings`: `cargo faster timings` compares the build with that check
   (or `--baseline FILE`), and with kache it shows each unit's cache outcome and waits. The
   facts and findings then say whether the time went to rebuilt workspace crates
   (`edit-rebuilt-many`), links (`slow-links`), waits (`wrapper-wait`,
   `critical-path-waiting`) or a busy machine (`contention`).

4. **Profile the slow crate.**

   ```sh
   cargo faster check-cost -p <crate> --keep /tmp/<crate>-prof
   cargo faster check-cost --profile /tmp/<crate>-prof/<file>.mm_profdata   # re-analyze later
   ```

   It runs on stable via `RUSTC_BOOTSTRAP=1`, with incremental off and no wrapper; tell the
   user that's how the numbers were measured. On a big crate under load it can take minutes:
   run it once, keep the profile, re-analyze it as often as you like.
   When the slow part is `cargo test` rebuilding after an edit, add `--tests`: that profiles the
   library's `--test` build (the header says `(lib, --test)`), which can be twice the plain lib.

5. **Fix the top item, then measure again** the same way, back to back, under similar load.
   Report before and after numbers for the same unit, not just the wall time.

## Reading check-cost

- **trait solving %**: if `evaluate_obligation` is most of the time, the problem is types, not
  code size. Splitting the crate or removing derives won't help much.
- **trait solving by what is being proved**: time grouped by the crate that defines each type.
  A dependency's types (an SDK client, an HTTP stack) taking seconds in a crate that only
  *holds* them means a heavy type is leaking into async code.
- **most expensive obligations**: look at `count` and `bodies`. `Foo: Send` proved 54 times in
  47 bodies means each async body re-walks `Foo`'s whole type graph. The fix is to stop the
  walk, not to make one walk faster.
- **bodies doing the most trait solving**: where to start editing. One body at 20% is common:
  a long async fn awaiting deep call chains.
- **proc macro expansion**: usually small. If it's large, see the guide's proc macro section.

## Fix playbook

| Finding | First fix | Cost |
|---|---|---|
| `send-proofs-per-async-body` | Hold the heavy type as `Arc<dyn Trait + Send + Sync>`, or put its heavy fields behind one | A vtable call |
| `trait-solving-dominates`, `heavy-function-body` | Return `Pin<Box<dyn Future<Output = T> + Send + '_>>` from functions at module boundaries; split the top body | A box per call |
| `async-trait-overhead`, `uses-async-trait` | Replace `#[async_trait]` with native `async fn` in traits (`-> impl Future<Output = T> + Send` where callers spawn, `dynosaur` for `dyn`-held traits), and ban it with clippy `disallowed-macros`. Its per-method lifetime bounds split rustc's trait-solving cache; on one workspace this cut trait solving 80%. Erase heavy types too: that's the other lever | Signature churn |
| `serial-crate`, `big-crate` | Profile it first (`check-cost`). Split only if it's actually just big | Refactor |
| `target-runner-wraps-tests`, `target-runner-in-scripts` | Remove or narrow `target.<triple>.runner`, or have it `exec "$@"` when `NEXTEST_EXECUTION_MODE=process-per-test`; use a once-per-run nextest setup script | |
| `nextest-threads-env` | Unset `NEXTEST_TEST_THREADS`, or pass `--test-threads` in the scripts that run each profile; put limits in `.config/nextest.toml` | |
| `test-build-dominates`, `tests-near-slow-timeout`, `test-threads-idle` (`tests --run`) | Build-bound: work on the test build's timings (fewer test binaries, debuginfo, linker). Run-bound: split or speed up the slowest tests, check test groups | |
| `database-tests` | Add what the finding lists as missing: Postgres on tmpfs, fsync off, a template database cloned per test | |
| `full-debuginfo` | `debug = "line-tables-only"` in `[profile.dev]` | Fewer variables in a debugger |
| `slow-linker` | `rust-lld` (default on x86_64 Linux since 1.90), `mold` or `wild` | |
| `duplicate-heavy-crates`, `tokio-full` | `cargo tree -d` / `cargo tree -e features -i tokio`, then upgrade or trim features | |
| `cold-worktrees`, `kache-*`, `cache-*` | Expect cold first builds in new worktrees; keep target and cache on one reflink-capable filesystem | |

## Pitfalls

- **Never `cargo -p` with kache or a similar cache.** Its keys include the set of packages being
  built, so `cargo check -p foo` misses the cache and rebuilds dependencies from scratch. Use
  `--workspace` and let the cache make the rest free. `cargo faster check-cost -p foo` is fine:
  `-p` there only picks the crate to profile, and cargo itself still runs with `--workspace`.
- **Measure under similar load.** On a shared machine, a number taken at load 20 and one taken at
  load 70 can't be compared. Run before and after back to back, and run heavy experiments under
  `nice -n 10` (the `--nice` default).
- **Compare like with like.** The same command (`check` vs `clippy` vs `test`), the same edit, and
  the same warmth: a cold worktree against a warm one measures the cache, not your fix.
  Incremental compilation only helps repeated builds in the same worktree. Seeding incremental
  state from another worktree made things slower, not faster, and clippy never seeds.
- **Don't profile in the user's checkout if they're working in it.** Use a throwaway worktree
  (`git worktree add --detach /tmp/prof HEAD`) and remove it afterwards.
- **`RUSTFLAGS` changes invalidate every cache.** Don't experiment with global rustflags in a
  shared target directory; it causes a full rebuild for everyone using it.
- **Don't trust the obvious suspects.** In the guide's case study, the crate looked too big and
  the macros looked guilty, but 92 to 97% of the time was one struct proved `Send` again in every
  async body. Profile before you refactor.
