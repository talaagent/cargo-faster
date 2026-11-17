# Contributing

Bug reports, new checks, documentation fixes, test cases and pull requests
are welcome. For a bug, include `cargo faster --version`, `rustc -vV`, your
OS, the command you ran and its `--json` output if you can. Open an issue
first for anything large. Report security issues privately (see
[SECURITY.md](SECURITY.md)).

A new check is most useful with the numbers that motivated it: what was
slow, how you found it, and the time before and after the fix. Add the
explanation to `docs/guide.md` and link the finding to it.

Keep pull requests small and focused, with a commit subject that reads well
as a changelog line (release-plz builds CHANGELOG.md from them). Coding
conventions and the repository layout are in [AGENTS.md](AGENTS.md).

## Checks

The minimum Rust is 1.88. The Nix dev shell pins the toolchain CI uses and
provides cargo-nextest and cargo-deny; with rustup, `rust-toolchain.toml`
picks the same toolchain.

```sh
nix develop -c cargo fmt --all --check
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
CARGO_FASTER_PROFILE_TESTS=1 nix develop -c cargo nextest run --workspace
RUSTDOCFLAGS="-D warnings" nix develop -c cargo doc --workspace --no-deps
nix develop -c ./scripts/no-std-check.sh
nix develop -c cargo deny check
```

CI also builds and tests on the MSRV, runs `cargo publish --dry-run`, and
checks the Nix flake (including `nix build .#cargo-faster`).

## Releases

`release-plz.toml` writes `CHANGELOG.md` and owns the `v<version>` tag and
GitHub release. The parser crates in `crates/` share cargo-faster's version
and are published with it; their commits go in the same changelog, and
they get no tag of their own. A new parser crate needs a `[[package]]`
entry there and its name in cargo-faster's `changelog_include`. With `release_always = false`, only a merged release PR
publishes. Review the version bump and the changelog, and merge only after
CI passes.

The workflow runs only while the `talaagent/cargo-faster` repository is
public.

Release PRs use the built-in `GITHUB_TOKEN` by default, with job-level
`contents: write` and `pull-requests: write` permissions. Enable **Allow
GitHub Actions to create and approve pull requests** in the organization and
repository Actions settings, then set the repository variable
`RELEASE_PR=true`. The workflow dispatches CI for those PRs itself (with
`actions: write`), because events from `GITHUB_TOKEN` don't trigger
workflows. An optional `RELEASE_PLZ_TOKEN` secret (GitHub App token or PAT)
replaces the built-in token.

### First publication

Trusted publishing can't create a crate, so the first release of each
crate (cargo-faster, and each parser crate) needs a token:

1. Create a short-lived crates.io token with the `publish-new` scope and
   store it as the `CARGO_REGISTRY_TOKEN` Actions secret.
2. Add the release date to the `0.1.0` heading in CHANGELOG.md.
3. Run the release-plz workflow by hand with `bootstrap=true`. It publishes
   the committed version.
4. On crates.io, add a trusted publisher to each new crate: owner `talaagent`,
   repository `cargo-faster`, workflow `release-plz.yml`, no environment.
5. Delete the `CARGO_REGISTRY_TOKEN` secret and revoke the token. Later
   releases use OIDC. A persistent token would override it.

Afterwards, check the crate's page on crates.io.

## License

Unless you explicitly state otherwise, any contribution you intentionally
submit for inclusion in this project, as defined in the Apache-2.0
license, is licensed under Apache-2.0, without any additional terms or
conditions.
