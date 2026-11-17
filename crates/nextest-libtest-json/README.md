# nextest-libtest-json

Parse the output of `cargo nextest run --message-format libtest-json` (or
`libtest-json-plus`) into typed events and a run summary.

> The libtest-json output is **experimental** in nextest. It needs
> `NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1`, and its fields may change between
> nextest releases. Unknown fields are ignored and unknown event names
> parse as `Other`, so a newer nextest degrades rather than fails.

```rust
use nextest_libtest_json::Run;

let out = r#"{"type":"test","event":"ok","name":"app::app$tests::adds","exec_time":0.004}
{"type":"test","event":"failed","name":"app::it$slow#2","exec_time":1.5,"stdout":"boom"}"#;
let run = Run::parse(out);
assert_eq!(run.tests.len(), 2);
assert_eq!(run.failed().count(), 1);
```

- `parse` / `parse_line` give every `Event` (suite and test events, with
  the `nextest` binary info that `libtest-json-plus` adds).
- `TestName::parse` splits `crate::binary[@stress-N]$test[#attempts]`.
- `Run` collects finished tests, the ignored count and each binary's
  totals.

All types are `serde` Serialize + Deserialize.

## `no_std`

The crate is `#![no_std]` and needs `alloc`. The default `std` feature
adds `parse_reader` for `std::io::BufRead`; build with
`default-features = false` for no_std targets.

## Fixtures

The tests parse real output from cargo-nextest 0.9.149 (cargo 1.99.0, rustc 1.99.1);
see `tests/fixtures/README.md`.

## License

Apache-2.0. Part of [cargo-faster](https://github.com/talaagent/cargo-faster).
