# cargo-faster

Find out why your Rust build is slow and how to make it faster.

`cargo faster` measures your build and reports what it found. Each finding comes with the numbers it saw, why they cost you time, a fix with its tradeoffs, and a link into [the guide](docs/guide.md).

```console
$ cargo install --locked cargo-faster
$ cargo faster
```

```text
== summary
  [high] The machine is busy: timings will be noisy and slower
  [medium] The target dir filesystem is 94% full
  [medium] 6 heavy crates are built in more than one version
  [medium] NEXTEST_TEST_THREADS overrides your nextest profile
  [medium] tala-agent has 108449 lines of Rust
  [low] cargo is limited to 16 jobs on 32 CPUs
  [low] Editing tala-spool rebuilds 98 of 183 workspace crates
```

The quick report reads your configuration, caches, test setup, dependency graph, and machine, and builds nothing. To see inside the compiler, find the slow crate with `cargo faster timings --run`, then self-profile it:

```console
$ cargo faster check-cost -p tala-agent-bin
```

```text
== check-cost: tala_agent_bin
  profiled time  38.7s wall, 38.6s across threads
  trait solving  32.1s (83%)
  measured with  RUSTC_BOOTSTRAP=1 -Zself-profile -Zself-profile-events=default,args, CARGO_INCREMENTAL=0, no RUSTC_WRAPPER

  most expensive obligations
  self    count  bodies  predicate
   0.47s     54      47  tala_agent::bot::Bot: Sync
   0.46s     54      47  tala_agent::bot::Bot: Send

  bodies doing the most trait solving
  self   %      preds  body
  8.06s  20.9%   2977  tala_agent_bin::canonical_persona::{{impl}}::invite
  3.14s   8.1%   1291  tala_agent_bin::host::{{impl}}[2]::invite

  [high] Trait solving is 83% of tala_agent_bin's check time
  [medium] Send/Sync proofs for `tala_agent::bot::Bot` are repeated in every async body
```

`check-cost` runs on the stable toolchain. It sets `RUSTC_BOOTSTRAP=1` so that stable `rustc` accepts `-Zself-profile`, and it says so in the output.

## Subcommands

| Command | What it checks |
|---|---|
| `cargo faster` | Everything below that doesn't build: a quick overall report |
| `timings` | A `--timings` report: critical path, crates that serialize the build, long codegen |
| `check-cost` | Self-profiles one crate: top queries, trait solving by type and by function body, proc macros |
| `setup` | Linker, debug info, codegen units, opt-levels, jobs, rustflags |
| `cache` | sccache or kache hit rates, incremental, cold worktrees |
| `machine` | Load, other builds, memory, disk, filesystems |
| `deps` | Heavy features, duplicate versions, proc macros, async-trait |
| `tests` | nextest, target runners, thread limits, database-backed tests |
| `rdeps` | What rebuilds when a crate changes, and which crates are worth splitting |

Every command takes `--json`. See [Using cargo faster](docs/cargo-faster.md) for details and real output.

## Docs

- [Why is my Rust build slow?](docs/guide.md): how `rustc` spends its time, how to measure it, and what to do about it, with a case study of a 312-second check.
- [Using cargo faster](docs/cargo-faster.md): each subcommand, and how to read its output.
- [`skills/debug-slow-rust-builds`](skills/debug-slow-rust-builds/SKILL.md): the same workflow as an agent skill.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
