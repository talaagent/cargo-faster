//! Print what a `cargo build --timings` report says, as JSON.
//!
//! ```text
//! cargo run -p cargo-timings-parse --example analyze -- target/cargo-timings/cargo-timing.html
//! ```
//!
//! With no argument, reads the newest report in `./target`.

use cargo_timings_parse::{Timings, analyze, newest_report};
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn core::error::Error>> {
    let path = match std::env::args_os().nth(1) {
        Some(p) => PathBuf::from(p),
        None => newest_report(Path::new("target")).ok_or("no report in target/cargo-timings")?,
    };
    let t = Timings::parse_html(&std::fs::read_to_string(&path)?)?;
    let a = analyze(&t);
    println!("{}", serde_json::to_string_pretty(&a)?);
    Ok(())
}
