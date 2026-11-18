# rustc-self-profile

[![crates.io][version-badge]][crate] [![downloads][downloads-badge]][crate] [![docs.rs][docs-badge]][docs] [![lib.rs][librs-badge]][librs] [![License: Apache-2.0][license-badge]][license]

Analyze a rustc self-profile (`-Zself-profile`, the `.mm_profdata` format
from [measureme](https://github.com/rust-lang/measureme)): where the
compiler spent its time in one crate, which function bodies and modules
cost the most to type check and borrow check, which trait obligations it
proved over and over, which proc macros were slow, and how much of the
profile is the profiler's own work.

```rust
use rustc_self_profile::{Profile, analyze};

let mut p = Profile::default();
// label, first argument, start ns, end ns, thread
p.push("typeck", Some("app::server::handle"), 0, 3_000_000, 0);
p.push("typeck", Some("app::util::add"), 3_000_000, 4_000_000, 0);
let a = analyze(&p);
assert_eq!(a.items[0].name, "app::server::handle");
assert_eq!(a.modules[0].name, "app::server");
```

With the `std` feature, `Profile::load("target/prof/app-12345")` reads a
real profile. Write one with

```sh
RUSTC_BOOTSTRAP=1 cargo rustc --lib -- \
  -Zself-profile=target/prof -Zself-profile-events=default,args
```

`RUSTC_BOOTSTRAP=1` lets a stable compiler take `-Z` flags; set it on that
one invocation only. Without `args`, the per-body, per-module and
per-obligation numbers are empty; the query table still works.

`Profile` and `Analysis` are serde Serialize + Deserialize, so a profile
can be loaded once, stored as JSON and analyzed anywhere. The
`analyze` example prints either as JSON:

```sh
cargo run -p rustc-self-profile --example analyze -- target/prof/app-12345.mm_profdata
```

## `no_std`

The crate is `#![no_std]` and needs `alloc` (maps are `hashbrown`).
Everything except `Profile::load` works without std. `load` sits behind
the default `std` feature because it reads files through `analyzeme`,
which needs `std` (files, memory maps, threads). Build with
`default-features = false` for no_std targets and feed it a deserialized
or hand-built `Profile`.

## Fixtures

The tests analyze a real profile of `cargo check` on this repository's
`nextest-libtest-json` crate, taken with rustc 1.99.1 (cargo 1.99.0) and
trimmed; see `tests/fixtures/README.md`.

## License

Apache-2.0. Part of [cargo-faster](https://github.com/talaagent/cargo-faster).

[version-badge]: https://img.shields.io/crates/v/rustc-self-profile.svg
[downloads-badge]: https://img.shields.io/crates/d/rustc-self-profile.svg
[crate]: https://crates.io/crates/rustc-self-profile
[docs-badge]: https://docs.rs/rustc-self-profile/badge.svg
[docs]: https://docs.rs/rustc-self-profile
[librs-badge]: https://img.shields.io/badge/lib.rs-rustc--self--profile-purple.svg
[librs]: https://lib.rs/crates/rustc-self-profile
[license-badge]: https://img.shields.io/badge/license-Apache--2.0-blue.svg
[license]: https://github.com/talaagent/cargo-faster/blob/main/LICENSE
