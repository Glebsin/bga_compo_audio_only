//! Parses argv into the root command and its trailing arguments.

use std::ffi::OsString;

/// The command `units::dispatch` gets when argv names no subcommand.
pub const ROOT: &str = "BGA_compo_audio_only";

pub struct Matches {
    pub args: Vec<OsString>,
}

pub fn parse(argv: &[OsString]) -> Matches {
    Matches {
        args: argv.iter().skip(1).cloned().collect(),
    }
}
