# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/talaagent/cargo-faster/compare/v0.0.0...v0.1.0) - 2026-11-18

### Other

- Parse Rust sources with syn and proc-macro2, versions with semver, kache reports with typed serde ([#89](https://github.com/talaagent/cargo-faster/pull/89))
- Add a cargo-faster.toml configuration and cargo faster check ([#88](https://github.com/talaagent/cargo-faster/pull/88))
- Find fingerprints in cargo 1.100's build-dir layout; don't compare test builds with a check ([#85](https://github.com/talaagent/cargo-faster/pull/85))
- Readable unit labels, target-specific check-cost advice and capped lists ([#83](https://github.com/talaagent/cargo-faster/pull/83))
- Detect a first build per mode and profile, and don't compare test builds with a check ([#82](https://github.com/talaagent/cargo-faster/pull/82))
- Don't compare build and run time for a filtered tests --run ([#80](https://github.com/talaagent/cargo-faster/pull/80))
- Package the test fixtures so cargo test works from the published crates ([#79](https://github.com/talaagent/cargo-faster/pull/79))
- Detect a shared jobserver and token-starved builds ([#77](https://github.com/talaagent/cargo-faster/pull/77))
- Don't pass --workspace twice when the cargo arguments already pick the packages ([#76](https://github.com/talaagent/cargo-faster/pull/76))
- Rank the cheap-edge table by rebuilds saved and cap re-export lists ([#68](https://github.com/talaagent/cargo-faster/pull/68))
- Open-source setup: badges, CI, release, metadata ([#64](https://github.com/talaagent/cargo-faster/pull/64))
- name the cargo command that started the kache session ([#63](https://github.com/talaagent/cargo-faster/pull/63))
- Use neutral crate names in fixtures and docs, and leave fixtures out of the published crates ([#61](https://github.com/talaagent/cargo-faster/pull/61))
- Rank dependency edges by rebuilds saved, report unused and re-exported dependencies, and recognize first builds ([#60](https://github.com/talaagent/cargo-faster/pull/60))
- tests --run: split run-only args, size the runner, flag test binaries without tests ([#58](https://github.com/talaagent/cargo-faster/pull/58))
- Split the --timings reader into cargo-timings-parse ([#59](https://github.com/talaagent/cargo-faster/pull/59))
- Flag dev-dependency feature splits; name busy-machine CPU users; fall back to the latest kache session with compiles ([#57](https://github.com/talaagent/cargo-faster/pull/57))
- Split self-profile analysis into the rustc-self-profile crate ([#48](https://github.com/talaagent/cargo-faster/pull/48))
- Show a busy machine as a condition above the summary, not a finding ([#47](https://github.com/talaagent/cargo-faster/pull/47))
- Print 0 instead of -0, "other" for (), and "depends" for one crate ([#46](https://github.com/talaagent/cargo-faster/pull/46))
- Measure the machine on macOS and name the filesystem behind df's '-' ([#45](https://github.com/talaagent/cargo-faster/pull/45))
- Split nextest libtest-json parsing into the nextest-libtest-json crate ([#44](https://github.com/talaagent/cargo-faster/pull/44))
- Read sccache's hit rate, uncached reasons and cache size from sccache --show-stats ([#43](https://github.com/talaagent/cargo-faster/pull/43))
- Say how to install cargo-nextest when tests --run needs it, and report a run with no tests as 0 tests ([#42](https://github.com/talaagent/cargo-faster/pull/42))
- Exit 3 when the timed build fails, give dependencies advice that works, and show the age of a reused timings report ([#41](https://github.com/talaagent/cargo-faster/pull/41))
- Add schema_version and cargo_faster_version to the JSON output, and pin guide links to the release tag ([#40](https://github.com/talaagent/cargo-faster/pull/40))
- Exit 2 on usage errors, add --fail-on, and rename check-cost --profile to --profdata ([#39](https://github.com/talaagent/cargo-faster/pull/39))
- rdeps and timings find dependency edges that are cheap to cut: few items used from a big crate, with the rebuilds the cut saves ([#38](https://github.com/talaagent/cargo-faster/pull/38))
- timings --cmd build|clippy|test times other builds, and --edit CRATE times the rebuild after an edit to one crate ([#36](https://github.com/talaagent/cargo-faster/pull/36))
- Say which kache session the cache numbers come from, and when the latest builds in it were incremental passthroughs with no cache lookups ([#35](https://github.com/talaagent/cargo-faster/pull/35))
- Give timings --json structured data: critical path steps and slowest units with crate, target, mode, start_s and took_s, plus wall_s, units_compiled and units_fresh ([#34](https://github.com/talaagent/cargo-faster/pull/34))
- tests --run: time the test build and the run, and report the slowest tests and binaries ([#33](https://github.com/talaagent/cargo-faster/pull/33))
- Explain where a timings build's time went: kache outcomes and waits, slow links, contention, rebuild fan-out ([#32](https://github.com/talaagent/cargo-faster/pull/32))
- pick the target with --lib/--bin/--test/--example/--bench and --tests, and show which build was profiled ([#31](https://github.com/talaagent/cargo-faster/pull/31))
- read CARGO_TARGET_<TRIPLE>_RUSTFLAGS, _LINKER and CARGO_BUILD_INCREMENTAL, in cargo's precedence order ([#30](https://github.com/talaagent/cargo-faster/pull/30))
- attribute body time to items and modules, and say when a crate is slow because it is big ([#29](https://github.com/talaagent/cargo-faster/pull/29))
- Flag test-only features enabled from [dependencies] and [workspace.dependencies] ([#27](https://github.com/talaagent/cargo-faster/pull/27))
- credit scripts that pass --test-threads and the database setup a workspace already has ([#28](https://github.com/talaagent/cargo-faster/pull/28))
- Leave profiler overhead out of check-cost's query table and shares ([#26](https://github.com/talaagent/cargo-faster/pull/26))
- Grade a target runner that steps aside under nextest as info, and report runners set by scripts ([#25](https://github.com/talaagent/cargo-faster/pull/25))
- List duplicate crate versions in semver order, pre-releases before releases ([#24](https://github.com/talaagent/cargo-faster/pull/24))
- Count #[async_trait] uses, size the finding by measured cost, and correct the guide ([#23](https://github.com/talaagent/cargo-faster/pull/23))
- Use bpaf-kit 0.2.1 instead of local workarounds ([#5](https://github.com/talaagent/cargo-faster/pull/5))
- Derive the CLI with bpaf and use bpaf-kit throughout ([#4](https://github.com/talaagent/cargo-faster/pull/4))
- Add Using cargo faster, the README and the debug-slow-rust-builds skill ([#3](https://github.com/talaagent/cargo-faster/pull/3))
- Add the checks, check-cost, timings and the CLI ([#2](https://github.com/talaagent/cargo-faster/pull/2))
- Bootstrap the cargo-faster workspace
- Group generic parameters and location-less async blocks in check-cost trait solving ([#81](https://github.com/talaagent/cargo-faster/pull/81))
- Read cargo 1.88 timings reports with an empty one-line UNIT_DATA ([#66](https://github.com/talaagent/cargo-faster/pull/66))

### Added

- A configuration file: `cargo-faster.toml` at the workspace root, or `[workspace.metadata.cargo-faster]`. Unknown keys are errors. It holds `hot` crates, `[[accept]]` entries for findings that are there by design (matched by id, crate and target, with a required reason, and still counted in the summary), `[budgets]`, `[[layer]]` rules, `[deps]` and `[env]`. A JSON schema is in `docs/cargo-faster.schema.json`.
- `cargo faster check`: layering rules, flagged crates, and a duplicate-version budget, resolved with guppy over every platform with real feature resolution, without building. `--timings` adds the cold-check, critical-path and per-hot-crate edit budgets. It exits 1 when a rule is broken, and `--format github` writes workflow annotations.
- `timings --hot` measures an edit to each hot crate. `rdeps` lists the hot crates first.
- `[env]` sets the profile, features and nextest profile that builds are measured with. `jobserver = "shared"` reports a token-starved build as expected.

