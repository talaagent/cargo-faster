# cargo-faster

[![CI][ci-badge]][ci] [![crates.io][version-badge]][crate] [![downloads][downloads-badge]][crate] [![docs.rs][docs-badge]][docs] [![lib.rs][librs-badge]][librs] [![Rust 1.88+][msrv-badge]][msrv] [![License: Apache-2.0][license-badge]][license]

Find out why your Rust build is slow and how to make it faster.

`cargo faster` measures your build and reports what it found. Each finding comes with the numbers it saw, why they cost you time, a fix with its tradeoffs, and a link into [the guide](docs/guide.md).

```console
$ cargo install --locked cargo-faster
$ cargo faster
```

```text
== conditions (results may be noisy)
  The machine is busy: timings will be noisy and slower

== summary
  [medium] The target dir filesystem is 94% full
  [medium] 6 heavy crates are built in more than one version
  [medium] app-agent has 108449 lines of Rust
  [low] cargo is limited to 16 jobs on 32 CPUs
  [low] Editing app-spool rebuilds 98 other workspace crates (of 183)
```

The quick report reads your configuration, caches, test setup, dependency graph, and machine, and builds nothing. To see inside the compiler, find the slow crate with `cargo faster timings --run` (`--cmd build|clippy|test` for other builds, `--edit CRATE` to time the rebuild after an edit to one crate), then self-profile it:

```console
$ cargo faster check-cost -p app-agent-bin
```

```text
== check-cost: app_agent_bin (bin)
  profiled time  38.7s wall, 38.6s across threads
  trait solving  32.1s (83%)
  measured with  rustc with RUSTC_BOOTSTRAP=1 -Zself-profile -Zself-profile-events=default,args, CARGO_INCREMENTAL=0, no RUSTC_WRAPPER

  most expensive obligations
  self    count  bodies  predicate
   0.47s     54      47  app_agent::bot::Bot: Sync
   0.46s     54      47  app_agent::bot::Bot: Send

  bodies doing the most trait solving
  self   %      preds  body
  8.06s  20.9%   2977  app_agent_bin::canonical_persona::{{impl}}::invite
  3.14s   8.1%   1291  app_agent_bin::host::{{impl}}[2]::invite

  [high] Trait solving is 83% of app_agent_bin's check time
  [medium] Send/Sync proofs for `app_agent::bot::Bot` are repeated in every async body
```

`--lib`, `--bin`, `--test`, `--example` and `--bench` pick the target, and `--tests` profiles the unit-test (`--test`) build, which is what `cargo test` rebuilds after an edit. `check-cost` runs on the stable toolchain. It sets `RUSTC_BOOTSTRAP=1` so that stable `rustc` accepts `-Zself-profile`, and it says so in the output.

## Subcommands

| Command | What it checks |
|---|---|
| `cargo faster` | Everything below that doesn't build: a quick overall report |
| `timings` | A `--timings` report: critical path, crates that serialize the build, long codegen, edges on the path that are cheap to cut |
| `check-cost` | Self-profiles one crate: top queries, trait solving by type and by function body, time by module, proc macros |
| `setup` | Linker, debug info, codegen units, opt-levels, jobs, rustflags |
| `cache` | sccache or kache hit rates, incremental, cold worktrees |
| `machine` | Load, other builds, memory, disk, filesystems |
| `deps` | Heavy features, test features in normal builds, duplicate versions, proc macros, async-trait |
| `tests` | nextest, target runners, thread limits, database-backed tests |
| `rdeps` | What rebuilds when a crate changes, which crates are worth splitting, and dependency edges that use only a few items from a big crate |
| `check` | Your rules from `cargo-faster.toml`: layering, flagged and duplicate dependencies, and with `--timings` cold-check and edit budgets. Exits 1 when one is broken |

Every command takes `--json`, and `--fail-on high|medium|low` to exit 1 on a finding that severe (for CI). A `cargo-faster.toml` at the workspace root records hot crates, findings accepted by design (with a reason), and the rules `cargo faster check` enforces; see [the configuration file](docs/cargo-faster.md#the-configuration-file). See [Using cargo faster](docs/cargo-faster.md) for details and real output.

## Platforms

cargo faster is Linux-first. Every check works on Linux. On macOS the machine check reads load, memory and processes from `sysctl`, `vm_stat` and `ps`, and filesystems from `df -P` and `mount`. Anything it can't measure is listed as `unavailable` rather than guessed. Everything else (setup, cache, deps, tests, rdeps, timings, check-cost) is the same on both. Windows is untested.

## Docs

- [Why is my Rust build slow?](docs/guide.md): how `rustc` spends its time, how to measure it, and what to do about it, with a case study of a 312-second check.
- [Using cargo faster](docs/cargo-faster.md): each subcommand, and how to read its output.
- [`skills/debug-slow-rust-builds`](skills/debug-slow-rust-builds/SKILL.md): the same workflow as an agent skill.

## MSRV

cargo-faster and its parser crates need Rust 1.88 (let chains), declared as
`rust-version` in Cargo.toml. CI builds and tests the workspace on it.
Raising the MSRV is a minor-version change.

## Contributing

Pull requests welcome; see [CONTRIBUTING.md][contributing]. Report
security issues privately as described in [SECURITY.md][security].

[contributing]: https://github.com/talaagent/cargo-faster/blob/main/CONTRIBUTING.md
[security]: https://github.com/talaagent/cargo-faster/blob/main/SECURITY.md

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

[ci-badge]: https://github.com/talaagent/cargo-faster/actions/workflows/ci.yml/badge.svg
[ci]: https://github.com/talaagent/cargo-faster/actions/workflows/ci.yml
[version-badge]: https://img.shields.io/crates/v/cargo-faster.svg
[downloads-badge]: https://img.shields.io/crates/d/cargo-faster.svg
[crate]: https://crates.io/crates/cargo-faster
[docs-badge]: https://docs.rs/cargo-faster/badge.svg
[docs]: https://docs.rs/cargo-faster
[librs-badge]: https://img.shields.io/badge/lib.rs-cargo--faster-purple.svg
[librs]: https://lib.rs/crates/cargo-faster
[msrv-badge]: https://img.shields.io/badge/rust-1.88%2B-orange.svg
[msrv]: https://github.com/talaagent/cargo-faster#msrv
[license-badge]: https://img.shields.io/badge/license-Apache--2.0-blue.svg
[license]: https://github.com/talaagent/cargo-faster/blob/main/LICENSE
