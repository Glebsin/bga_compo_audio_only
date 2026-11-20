//! Parses argv into a command and its arguments.
//!
//! Replaces the clap front door after the lift: the generated command was a single
//! root command with one trailing-var-arg OsString argument — no flags, no
//! subcommands, no validation, and a parse path that could not fail — so collecting
//! `argv[1..]` verbatim is exactly what clap did.

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
