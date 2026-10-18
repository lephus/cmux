//! ShellX W0 Task 7 spike: a native Windows terminal window.
//!
//! GPUI (crates.io `gpui = "=0.2.2"`, Direct3D 11 + DirectWrite on Windows)
//! draws a libghostty-vt grid fed by ConPTY running PowerShell. Keyboard
//! text and IME composition come through GPUI's `EntityInputHandler`, so a
//! Vietnamese Telex preedit is drawn inline and only committed text reaches
//! the PTY. `--smoke` runs the same session headless and exits.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod session;
mod smoke;

#[cfg(windows)]
mod app;

fn main() {
    if std::env::args().skip(1).any(|arg| arg == "--smoke") {
        match smoke::run() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("smoke: FAIL: {error:#}");
                std::process::exit(1);
            }
        }
    }

    #[cfg(windows)]
    app::run();

    #[cfg(not(windows))]
    {
        eprintln!("the window is Windows-only; use --smoke to exercise the session");
        std::process::exit(2);
    }
}
