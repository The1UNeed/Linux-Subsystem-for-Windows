//! Starting LSW programs in the background (the VM supervisor from a
//! launcher, one view from the other).

use std::path::PathBuf;

/// A program started by [`start`].
#[derive(Debug)]
pub enum Started {
    /// As this transient systemd user unit.
    Unit(String),
    /// As a child process (no systemd); `command` is how to rerun it.
    Child {
        child: std::process::Child,
        command: String,
    },
}

impl Started {
    /// Whether it is still running. A caller waiting for it (Windows
    /// booting) can stop waiting as soon as it has exited.
    pub fn running(&mut self) -> bool {
        match self {
            // `systemd-run` returns once the unit is active; with
            // `--collect` it is gone (inactive) as soon as it exits.
            Started::Unit(unit) => std::process::Command::new("systemctl")
                .args(["--user", "is-active", "--quiet", unit.as_str()])
                .stdin(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success()),
            Started::Child { child, .. } => matches!(child.try_wait(), Ok(None)),
        }
    }

    /// Where to look for why it stopped.
    pub fn log_hint(&self) -> String {
        match self {
            Started::Unit(unit) => format!("`journalctl --user -u {unit}`"),
            Started::Child { command, .. } => format!("run `{command}` in a terminal"),
        }
    }
}

/// Start an LSW program next to the current one, detached: as a transient
/// systemd user unit `unit` when available (it outlives the caller and logs
/// to the journal), otherwise as a plain child process.
pub fn start(program: &str, args: &[&str], unit: &str) -> std::io::Result<Started> {
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
        Ok(s) if s.success() => Ok(Started::Unit(unit.to_owned())),
        _ => std::process::Command::new(&exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|child| Started::Child {
                child,
                command: std::iter::once(program)
                    .chain(args.iter().copied())
                    .collect::<Vec<_>>()
                    .join(" "),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_that_exited_is_not_running() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let mut s = Started::Child {
            child,
            command: "lsw --name windows run".into(),
        };
        for _ in 0..200 {
            if !s.running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!s.running());
        assert_eq!(s.log_hint(), "run `lsw --name windows run` in a terminal");
    }

    #[test]
    fn a_running_child_is_running() {
        let child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let mut s = Started::Child {
            child,
            command: "sleep 5".into(),
        };
        assert!(s.running());
        if let Started::Child { child, .. } = &mut s {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}
