//! `lsw-agent`: the LSW guest agent. Runs in the logged-in Windows session
//! (started at logon by a scheduled task with highest privileges) and talks
//! to the host over two virtio-serial ports:
//!
//! * `org.lsw.control.0` – the VM supervisor: handshake, shared folders,
//!   shutdown.
//! * `org.lsw.session.0` – the LSW UI: clipboard, display size.
//!
//! The protocol logic lives in [`core`] (platform independent, unit tested);
//! [`win`] is the thin Win32 layer.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod coherence;
#[cfg_attr(not(windows), allow(dead_code))]
mod core;
#[cfg_attr(not(windows), allow(dead_code))]
mod formats;
#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    std::process::exit(win::run());
    #[cfg(not(windows))]
    {
        eprintln!("lsw-agent runs inside the Windows guest");
        std::process::exit(2);
    }
}
