//! Persistent VM configuration (`vm.json` in the VM directory).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Resource profiles offered by the setup wizard.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    Productivity,
    Development,
    Games,
}

impl Profile {
    /// `(memory MiB, vCPUs, disk GiB)` before host capping.
    pub fn resources(self) -> (u32, u16, u64) {
        match self {
            Profile::Productivity => (8 * 1024, 4, 128),
            Profile::Development | Profile::Games => (16 * 1024, 8, 256),
        }
    }

    /// Resources capped at half of the host, never below Windows 11's
    /// minimum of 4 GiB and 2 vCPUs.
    pub fn capped(self, host_mem_mib: u64, host_cpus: u16) -> (u32, u16, u64) {
        let (mem, cpus, disk) = self.resources();
        let mem_cap = u32::try_from(host_mem_mib / 2).unwrap_or(u32::MAX);
        let mem = mem.min(mem_cap).max(4096);
        let cpus = cpus.min((host_cpus / 2).max(1)).max(2);
        (mem, cpus, disk)
    }
}

/// How the guest reaches the network.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Network {
    /// Unprivileged NAT through `passt` (default).
    Passt,
    /// QEMU's built-in user networking (fallback when passt is missing).
    User,
    /// No network device.
    None,
}

/// GPU tier selected for this VM (see docs/DESIGN.md §3).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum GpuTier {
    /// Paravirtualized 3D (virgl / Venus).
    A,
    /// Passthrough of a second GPU.
    B,
    /// Intel SR-IOV virtual function.
    C,
    /// 2D only.
    D,
}

/// UEFI firmware files for this VM.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Firmware {
    /// Read-only OVMF code image (secure-boot capable build).
    pub code: PathBuf,
    /// Per-VM writable copy of the NVRAM variables.
    pub vars: PathBuf,
    /// Whether the vars template had Microsoft keys enrolled.
    pub secure_boot: bool,
}

/// Media attached only while Windows is being installed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct InstallMedia {
    pub windows_iso: PathBuf,
    pub virtio_win_iso: PathBuf,
    /// CD image carrying `autounattend.xml` and LSW's first-boot scripts.
    pub setup_iso: PathBuf,
}

/// Persistent configuration of the single Windows VM.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    pub name: String,
    pub memory_mib: u32,
    pub vcpus: u16,
    pub disk_path: PathBuf,
    pub disk_size_gib: u64,
    pub firmware: Firmware,
    pub tpm_state_dir: PathBuf,
    pub network: Network,
    pub gpu_tier: GpuTier,
    /// vsock context id for the guest agent channel (≥ 3).
    pub vsock_cid: Option<u32>,
    /// Present until the unattended installation finished.
    pub install: Option<InstallMedia>,
}

impl VmConfig {
    pub const FILE_NAME: &'static str = "vm.json";

    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read(dir.join(Self::FILE_NAME))?;
        let cfg: Self = serde_json::from_slice(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Write atomically (temp file + rename) so a crash never leaves a
    /// truncated config behind.
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        self.validate()?;
        let tmp = dir.join(".vm.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, dir.join(Self::FILE_NAME))?;
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.name.is_empty()
                && self.name.len() <= 32
                && self
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "VM name must be 1-32 characters of [A-Za-z0-9_-]"
        );
        anyhow::ensure!(
            self.memory_mib >= 4096,
            "Windows 11 needs at least 4096 MiB"
        );
        anyhow::ensure!(self.vcpus >= 2, "Windows 11 needs at least 2 vCPUs");
        anyhow::ensure!(self.disk_size_gib >= 64, "Windows 11 needs at least 64 GiB");
        if let Some(cid) = self.vsock_cid {
            anyhow::ensure!(cid >= 3 && cid != u32::MAX, "vsock CID must be >= 3");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(dir: &Path) -> VmConfig {
        VmConfig {
            name: "windows".into(),
            memory_mib: 8192,
            vcpus: 4,
            disk_path: dir.join("disk.raw"),
            disk_size_gib: 128,
            firmware: Firmware {
                code: "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd".into(),
                vars: dir.join("OVMF_VARS.fd"),
                secure_boot: true,
            },
            tpm_state_dir: dir.join("tpm"),
            network: Network::Passt,
            gpu_tier: GpuTier::D,
            vsock_cid: Some(3),
            install: None,
        }
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = sample(dir.path());
        cfg.save(dir.path()).unwrap();
        assert_eq!(VmConfig::load(dir.path()).unwrap(), cfg);
    }

    #[test]
    fn rejects_bad_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = sample(dir.path());
        cfg.name = "../evil".into();
        assert!(cfg.validate().is_err());
        let mut cfg = sample(dir.path());
        cfg.memory_mib = 2048;
        assert!(cfg.validate().is_err());
        let mut cfg = sample(dir.path());
        cfg.vsock_cid = Some(2);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn profiles_cap_at_half_host() {
        assert_eq!(Profile::Productivity.capped(32 * 1024, 16), (8192, 4, 128));
        assert_eq!(Profile::Development.capped(16 * 1024, 8), (8192, 4, 256));
        // Never below the Windows 11 minimum, even on a tiny host.
        assert_eq!(Profile::Games.capped(6 * 1024, 2), (4096, 2, 256));
    }
}
