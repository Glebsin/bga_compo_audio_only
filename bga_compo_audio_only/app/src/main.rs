mod cli;
mod units;

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let matches = cli::parse(&argv);
    units::dispatch(cli::ROOT, &matches).unwrap_or_else(|| {
        eprintln!("{}: no handler for `{}`", cli::ROOT, cli::ROOT);
        ExitCode::from(2)
    })
}
