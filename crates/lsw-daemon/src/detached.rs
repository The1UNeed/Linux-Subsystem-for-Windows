//! Starting LSW programs in the background (the VM supervisor from a
//! launcher, one view from the other).

use std::path::PathBuf;

/// Start an LSW program next to the current one, detached: as a transient
/// systemd user unit `unit` when available (it outlives the caller and logs
/// to the journal), otherwise as a plain child process.
pub fn start(program: &str, args: &[&str], unit: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(program)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(program));
    let systemd = std::process::Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet", "--unit", unit])
        // The VM supervisor shuts Windows down on SIGTERM (up to 120 s);
        // only it gets the signal (QEMU would quit on it).
        .args([
            "--property",
            "TimeoutStopSec=150",
            "--property",
            "KillMode=mixed",
        ])
        .arg(&exe)
        .args(args)
        .status();
    match systemd {
        Ok(s) if s.success() => Ok(()),
        _ => std::process::Command::new(&exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ()),
    }
}
