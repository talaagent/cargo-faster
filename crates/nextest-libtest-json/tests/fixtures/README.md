# Fixtures

`cargo-faster/` holds real output, captured on 2026-11-17 with
cargo-nextest 0.9.149, cargo 1.99.0, rustc 1.99.1 and
`NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1`:

- `nextest-libtest-json-plus.jsonl`: `cargo nextest run --workspace
  --message-format libtest-json-plus` in the cargo-faster repository
  (110 tests, all passing).
- `failing-retried.jsonl`: the same flags plus `--retries 1
  --no-fail-fast` on a scratch crate with one passing, one failing and
  one ignored test.
