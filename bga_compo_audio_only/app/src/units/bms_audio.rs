//! Glue from the front door to `bms-audio`: run the renderer, return its exit code.

use std::process::ExitCode;

use crate::cli;

pub fn run(_command: &str, matches: &cli::Matches) -> ExitCode {
    ExitCode::from(bms_audio::renderer::run(&matches.args))
}
