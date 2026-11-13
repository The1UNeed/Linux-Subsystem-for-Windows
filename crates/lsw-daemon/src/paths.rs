//! Filesystem layout.
//!
//! * Persistent data: `$XDG_DATA_HOME/lsw/vms/<name>/` (config, disk, NVRAM,
//!   TPM state, logs).
//! * QEMU runtime files: `$XDG_RUNTIME_DIR/lsw/<name>/` (QMP sockets, guest
//!   agent channel, pid).
//! * Helper sockets: `$HOME/.cache/lsw/sock/<name>/` (swtpm, passt, virtiofsd). Ubuntu
//!   confines `/usr/bin/swtpm` and `/usr/bin/passt` with AppArmor profiles
//!   that may only create sockets under `/tmp` or `@{HOME}`; `/run/user` is
//!   denied. The directory is derived from `$HOME` itself, not an XDG
//!   variable, because the profiles match `@{HOME}`.
//!
//! Unix socket paths are limited to 107 bytes; [`VmPaths::create`] checks.

use std::path::{Path, PathBuf};

/// Maximum usable length of a Unix socket path on Linux.
pub const MAX_SOCKET_PATH: usize = 107;

/// Directories for one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmPaths {
    pub data: PathBuf,
    pub run: PathBuf,
    /// Socket directory for the AppArmor-confined helpers (swtpm, passt).
    pub helper_run: PathBuf,
}

impl VmPaths {
    /// Resolve from the environment.
    pub fn resolve(name: &str) -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute());
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")))
            .ok_or_else(|| anyhow::anyhow!("neither XDG_DATA_HOME nor HOME is set"))?;
        let run_base = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => std::env::temp_dir().join(format!("lsw-{}", user_id())),
        };
        let helper_base = match &home {
            Some(h) => h.join(".cache/lsw/sock"),
            None => run_base.join("lsw/sock"),
        };
        Ok(Self::with_bases(&data_home, &run_base, &helper_base, name))
    }

    pub fn with_bases(data_home: &Path, run_base: &Path, helper_base: &Path, name: &str) -> Self {
        Self {
            data: data_home.join("lsw/vms").join(name),
            run: run_base.join("lsw").join(name),
            helper_run: helper_base.join(name),
        }
    }

    pub fn qmp_socket(&self) -> PathBuf {
        self.run.join("qmp.sock")
    }
    /// Secondary QMP monitor for `lsw ctl`.
    pub fn ctl_socket(&self) -> PathBuf {
        self.run.join("ctl.sock")
    }
    pub fn tpm_socket(&self) -> PathBuf {
        self.helper_run.join("tpm.sock")
    }
    pub fn passt_socket(&self) -> PathBuf {
        self.helper_run.join("passt.sock")
    }
    /// vhost-user socket of the virtiofsd serving the `index`-th share
    /// (named by index, not tag, to stay within the socket path limit).
    pub fn virtiofs_socket(&self, index: usize) -> PathBuf {
        self.helper_run.join(format!("fs{index}.sock"))
    }
    /// virtio-serial control channel to the guest agent (QEMU listens).
    pub fn agent_socket(&self) -> PathBuf {
        self.run.join("agent.sock")
    }
    /// virtio-serial session channel to the guest agent, used by the UI.
    pub fn session_socket(&self) -> PathBuf {
        self.run.join("session.sock")
    }
    /// virtio-serial Coherence channel (window list and frames).
    pub fn coherence_socket(&self) -> PathBuf {
        self.run.join("coherence.sock")
    }
    /// QMP monitor reserved for the UI (display connection, input).
    pub fn ui_qmp_socket(&self) -> PathBuf {
        self.run.join("ui.sock")
    }
    pub fn qemu_pidfile(&self) -> PathBuf {
        self.run.join("qemu.pid")
    }
    pub fn serial_log(&self) -> PathBuf {
        self.data.join("serial.log")
    }
    pub fn qemu_log(&self) -> PathBuf {
        self.data.join("qemu.log")
    }
    /// Preferred display size (`{"width":…,"height":…}`), written by the
    /// app from the monitor size; used as virtio-gpu's preferred mode.
    pub fn display_size_file(&self) -> PathBuf {
        self.data.join("display.json")
    }
    /// The LSW Tools CD, rebuilt at every start.
    pub fn tools_iso(&self) -> PathBuf {
        self.data.join("tools.iso")
    }
    pub fn state_file(&self) -> PathBuf {
        self.data.join("saved-state.bin")
    }

    /// Create the directories with owner-only permissions.
    pub fn create(&self) -> anyhow::Result<()> {
        for dir in [&self.data, &self.run, &self.helper_run] {
            std::fs::create_dir_all(dir)?;
            restrict(dir)?;
        }
        for sock in [
            self.qmp_socket(),
            self.ctl_socket(),
            self.agent_socket(),
            self.session_socket(),
            self.coherence_socket(),
            self.ui_qmp_socket(),
            self.tpm_socket(),
            self.passt_socket(),
            self.virtiofs_socket(lsw_proto::MAX_LIST_LEN),
        ] {
            anyhow::ensure!(
                sock.as_os_str().len() <= MAX_SOCKET_PATH,
                "socket path too long: {}",
                sock.display()
            );
        }
        Ok(())
    }
}

#[cfg(unix)]
fn restrict(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Owner uid of the home directory: a dependency-free stand-in for
/// `getuid()` (the workspace forbids `unsafe`), used only to name the
/// fallback runtime directory when `XDG_RUNTIME_DIR` is unset.
#[cfg(unix)]
fn user_id() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::env::var_os("HOME")
        .and_then(|h| std::fs::metadata(h).ok())
        .map(|m| m.uid())
        .unwrap_or(0)
}

#[cfg(not(unix))]
fn user_id() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let p = VmPaths::with_bases(
            &tmp.path().join("data"),
            &tmp.path().join("run"),
            &tmp.path().join("home/.cache/lsw/sock"),
            "win",
        );
        p.create().unwrap();
        assert!(p.data.ends_with("lsw/vms/win"));
        assert!(p.qmp_socket().starts_with(&p.run));
        assert!(p.tpm_socket().starts_with(tmp.path().join("home")));
        assert!(p.passt_socket().starts_with(tmp.path().join("home")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&p.data, &p.run, &p.helper_run] {
                let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700);
            }
        }
    }

    #[test]
    fn rejects_overlong_socket_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("x".repeat(120));
        let p = VmPaths::with_bases(
            &tmp.path().join("data"),
            &deep,
            &tmp.path().join("h"),
            "win",
        );
        assert!(p.create().is_err());
    }
}
