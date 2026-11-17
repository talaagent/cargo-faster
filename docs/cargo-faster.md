# Using cargo faster

`cargo faster` measures your build and tells you what it found. Each finding has four parts:

- **saw**: what it measured, with numbers;
- **why**: why that costs you time;
- **fix**: what to change, and what the change costs;
- **more**: a link to the section of [the guide](guide.md) that explains it.

It doesn't change your code or your configuration. Most subcommands only read things: your manifests, `cargo metadata`, Cargo's config, your cache's statistics, and `/proc`. Three of them build: `timings --run`, `tests --run` and `check-cost`. They run under `nice -n 10` by default, because a build machine is often shared.

This page goes through the subcommands one at a time. The example output comes from a real workspace of 183 crates on a busy 32-core machine; it's the workspace from [the case study](guide.md#case-study-a-312-second-check).

## Installing

```console
$ cargo install --locked cargo-faster
```

Or, with Nix:

```console
$ nix run github:talaagent/cargo-faster -- faster
```

`cargo faster` works on the stable toolchain. Shell completions and a man page come with it:

```console
$ cargo-faster --bpaf-complete-style-bash > ~/.local/share/bash-completion/completions/cargo-faster
$ cargo-faster --bpaf-complete-style-zsh  > ~/.zfunc/_cargo-faster
$ cargo-faster --bpaf-complete-style-fish > ~/.config/fish/completions/cargo-faster.fish
$ cargo-faster --man > ~/.local/share/man/man1/cargo-faster.1
```

## Global options

Every subcommand takes these, before or after the subcommand's name (`cargo faster --json setup` and `cargo faster setup --json` are the same):

- `--json` prints the report as JSON instead of text, for scripts and agents. Findings keep their `id`, `severity`, `title`, `saw`, `why`, `fix`, and `guide` fields. Each section also has a `data` object with what it measured, so a script can compare runs without parsing the text tables. For `timings`, that's `wall_s`, `units_compiled`, `units_fresh`, `critical_path` (each step with `unit`, `crate`, `version`, `target`, `mode`, `kind`, `start_s`, `took_s`, `waited` (`full` or `meta`), `gap_s`, and `cache` and `wait_s` when kache's records were joined), `slowest_units`, `kinds`, `linked`, `wrapper`, `slowdown` and `rebuilt`. Seconds are in fields ending in `_s`. For `check-cost`, it's `queries`, `obligations`, `trait_solving_by_category`, `heavy_bodies`, `items` and `modules`.
- `--manifest-path PATH` points at a workspace other than the current directory's.
- `--fail-on LEVEL` exits 1 when any finding is at least `LEVEL` severe: `high`, `medium` or `low`. The report is printed first. Use it in CI to fail a job on a new `high` finding.
- `--dry-run` prints the builds that `timings --run`, `tests --run` and `check-cost` would start, and stops before starting them. The quick checks still run, because they only read things.
- `-v` and `-q` change how much it logs to stderr.

The builds it starts are echoed to stderr before they run. Errors go to stderr as `cargo faster: error[CODE]: message`. The codes are stable: `usage`, `no_workspace`, `no_timings`, `unknown_value`, `failed`, and `cmd_spawn`, `cmd_exit`, `cmd_timeout`, `cmd_io`, `cmd_utf8` when a command it runs goes wrong. A misspelled crate name for `rdeps`, `timings --edit`, or a wrong `-p` or `--bin` for `check-cost`, lists the three closest names.

The exit status is:

- `0`: the report was printed, findings or not;
- `1`: an error, or a finding at or above `--fail-on`;
- `2`: a usage error: an unknown option, a bad value, or options that don't go together (`timings --body` without `--edit`).

Findings have one of four severities: `high`, `medium`, `low`, and `info`. A `high` finding probably costs you a lot of time right now. An `info` finding is something to know about, not necessarily something to change.

## `cargo faster` (report)

With no subcommand, `cargo faster` runs every check that's quick and doesn't build anything: `setup`, `cache`, `machine`, `deps`, `tests`, `rdeps`, and `timings` if there's a timings report lying around. It ends with a summary, sorted by severity:

```text
== summary
  [high] The machine is busy: timings will be noisy and slower
  [medium] The target dir filesystem is 94% full
  [medium] 6 heavy crates are built in more than one version
  [medium] tala-agent has 108449 lines of Rust
  [low] cargo is limited to 16 jobs on 32 CPUs
  [low] Editing tala-spool rebuilds 98 of 183 workspace crates
  [low] tala-admin-proto-orgs is 43509 lines, mostly generated code
  [low] tala-admin-proto-personas is 50889 lines, mostly generated code
  [low] tala-room-ops uses 2 items from tala-config (31k lines)
    saw: tala-room-ops depends on tala-config (31107 lines) but uses only
         `tala_config::config::Project`, `tala_config::state::Room`. An edit to tala-config
         rebuilds 74 workspace crates; without this edge it would rebuild 69.
```

It took about three seconds on that workspace. Start here. Then use the subcommands for the parts you care about.

Notice what's missing from that summary: the trait-solving problem that was actually making this workspace slow. The quick checks can't see inside the compiler. For that, you need `timings` to find the slow crate and `check-cost` to find out why it's slow.

## `cargo faster timings`

Cargo can record a timeline of a build with `--timings`. It writes an HTML report to `target/cargo-timings/`. `cargo faster timings` reads that report and pulls out the parts that matter:

```console
$ cargo check --workspace --timings
$ cargo faster timings
```

Or, to run the build and read the report in one step:

```console
$ cargo faster timings --run
```

That runs `cargo check --workspace --timings` under `nice -n 10`. Use `--nice 0` to run it at normal priority. You can also pass a report file to read, including one from another machine. `--baseline FILE` names an earlier report to compare with (see below).

`--cmd` picks the build to time, and anything after `--` goes to cargo:

```console
$ cargo faster timings --cmd build                  # cargo build --workspace --timings
$ cargo faster timings --cmd clippy                 # cargo clippy --workspace --timings
$ cargo faster timings --cmd test -- --profile ci   # cargo nextest run --workspace --no-run --timings --profile ci
```

`--cmd test` times the test build: `cargo nextest run --no-run` when nextest is installed, `cargo test --no-run` otherwise. Every build is `--workspace`, never `-p`, so it resolves features and cache keys the way your usual build does. Clippy runs rustc through `clippy-driver`, so its units don't share artifacts or cache entries with `cargo check`: time the one you wait on.

### What an edit to one crate costs

`--edit CRATE` measures the build you get after editing a crate, without you editing anything:

```console
$ cargo faster timings --edit tala-mx --cmd clippy
+ cargo clippy --workspace
+ cargo clippy --workspace --timings
== timings
  build        cargo clippy --workspace --timings
  edit         tala-mx: a new public function (the crate's API changed) in crates/tala-mx/src/lib.rs, restored after the build
  ...
  rebuilt      23 workspace crates, from tala-mx
```

It first runs the build without `--timings`, so the timed build starts from an up-to-date tree. Then it appends a probe function to the crate's root file, times the build, and puts the file back (also on an error or Ctrl-C). The probe is a new public function, which changes the crate's API the way adding or changing a public item does. `--body` changes only a private function's body instead, which is the cheapest kind of edit: compare the two to see what your dependents pay for API changes. A proc-macro crate always gets a body edit. Restoring the file is an edit too, so your next build rebuilds the same crates once more.

Here's a report from an edit to the application crate:

```text
== timings
  wall         84.3s
  units        7 compiled, 1198 fresh
  parallelism  2.1 units at a time, CPU 55% on average

  critical path (each step waited for the one above it)
  start  took   waited  unit
  0.38s  42.3s  full    tala-agent (check)
  42.6s  30.9s  full    tala-agent-bin (check)
  76.0s  8.25s  full    tala-agent-bin tala-agent "bin" (check-test)

  slowest units
  took   % wall  unit
  52.6s     62%  tala-agent (check-test)
  42.3s     50%  tala-agent (check)
  35.3s     42%  tala-agent-bin (check-test)
  30.9s     37%  tala-agent-bin (check)
  ...

  [high] tala-agent (check-test) serializes the build
    saw: tala-agent (check-test) took 52.6s of the 84.3s build while little else could run:
         ...
    fix: Find out what's slow inside it with `cargo faster check-cost -p tala-agent`. If
         it's just big, split it (see `cargo faster rdeps`) so the pieces build in parallel
         and edits rebuild less.
```

The *critical path* is the chain of units the build had to wait for, one after another. Making anything else faster doesn't help. The `waited` column says whether a step waited for the previous one to finish completely (`full`), or only for its metadata (`rmeta`, which is what [pipelining](guide.md#crates-as-compilation-units-and-pipelining) gives you).

These units are all `check` units, which means no LLVM at all. So the time is in the frontend: type checking, trait solving, borrow checking. That's where `check-cost` comes in.

The timings check also finds:

- `serial-crate`: a unit that runs while little else can, so most of your cores sit idle;
- `long-codegen`: a unit that spends most of its time in LLVM, which usually means big generic instantiations or a release profile doing more than it needs to;
- `low-parallelism`: a build that keeps few cores busy overall;
- `critical-path-dominated`: one unit that is most of the critical path;
- `slow-build-script`: a build script that holds up the crates waiting for it.
- `post-metadata-analysis`: check units that spend a long time after writing their metadata, which is analysis such as lints and borrow checking, not codegen (a check has none);
- `slow-links`: a build that spends a lot of its time linking binaries and tests, or ends with a long stretch of nothing but links;
- `contention`: units that took far longer than the same crates did in an earlier check, so the machine was busy, not the code slow;
- `critical-path-waiting`: steps on the critical path that sat idle after they were ready, or waited for the cache's scheduler;
- `wrapper-wait`: a lot of time spent waiting for the rustc wrapper's scheduler or for another build compiling the same thing;
- `edit-rebuilt-many`: an edit that recompiled many workspace crates, and which crates it started from;
- `cheap-to-cut-edge`: a step on the critical path that waited for a big workspace crate it uses only a few items from (the same check as `rdeps`, with the seconds that crate took on the path);
- `slow-build`: a slow build that none of the above explains, with what was measured and what to try next.

### Where a build's time went

After an edit to a crate near the root of a workspace, `cargo build --timings` and `cargo faster timings` say more than the critical path:

```text
== timings
  wall           152s
  units          94 compiled, 0 fresh
  parallelism    2.7 units at a time, CPU 74% on average, jobs=16 on 32 CPUs
  cache          94 compiled incrementally (passthrough, no cache lookup) (414s); 0.00s waiting for the wrapper
  compared with  an earlier check (67.9s wall): 60 units, 1.2x their time there (median)
  rebuilt        88 workspace crates, from tala-core
  baseline       target/cargo-timings/cargo-timing-20261117T092210.551Z.html

  critical path (each step waited for the one above it; gap: idle after it was ready)
  start  took   gap     cache  wait    waited  unit
  0.66s  7.40s  -       incr   -       full    tala-core
  8.06s  24.1s  -       incr   -       meta    tala-config
  ...
  74.7s  51.2s  -       incr   -       meta    tala-admin
   126s  17.5s  0.010s  incr   -       meta    tala-agent-bin
   143s  8.16s  -       incr   -       full    tala-agent-bin tala-agent "bin"

  where the time went
  busy   % busy  on path  units  kind
   389s     94%     138s     88  libraries (built)
  24.7s      6%    8.16s      6  binaries and tests (codegen + link)
```

- **cache**: when `RUSTC_WRAPPER` is kache, `cargo faster timings` asks `kache stats --since … --root <workspace> --format json` for the build's time window and matches its records to the report's units by crate and start time. Each unit is a `hit` (restored from the cache), a `miss` (compiled), `incr` (an incremental compile kache passed to rustc) or `pass` (passed through for another reason), with the time it waited for kache's scheduler. Records older than a week are gone, so reports from long ago or another machine show no cache column.
- **compared with**: a build is compared with a `cargo check` report written just before it in the same directory, or with the report you pass with `--baseline FILE`. A library's time up to its metadata is compared with its check. When units take several times longer than they did there, the `contention` finding says the machine, not the code, was the problem.
- **rebuilt**: how many workspace crates compiled, and which of them had no rebuilt workspace dependency: where the edit was.
- **gap**: how long a step on the critical path sat idle after the unit it waited for had finished.
- **where the time went**: busy seconds and seconds on the critical path, split into checks, library builds, binaries and tests, and build scripts.

So a slow build after an edit to `tala-core` is explained as: 88 crates rebuilt, all incrementally (not cacheable), on a path through a few big crates; not waits, and not a busy machine.


## `cargo faster check-cost`

This is the one that finds causes rather than symptoms. It self-profiles the compiler while it checks one crate, and then it tells you where the time went: which compiler queries, which trait predicates, and which function bodies asked for them.

```console
$ cargo faster check-cost -p tala-agent-bin
```

Here's what it does, so you know what you're running:

1. It updates the modification time of the crate's root source file. That's the only change it makes to your tree, and the contents stay the same.
2. It runs `cargo check --workspace -vv` with a filter for the target you picked (`--lib`, `--bin NAME`, `--test NAME`, ...; `--profile test` for `--tests`) and captures the exact `rustc` command line Cargo uses for your crate. That build checks the selected targets in every member and everything they depend on, so on a cold cache it is most of the run. It always uses `--workspace`, and never `cargo -p`, even when you pass `-p` to `cargo faster`. Some caches, like kache, key on the set of packages being built, so `cargo -p` would miss the cache and rebuild your dependencies.
3. It runs that `rustc` command again, by itself, with self-profiling turned on: `-Zself-profile -Zself-profile-events=default,args`, with `CARGO_INCREMENTAL=0` and no `RUSTC_WRAPPER`.
4. It reads the profile and analyzes it.

`-Z` flags are normally nightly-only. `cargo faster` sets `RUSTC_BOOTSTRAP=1` so your stable compiler accepts them. That's an unstable escape hatch, and the report says so on a `measured with` line, so that nobody mistakes these numbers for an ordinary build. Profiling your crate with the same compiler you normally use matters: a nightly might have a different trait solver.

The profiler costs time of its own: at the end of the run it turns every recorded query key into a string (`self_profile_alloc_query_strings`), which takes seconds with `args` on a big crate. That isn't compile time, so it's left out of the query table, the profiled time and every percentage, and shown once on a `profiler overhead` line. The `rustc wall time` line is the replay as measured from outside, overhead included.

Incremental compilation is off so the profile shows the full cost of checking the crate, not whatever happened to be cached. The wrapper is off so a cache can't hand back a stored result instead of compiling.

Options:

- `-p NAME` picks the workspace member. The default is the root package, or the biggest member if there isn't one.
- `--lib`, `--bin NAME`, `--test NAME`, `--example NAME` and `--bench NAME` pick the target. The default is the library, or the first binary if there is no library. `--test NAME` is an integration test, `tests/NAME.rs`.
- `--tests` profiles the `--test` build of the library, binary or example: the unit-test crate, with its `#[cfg(test)]` modules and the test harness. That is the crate `cargo test` and `cargo nextest` rebuild after every edit, and it is often much bigger than the library itself. Integration tests and benchmarks are always built with `--test`.
- `-- ARGS` passes extra arguments to `cargo check`, like `-- --features postgres`. Target flags here (`-- --tests`, `-- --all-targets`) replace the filter `cargo faster` would add. When they produce both builds of the library, the normal one is profiled, unless they include `--tests` or `--profile test`; then it is the `--test` one.
- `--keep DIR` writes the profile into `DIR` and keeps it, so you can analyze it again with `--profdata` or open it with `summarize` and `crox` from [measureme](https://github.com/rust-lang/measureme).
- `--profdata FILE` analyzes an existing `.mm_profdata` file without building anything.
- `--nice N` sets the priority, as with `timings`.

The header and the `target` line say which build was profiled: `(lib)`, `(bin)`, `(lib, --test)` for unit tests, `(test)` for an integration test. With `--test`, `measured with` starts with `rustc --test`. Both come from the command line recorded in the profile, so `--profdata FILE` shows them too.

Here's a real profile of the binary crate from the case study, after the first fix:

```text
== check-cost: tala_agent_bin (bin)
  target         crates/tala-agent-bin/src/main.rs (bin, without #[cfg(test)] code)
  profiled time  38.7s wall, 38.6s across threads
  trait solving  32.1s (83%)
  proc macros    0.076s (0%)
  codegen        0.004s (0%)
  measured with  rustc with RUSTC_BOOTSTRAP=1 -Zself-profile -Zself-profile-events=default,args, CARGO_INCREMENTAL=0, no RUSTC_WRAPPER

  top queries by self time
  self    %      count   query
   32.1s  83.0%  123475  evaluate_obligation
   1.41s   3.6%   18197  type_op_prove_predicate
   1.33s   3.5%     513  check_coroutine_obligations
   ...

  trait solving by what is being proved
  self   %      count  category
  6.55s  16.9%  17913  types from tala_agent
  4.33s  11.2%    921  async bodies in tala_agent
  4.03s  10.4%    720  types from matrix_sdk
  2.68s   6.9%  25857  types from std
  2.00s   5.2%   4938  types from tokio
  ...

  most expensive obligations
  self    count  bodies  predicate
   0.47s     54      47  tala_agent::bot::Bot: Sync
   0.46s     54      47  tala_agent::bot::Bot: Send
   0.45s     54      47  std::sync::Weak<tala_agent::bot::Bot>: Send
   ...

  bodies doing the most trait solving
  self   %      preds  body
  8.06s  20.9%   2977  tala_agent_bin::canonical_persona::{{impl}}::invite
  3.14s   8.1%   1291  tala_agent_bin::host::{{impl}}[2]::invite
  ...

  proc macro expansion
  time    %     uses  macro
  0.073s  0.2%    32  #[async_trait::async_trait]
  ...
```

Here's how to read it, from the top:

- **profiled time** and **trait solving** tell you what kind of problem you have. If trait solving is most of the time, keep reading this page. If `codegen` is large, see [generic code](guide.md#generic-code-and-monomorphization). If `proc macros` is large, see [procedural macros](guide.md#procedural-macros).
- **top queries** is what `summarize` from measureme would show you. `evaluate_obligation` is the trait solver.
- **trait solving by what is being proved** groups the solver's time by the crate that defines the type in each predicate. Here, proofs about `matrix_sdk` types are 4 seconds of a crate that doesn't define any of them. That's the sign of a heavy type from a dependency leaking into your async code.
- **most expensive obligations** lists single predicates. The `count` and `bodies` columns are the ones to look at: `Bot: Send` was proved 54 times, in 47 different function bodies. Each proof costs little, but the compiler doesn't share the answer between bodies, so you pay it again and again.
- **bodies doing the most trait solving** attributes the solver's time to the function that asked for it. This is where you start editing. In this case, one `invite` method is a fifth of the crate.
- **time by module** adds up the self time of every body query (`typeck_root`, `mir_borrowck`, `mir_built` and friends) by top-level module, with closures and async blocks counted towards the item they're in. **bodies by total frontend time** is the same per item. When no table has a hot spot and these two show the time spread over many items, the crate is slow because it is big: the `check-time-is-crate-size` finding names the modules to [split out](guide.md#splitting-a-big-crate) first, with their line counts when the profile came from a live run.

The findings then put it together:

```text
  [high] Trait solving is 83% of tala_agent_bin's check time
  [medium] Send/Sync proofs for `tala_agent::bot::Bot` are repeated in every async body
  [medium] One function body takes a large share of type checking
```

Each finding has a fix: erasing the heavy type behind `Arc<dyn Trait + Send + Sync>`, or boxing the futures at a module boundary. The guide explains both, in [Erasing a heavy type](guide.md#erasing-a-heavy-type) and [Boxing futures at a boundary](guide.md#boxing-futures-at-a-boundary).

A profile of a big crate on a busy machine can take several minutes. That's because it's a full, non-incremental check. Run it once, keep the profile with `--keep`, and analyze it again with `--profdata` as often as you like.

## `cargo faster setup`

`setup` reads your profiles and Cargo configuration, and checks the settings that most often matter:

```text
== setup
  dev debuginfo  line-tables-only (Cargo.toml)
  linker         rust-lld (default since Rust 1.90 on x86_64 Linux)
  jobs           16 (CARGO_BUILD_JOBS)

  [low] cargo is limited to 16 jobs on 32 CPUs
  [info] mold or wild is installed but lld is in use
```

It looks at:

- **the linker**: whether you link with the system default, `lld`, `mold`, or `wild`, and whether a faster one is installed;
- **debug info**: full debug info in `dev` is the biggest single input to link time, and `line-tables-only` keeps backtraces while dropping most of it;
- **split debuginfo**: `packed` costs an extra step on every link;
- **codegen units**: a low `codegen-units` in `dev` serializes LLVM;
- **opt-level**: your own crates at `opt-level` above 0 in `dev` make every edit pay for optimization, where optimizing only dependencies (`[profile.dev.package."*"]`) doesn't;
- **incremental**: whether something turned it off;
- **`-Zshare-generics`**: already the default for unoptimized builds, so usually redundant;
- **the parallel frontend** (`-Zthreads`), and `-C target-cpu=native`;
- **`RUSTFLAGS`** in the environment, which silently replaces the `rustflags` in your Cargo config, and changes every cache key;
- **jobs**: `CARGO_BUILD_JOBS` or `build.jobs` limiting parallelism.

It reads settings the way cargo does, environment included: rustflags come from `CARGO_ENCODED_RUSTFLAGS`, else `RUSTFLAGS`, else `target.<triple>.rustflags` plus `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, else `build.rustflags` plus `CARGO_BUILD_RUSTFLAGS`; incremental from `CARGO_INCREMENTAL`, else `build.incremental` (or `CARGO_BUILD_INCREMENTAL`), else the profile. Each line names where its value came from, e.g. `linker  mold (CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS)`.

Every one of these is a tradeoff, and the finding says what you give up. Turning off debug info makes builds faster and debugging worse. Optimizing dependencies makes tests faster and clean builds slower. `setup` tells you what you have; you decide.

## `cargo faster cache`

`cache` checks your compiler cache and your incremental state:

```text
== cache
  rustc wrapper      .../kache-2.0.1/bin/kache (RUSTC_WRAPPER)
  incremental (dev)  on (default for dev)
  target dir         /home/me/proj/app/target
  worktrees          375 (82 without target/, 5 with incremental state)
  kache (24h)        72% hits, 54% by compile time, 59213s saved
  kache restores     102.9 GiB reflinked, 23.9 GiB copied
  kache session      started 10:07 UTC, 12 min ago, ran 0.035s
  kache last build   no compiles in the latest session (only probes); run a build first for per-build numbers

  [info] 82 worktree(s) have no target directory yet
```

It knows about `sccache` and `kache`. With kache, it reads `kache stats --json` for the last day and for the last build in this workspace, and `kache explain --json` for why the misses missed. The last build is kache's latest *session* in this workspace, and a session can span several cargo commands, so `kache session` says when it started and how long it ran. When the latest builds in it were all incremental compiles that kache passed straight to rustc, a `kache latest` line says so ("94 crates compiled incrementally, starting 3 min ago, with no cache lookups"): the hit rate and the miss causes are from the session's earlier builds, and the miss causes are labeled with when kache recorded them. It reports hit rates by count *and* by compile time. Those can differ a lot: a cache that hits 98% of crates but misses your three biggest ones still leaves you with most of the build.

It also looks at git worktrees, because they change the picture. Every worktree has its own target directory, so the first build in a new one is cold: nothing incremental, and whatever the cache can give you at best. A few things we learned the hard way, which `cache` points out when it sees the setup that causes them:

- Incremental compilation only helps repeated builds in the same worktree.
- Seeding incremental state from another worktree was *slower* than not seeding: 179 and 204 seconds, against 139 seconds without it.
- Clippy runs never seeded at all.

See [Caching](guide.md#caching).

## `cargo faster machine`

Before you believe any timing, know what else the machine was doing:

```text
== machine
  cpus           32
  load           74.8 (1 min), 69.2 (5 min) = 234% of the CPUs
  other builds   21 rustc, 0 cargo processes
  memory         179.6 GiB available of 249.1 GiB
  target dir fs  /home/me/proj/app/target on /home (btrfs), 221.9 GiB free
  cache fs       /home/me/.cache/kache-store on /home (btrfs), 221.9 GiB free
  tmp fs         /tmp on /tmp (unknown), 817.1 GiB free

  [high] The machine is busy: timings will be noisy and slower
  [medium] The target dir filesystem is 94% full
```

It checks load against the number of cores, other `rustc` and `cargo` processes, memory and swap, free disk space, and whether your target directory and your cache are on the same filesystem. If they aren't, a cache hit is a full copy instead of a reflink.

A load average of 74 on 32 cores means every number on this page could be twice what it would be on a quiet machine. That's fine, as long as you compare before and after under similar load, back to back. See [Measuring fairly](guide.md#measuring-fairly).

## `cargo faster deps`

`deps` looks at your dependency graph for things that cost compile time:

```text
== deps
  packages           811
  proc macros        41
  duplicated crates  32
  tokio features     bytes, default, fs, full, io-std, ...

  [medium] 6 heavy crates are built in more than one version
    saw: base64 0.21.7 + 0.22.1 + 0.23.1, itertools 0.14.0 + 0.15.0, rand 0.8.8 + 0.9.5 +
         0.10.3, syn 2.0.119 + 3.0.7, thiserror-impl 1.0.69 + 2.0.21, windows-sys 0.52.0 +
         0.61.2. 32 crates are duplicated in total.
  [medium] #[async_trait] is used 62 times in 9 workspace crates
    saw: 62 #[async_trait] attributes: tala-agent 29, tala-session-engine 9, tala-github-api
         8, tala-delegate-runtime 5, tala-session-worker 4 and 4 more. They define 7 traits,
         7 held as `dyn` (`CodexApi`, `GithubHttp`, `Hooks`, `ImageApi`, `OwnerGh`). 23 are
         impls of traits defined outside the workspace, which stay until upstream drops the
         macro.
```

It finds:

- `tokio` with the `full` feature asked for by a workspace crate;
- `syn` with `full`, and more than one major version of `syn`;
- duplicate versions of heavy crates;
- test-only features (`testing`, `test-util`, `mock*`, `fake*`) enabled from `[dependencies]` instead of `[dev-dependencies]`, which compile test helpers into every normal build ([Keep test features out of normal builds](guide.md#keep-test-features-out-of-normal-builds));
- proc macros;
- `#[async_trait]` attributes counted in the workspace's sources (not its manifests), the traits they define, and how many of those are held as `dyn`. Ten or more uses is `[medium]`; `check-cost` then tells you what they actually cost.

Features are unified across the whole graph. In this workspace, `full` shows up in tokio's features even though no workspace crate asks for it: a dependency does. `cargo tree -e features -i tokio` tells you which one.

See [Dependencies and features](guide.md#dependencies-and-features).

## `cargo faster tests`

Slow tests are often not slow to compile at all. `tests` checks how they're run:

```text
== tests
  nextest               installed, .config/nextest.toml present
  test binaries         198 (6 integration test files)
  target runner         scripts/kache-test-runner (target.x86_64-unknown-linux-gnu.runner in .cargo/config.toml)
  runner override       scripts/gate:180 ("cargo nextest|$runner_var=$PWD/scripts/test-runner $cargo nextest run ...")
  NEXTEST_TEST_THREADS  4
  database crates       sqlx
  db tests: template databases  crates/tala-core/src/pg_testing.rs:602
  db tests: fsync off           crates/tala-core/src/pg_testing.rs:231
  db tests: tmpfs               crates/tala-core/src/pg_testing.rs:4

  [low] `scripts/kache-test-runner` steps aside under nextest
  [info] NEXTEST_TEST_THREADS=4 sets the threads for plain nextest runs
```

A *target runner* wraps every execution of a test binary, and nextest runs every test in its own process, so a runner starts once per test. In the case study, tests that took 0.00 seconds waited 3 to 8 seconds each in the runner, and the runner took about 44% of all test-seconds. `tests` reads the runner: if it's a script that checks `NEXTEST_EXECUTION_MODE` and execs the test directly, it costs a millisecond per test and the finding is `info`; if it doesn't, it's `high` under nextest. It also finds scripts that set `CARGO_TARGET_<TRIPLE>_RUNNER` themselves, like the `scripts/gate` above, whose runner beats the configured one for those runs (here `low`: the gate's runner doesn't step aside).

Beyond runners, it checks:

- whether you use nextest at all;
- `NEXTEST_TEST_THREADS` in the environment, which silently beats `test-threads` in your profile, unless the scripts that run that profile pass `--test-threads` themselves (as `scripts/gate` does here, so it's only `info`);
- database-backed tests, and how to make each test database cheap: Postgres on tmpfs, and cloning a template database instead of running migrations for every test. It looks for what the workspace already does first (here all three, so there's no finding);
- many integration test files, each of which is its own binary to link.

All of that is read from configuration. To measure, add `--run`:

```console
$ cargo faster tests --run -- --profile gate --test-threads 8
```

It times `cargo nextest run --workspace --no-run --timings`, runs the tests (`--no-fail-fast`, with nextest's libtest-json output), and adds a `test run` section and the build's `timings` section:

```text
== test run
  nextest profile   gate
  build (--no-run)  28.4s
  run               661s
  tests             3953 run, 1 failed, 11 skipped
  test threads      8 (--test-threads)
  test-seconds      1191s over 661s × 8 threads (23% busy)

  slowest tests
  took   binary                          test
   600s  tala-runner::cli                session_worker_reports_invalid_request_as_json
  8.49s  tala-client::tala_client        files::tests::a_big_image_is_scaled_to_fit
  ...
  test binaries by total test time (fastest ≈ the binary's per-test startup)
  total  tests  slowest  fastest  binary
   600s      5     600s   0.052s  tala-runner::cli
   139s    754    3.87s   0.009s  tala-agent::tala_agent
  ...

  [medium] 1 test(s) ran into the slow-timeout (60.0s)
  [info] Test threads were busy 23% of the run
```

Here one hung test, killed by the profile's terminate-after at 600s, is most of the 11-minute run; without it the suite needs about a minute on 8 threads.

Its findings: `test-build-dominates` (the build takes over a minute and more than twice the run), `tests-near-slow-timeout` (tests at half the profile's `slow-timeout` or more), and `test-threads-idle` (the threads were busy less than half the run). Arguments after `--` go to both nextest commands; never pass `-p` there if you use a caching wrapper, as it changes feature resolution and the cache keys.

See [Test suites](guide.md#test-suites) and [Measuring a test run](guide.md#measuring-a-test-run).

## `cargo faster rdeps`

`rdeps` shows the shape of your workspace: which crates rebuild the most when you edit them, and which crates are big enough to split.

```console
$ cargo faster rdeps            # the whole workspace
$ cargo faster rdeps app-core   # one crate (or -p app-core)
```

```text
== rdeps
  workspace crates  183

  crates whose edits rebuild the most
  rebuilds  lines  crate
        98   6417  tala-spool
        95    150  tala-ids
        91   2640  tala-hook-wire
        87  11878  tala-core
        ...

  dependency edges that are cheap to cut
  items  rebuilds without it  edge
      2             74 -> 69  tala-room-ops -> tala-config

  [medium] tala-agent has 108449 lines of Rust
    saw: tala-agent is 108449 lines; its biggest modules are delegates (14378 lines), agent
         (11366 lines), personas (9094 lines), guests.rs (3243 lines).
  [low] Editing tala-spool rebuilds 98 of 183 workspace crates
  [low] tala-admin-proto-personas is 50889 lines, mostly generated code
  [low] tala-room-ops uses 2 items from tala-config (31k lines)
    saw: tala-room-ops depends on tala-config (31107 lines) but uses only
         `tala_config::config::Project`, `tala_config::state::Room`. An edit to tala-config
         rebuilds 74 workspace crates; without this edge it would rebuild 69.
```

A crate near the bottom of the graph decides the cost of most edits, because every edit to it rebuilds everything above it. A big crate costs you on every edit to it, and can't use more than a few cores. `rdeps` names the biggest modules, which are the candidates to move into crates of their own.

For the crates that rebuild the most, `rdeps` also reads each direct dependent's source and counts the distinct items it uses from the crate (`tala_config::config::Project` and `tala_config::state::Room` are two). When a dependent uses at most 5 items from a crate of 5,000 lines or more, and dropping the edge would spare at least 3 crates on an edit (or the edge holds up the critical path for 2 s or more), it reports a `cheap-to-cut-edge` finding with what an edit rebuilds now and without the edge. Move those items into a small crate lower down and the dependent stops waiting for, and rebuilding with, the big one. `cargo faster rdeps CRATE` lists every dependent of that crate with its item count; `?` means a glob or alias hid what's used. See [Cutting a dependency edge](guide.md#cutting-a-dependency-edge).

Before you split a crate, though, run `check-cost` on it. The crate in the case study looked too big, and splitting it would not have fixed anything. See [The shape of your workspace](guide.md#the-shape-of-your-workspace).

## A workflow

Putting it together, here's the order that has worked for us:

1. `cargo faster`. Fix anything `high` that's cheap to fix, especially in `tests` and `setup`.
2. `cargo faster machine`. Note the load, and don't compare numbers across very different loads.
3. Make the edit you care about, then `cargo faster timings --run`. Find the units on the critical path.
4. `cargo faster check-cost -p <crate> --keep /tmp/prof` for the slowest `check` unit.
5. Fix the top body or the top type. Measure the same way, back to back.

For agents, the same workflow is written down as a skill in [`skills/debug-slow-rust-builds`](../skills/debug-slow-rust-builds/SKILL.md).
