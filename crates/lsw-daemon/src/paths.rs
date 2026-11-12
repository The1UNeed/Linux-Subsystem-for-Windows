//! Filesystem layout.
//!
//! * Persistent data: `$XDG_DATA_HOME/lsw/vms/<name>/` (config, disk, NVRAM,
//!   TPM state, logs).
//! * Runtime sockets: `$XDG_RUNTIME_DIR/lsw/<name>/` (QMP, swtpm, passt).
//!   Unix socket paths are limited to 107 bytes, so they never live under a
//!   potentially deep data directory.

use std::path::{Path, PathBuf};

/// Maximum usable length of a Unix socket path on Linux.
pub const MAX_SOCKET_PATH: usize = 107;

/// Directories for one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmPaths {
    pub data: PathBuf,
    pub run: PathBuf,
}

impl VmPaths {
    /// Resolve from the environment.
    pub fn resolve(name: &str) -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")))
            .ok_or_else(|| anyhow::anyhow!("neither XDG_DATA_HOME nor HOME is set"))?;
        let run_base = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => std::env::temp_dir().join(format!("lsw-{}", user_id())),
        };
        Ok(Self::with_bases(&data_home, &run_base, name))
    }

    pub fn with_bases(data_home: &Path, run_base: &Path, name: &str) -> Self {
        Self {
            data: data_home.join("lsw/vms").join(name),
            run: run_base.join("lsw").join(name),
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
        self.run.join("tpm.sock")
    }
    pub fn passt_socket(&self) -> PathBuf {
        self.run.join("passt.sock")
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
    pub fn state_file(&self) -> PathBuf {
        self.data.join("saved-state.bin")
    }

    /// Create both directories with owner-only permissions.
    pub fn create(&self) -> anyhow::Result<()> {
        for dir in [&self.data, &self.run] {
            std::fs::create_dir_all(dir)?;
            restrict(dir)?;
        }
        for sock in [
            self.qmp_socket(),
            self.ctl_socket(),
            self.tpm_socket(),
            self.passt_socket(),
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
        let p = VmPaths::with_bases(&tmp.path().join("data"), &tmp.path().join("run"), "win");
        p.create().unwrap();
        assert!(p.data.ends_with("lsw/vms/win"));
        assert!(p.qmp_socket().starts_with(&p.run));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p.run).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn rejects_overlong_socket_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("x".repeat(120));
        let p = VmPaths::with_bases(&tmp.path().join("data"), &deep, "win");
        assert!(p.create().is_err());
    }
}
