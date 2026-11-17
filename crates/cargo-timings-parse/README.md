# cargo-timings-parse

Parse the HTML reports `cargo build --timings` writes to
`target/cargo-timings/` into typed units, and analyze them: the critical
path (and whether each step waited for metadata or a full build), crates
that ran mostly alone, long code generation, check units with a long tail
after their metadata, linking, build scripts, time by kind of unit,
slowdowns against an earlier report, and the workspace crates a build
recompiled.

```rust
use cargo_timings_parse::{Kind, Timings, analyze};

let html = r#"<script>
const UNIT_DATA = [
  {"i": 0, "name": "core-lib", "version": "0.1.0", "mode": "todo", "target": "",
   "start": 0.0, "duration": 4.0, "rmeta_time": 1.5, "unblocked_units": [1]},
  {"i": 1, "name": "app-cli", "version": "0.1.0", "mode": "todo", "target": " app \"bin\"",
   "start": 4.1, "duration": 2.0, "unblocked_units": []}
];
</script>"#;
let t = Timings::parse_html(html).expect("a timings report");
assert_eq!(t.units[1].kind(), Kind::Link);

let a = analyze(&t);
assert!((a.wall - 6.1).abs() < 1e-9);
let path: Vec<&str> = a.critical_path.iter().map(|s| s.unit.as_str()).collect();
assert_eq!(path, ["core-lib", "app-cli app \"bin\""]);
```

`analyze_with` takes an `Extra`: an earlier report to compare unit times
with (a `cargo check` of the same edit compares each library up to its
metadata), and the workspace's crates and their dependencies, to count the
members a build recompiled and where the edit was. `critical_path` gives
each step's position in `Timings::units`, to join other per-unit data
(a cache wrapper's records, say) onto the path.

`Timings`, `Unit` and `Analysis` are serde Serialize + Deserialize; the
analysis serializes with seconds in `_s` fields. Reports from cargo 1.88
on (with `sections`) and older ones (with `rmeta_time` and
`unlocked_units`) both parse.

The `analyze` example prints the analysis of a report (by default the
newest in `./target`) as JSON:

```sh
cargo build --timings
cargo run -p cargo-timings-parse --example analyze -- target/cargo-timings/cargo-timing.html
```

## `no_std`

The crate is `#![no_std]` and needs `alloc` (maps are `BTreeMap`).
Parsing and analysis work without std. `reports`, `newest_report` and
`check_before`, which list and read reports in a target directory, need
the file system and sit behind the default `std` feature. Build with
`default-features = false` for no_std targets.

## Fixtures

The tests read two real reports from this repository: a cold
`cargo check --workspace --all-targets` and the `cargo build --workspace`
after it, made with cargo 1.99.0 and rustc 1.99.1; see
`tests/fixtures/README.md`.

## License

Apache-2.0. Part of [cargo-faster](https://github.com/talaagent/cargo-faster).
