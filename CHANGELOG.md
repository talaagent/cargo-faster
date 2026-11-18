# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A configuration file: `cargo-faster.toml` at the workspace root, or `[workspace.metadata.cargo-faster]`. Unknown keys are errors. It holds `hot` crates, `[[accept]]` entries for findings that are there by design (matched by id, crate and target, with a required reason, and still counted in the summary), `[budgets]`, `[[layer]]` rules, `[deps]` and `[env]`. A JSON schema is in `docs/cargo-faster.schema.json`.
- `cargo faster check`: layering rules, flagged crates, and a duplicate-version budget, resolved with guppy over every platform with real feature resolution, without building. `--timings` adds the cold-check, critical-path and per-hot-crate edit budgets. It exits 1 when a rule is broken, and `--format github` writes workflow annotations.
- `timings --hot` measures an edit to each hot crate. `rdeps` lists the hot crates first.
- `[env]` sets the profile, features and nextest profile that builds are measured with. `jobserver = "shared"` reports a token-starved build as expected.

## [0.1.0](https://github.com/talaagent/cargo-faster/releases/tag/v0.1.0)

First release.
