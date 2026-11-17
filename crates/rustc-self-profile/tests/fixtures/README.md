# Fixtures

`cargo-faster/nextest-libtest-json-check.json` is a real self-profile,
captured on 2026-11-17 with cargo 1.99.0 and rustc 1.99.1 in a copy of
the cargo-faster repository:

```sh
RUSTC_WRAPPER= CARGO_INCREMENTAL=0 RUSTC_BOOTSTRAP=1 \
  cargo rustc -p nextest-libtest-json --lib --profile check -- \
  -Zself-profile=prof -Zself-profile-events=default,args
```

It was loaded with `Profile::load` and saved as `Profile` JSON (the
`analyze --profile` example), then trimmed from 6 MB: it keeps intervals
of at least 50 µs, plus body, obligation, proc-macro and profiler
intervals of at least 10 µs (1098 of them), and the absolute paths in
`cmd` were made relative. One interval per line.
