//! Glue from the front door to `bms-audio`: parse the arguments, call `bms_audio::port`,
//! print what the original prints and return its exit code. Every case is answered in Rust.

use std::process::ExitCode;

use crate::cli;

pub fn run(_command: &str, matches: &cli::Matches) -> ExitCode {
    ExitCode::from(bms_audio::port::run(&matches.args))
}
