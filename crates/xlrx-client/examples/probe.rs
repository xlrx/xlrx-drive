//! Checks whether a folder can become a sync folder and prints what its volume does with names
//! and timestamps (ADR 0002 §7.2). Exit status 1 if the folder is refused.
//!
//! `cargo run -p xlrx-client --example probe -- <ordner>`

use std::path::PathBuf;
use std::process::ExitCode;

use xlrx_client::local::probe::probe;

fn main() -> ExitCode {
    let Some(root) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("Aufruf: probe <ordner>");
        return ExitCode::from(2);
    };
    match probe(&root) {
        Ok(caps) => {
            println!("{caps:?}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("abgelehnt: {e}");
            ExitCode::FAILURE
        }
    }
}
