# Fixtures

`cargo-faster/check.html` and `cargo-faster/build.html` are real
`--timings` reports, captured on 2026-11-17 with cargo 1.99.0 and rustc
1.99.1 (jobs=16 on 32 CPUs) in a copy of the cargo-faster repository with
the three crates it had then (cargo-faster, nextest-libtest-json,
rustc-self-profile), from an empty target directory, with no rustc
wrapper:

```sh
export CARGO_TARGET_DIR=/tmp/cf-fixtures/tt RUSTC_WRAPPER=
cargo check --workspace --all-targets --timings   # check.html
cargo build --workspace --timings                 # build.html
```

They are unchanged apart from their names. The build reuses the check's
proc macros and build scripts, so it compiles the libraries the check only
checked, and the binary.
