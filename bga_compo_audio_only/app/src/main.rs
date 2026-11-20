mod cli;
mod units;

use std::ffi::OsString;
use std::process::ExitCode;

// Faster global allocator; program output is unaffected.
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let matches = cli::parse(&argv);
    let code = units::dispatch(cli::ROOT, &matches).unwrap_or_else(|| {
        eprintln!("{}: no handler for `{}`", cli::ROOT, cli::ROOT);
        ExitCode::from(2)
    });
    pause_on_fresh_console();
    code
}

/// Drag-and-drop launches the app in a console window that Windows closes the
/// moment the process exits, taking the render log with it. When — and only
/// when — the console was created for this very run (no other process is
/// attached to it), wait for Enter before leaving. Runs from an open terminal
/// or with piped stdio keep the previous non-interactive behavior.
#[cfg(windows)]
fn pause_on_fresh_console() {
    use std::io::Write;

    #[link(name = "kernel32")]
    // SAFETY: kernel32 is loaded into every Windows process; both functions are
    // documented console queries whose failures need no handling here.
    unsafe extern "system" {
        fn GetConsoleWindow() -> *mut core::ffi::c_void;
        fn GetConsoleProcessList(lpdwprocesslist: *mut u32, dwprocesscount: u32) -> u32;
    }
    // SAFETY: neither call keeps pointers beyond the call itself.
    unsafe {
        if GetConsoleWindow().is_null() {
            return; // no console attached at all
        }
        let mut procs = [0u32; 8];
        // Exactly one attached process -> the console was made for this run
        // (Explorer drag-and-drop / double-click); a shared terminal reports > 1.
        if GetConsoleProcessList(procs.as_mut_ptr(), 8) != 1 {
            return;
        }
    }
    let _ = std::io::stdout().flush();
    // The stderr log (Created: ... / Cannot ...) is CRLF; match it.
    let _ = std::io::stderr().write_all(b"Press Enter to close.\r\n");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

#[cfg(not(windows))]
fn pause_on_fresh_console() {}
