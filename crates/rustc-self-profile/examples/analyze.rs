//! Print a self-profile's analysis as JSON:
//!
//! ```text
//! cargo run --example analyze -- target/prof/app-12345.mm_profdata
//! cargo run --example analyze -- --profile target/prof/app-12345.mm_profdata
//! ```
//!
//! `--profile` prints the loaded [`Profile`] itself (every interval), the
//! form the tests' fixtures are stored in.

use rustc_self_profile::{Profile, analyze};
use std::path::Path;

fn main() -> Result<(), Box<dyn core::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let raw = args.first().is_some_and(|a| a == "--profile");
    if raw {
        args.remove(0);
    }
    let path = args
        .first()
        .ok_or("usage: analyze [--profile] <file.mm_profdata>")?;
    let path = Path::new(path);
    let stem = if path.extension().is_some_and(|x| x == "mm_profdata") {
        path.with_extension("")
    } else {
        path.to_path_buf()
    };
    let p = Profile::load(&stem)?;
    let json = if raw {
        serde_json::to_string(&p)?
    } else {
        serde_json::to_string_pretty(&analyze(&p))?
    };
    println!("{json}");
    Ok(())
}
