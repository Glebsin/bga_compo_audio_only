//! Command dispatch.

use std::process::ExitCode;

use crate::cli;

mod bms_audio;

pub fn dispatch(command: &str, matches: &cli::Matches) -> Option<ExitCode> {
    match command {
        "BGA_compo_audio_only" => Some(bms_audio::run(command, matches)),
        _ => None,
    }
}
