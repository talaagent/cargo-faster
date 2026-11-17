#!/usr/bin/env bash
# Check that the parser crates build without std: for a target with no std
# (wasm32v1-none) and with their default `std` feature off, and that clippy
# passes on them with and without it. Needs the wasm32v1-none target (the
# Nix dev shell has it; with rustup, `rustup target add wasm32v1-none`).
set -euo pipefail

crates=(nextest-libtest-json rustc-self-profile cargo-timings-parse)

for c in "${crates[@]}"; do
  cargo build --locked -p "$c" --no-default-features --target wasm32v1-none
  cargo clippy --locked -p "$c" --all-targets --no-default-features -- -D warnings
  cargo clippy --locked -p "$c" --all-targets --all-features -- -D warnings
done
