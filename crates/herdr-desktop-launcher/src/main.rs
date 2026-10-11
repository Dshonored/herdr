//! `herdr-desktop.exe`: what the Start Menu and desktop shortcuts open on Windows.
//!
//! `herdr.exe` is a console program, so opening it directly shows a console running the
//! terminal client. This GUI-subsystem launcher starts `herdr.exe gui` beside it with a
//! hidden console instead; programs the app starts (the server, ssh, curl) inherit that
//! hidden console rather than each opening a window.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(launcher) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(launcher.with_file_name("herdr.exe"))
        .arg("gui")
        .args(std::env::args_os().skip(1))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("herdr-desktop is the Windows launcher; run `herdr gui` instead");
}
