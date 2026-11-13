//! Persistent VM configuration (`vm.json` in the VM directory).

use lsw_proto::KnownFolder;
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

/// Most shared folders per VM (each takes a fixed PCI slot).
pub const MAX_SHARES: usize = 14;

/// A Linux folder shared into Windows over virtiofs, backing one Windows
/// known folder.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Share {
    /// virtiofs tag, `[a-z0-9-]{1,36}` (checked by [`VmConfig::validate`]).
    pub tag: String,
    /// Absolute host directory exported to the guest.
    pub host_path: PathBuf,
    /// Windows known folder redirected to the share.
    pub folder: KnownFolder,
}

impl Share {
    /// The default pairing: Linux XDG user folders ↔ Windows known folders.
    /// Folders that do not exist on the host are skipped; the home directory
    /// itself is never shared.
    pub fn defaults(home: &Path, xdg: impl Fn(&str) -> Option<PathBuf>) -> Vec<Share> {
        [
            ("DESKTOP", "Desktop", KnownFolder::Desktop),
            ("DOCUMENTS", "Documents", KnownFolder::Documents),
            ("DOWNLOAD", "Downloads", KnownFolder::Downloads),
            ("PICTURES", "Pictures", KnownFolder::Pictures),
            ("MUSIC", "Music", KnownFolder::Music),
            ("VIDEOS", "Videos", KnownFolder::Videos),
        ]
        .into_iter()
        .filter_map(|(key, fallback, folder)| {
            let dir = xdg(key).unwrap_or_else(|| home.join(fallback));
            (dir.is_dir() && dir.is_absolute() && dir != home).then(|| Share {
                tag: format!("lsw-{}", fallback.to_ascii_lowercase()),
                host_path: dir,
                folder,
            })
        })
        .collect()
    }
}

/// Preferred display size, persisted by the app (see
/// [`crate::paths::VmPaths::display_size_file`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplaySize {
    pub width: u32,
    pub height: u32,
}

impl DisplaySize {
    /// Read the stored size; falls back to 1280×800 and rejects sizes
    /// virtio-gpu would not accept.
    pub fn load(path: &Path) -> (u32, u32) {
        let stored = std::fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<DisplaySize>(&raw).ok());
        match stored {
            Some(d) if (640..=16384).contains(&d.width) && (480..=16384).contains(&d.height) => {
                (d.width, d.height)
            }
            _ => (1280, 800),
        }
    }

    /// Store atomically.
    pub fn save(self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&self)?)?;
        std::fs::rename(tmp, path)
    }
}

/// Parse `user-dirs.dirs` (`XDG_DOCUMENTS_DIR="$HOME/Dokumente"`) into
/// `("DOCUMENTS", path)` pairs. Only `$HOME/…` and absolute values are
/// accepted, as the xdg-user-dirs format specifies.
pub fn parse_user_dirs(text: &str, home: &Path) -> Vec<(String, PathBuf)> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once('=')?;
            let name = key.strip_prefix("XDG_")?.strip_suffix("_DIR")?;
            let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
            let path = if let Some(rest) = value.strip_prefix("$HOME") {
                let rest = rest.trim_start_matches('/');
                if rest.is_empty() {
                    home.to_path_buf()
                } else {
                    home.join(rest)
                }
            } else if value.starts_with('/') {
                PathBuf::from(value)
            } else {
                return None;
            };
            Some((name.to_owned(), path))
        })
        .collect()
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
    /// Shared folders (virtiofs).
    #[serde(default)]
    pub shares: Vec<Share>,
    /// Give Windows a microphone (off by default, as in Parallels).
    #[serde(default)]
    pub microphone: bool,
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
        anyhow::ensure!(
            self.shares.len() <= MAX_SHARES,
            "at most {MAX_SHARES} shared folders"
        );
        let mut seen = std::collections::HashSet::new();
        for share in &self.shares {
            anyhow::ensure!(
                lsw_proto::is_valid_tag(&share.tag),
                "invalid share tag {:?}",
                share.tag
            );
            anyhow::ensure!(
                seen.insert(&share.tag),
                "duplicate share tag {:?}",
                share.tag
            );
            anyhow::ensure!(
                share.host_path.is_absolute(),
                "share path must be absolute: {}",
                share.host_path.display()
            );
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
            shares: vec![],
            microphone: false,
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
        let share = |tag: &str, path: &str| Share {
            tag: tag.into(),
            host_path: path.into(),
            folder: KnownFolder::Documents,
        };
        for bad in [
            vec![share("Bad Tag", "/home/u/Documents")],
            vec![share("lsw-docs", "relative/path")],
            vec![share("a", "/x"), share("a", "/y")],
        ] {
            let mut cfg = sample(dir.path());
            cfg.shares = bad;
            assert!(cfg.validate().is_err());
        }
    }

    #[test]
    fn default_shares_pair_existing_xdg_folders_only() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir(h.join("Documents")).unwrap();
        std::fs::create_dir(h.join("Bilder")).unwrap();
        // XDG_PICTURES_DIR points at a localized folder; Music is missing;
        // a folder configured as the home directory itself is never shared.
        let shares = Share::defaults(h, |key| match key {
            "PICTURES" => Some(h.join("Bilder")),
            "DESKTOP" => Some(h.to_path_buf()),
            _ => None,
        });
        let tags: Vec<_> = shares.iter().map(|s| s.tag.as_str()).collect();
        assert_eq!(tags, ["lsw-documents", "lsw-pictures"]);
        assert_eq!(shares[1].host_path, h.join("Bilder"));
        assert_eq!(shares[1].folder, KnownFolder::Pictures);
    }

    #[test]
    fn display_size_roundtrip_and_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("display.json");
        assert_eq!(DisplaySize::load(&f), (1280, 800));
        DisplaySize {
            width: 2560,
            height: 1440,
        }
        .save(&f)
        .unwrap();
        assert_eq!(DisplaySize::load(&f), (2560, 1440));
        std::fs::write(&f, br#"{"width":99999,"height":10}"#).unwrap();
        assert_eq!(DisplaySize::load(&f), (1280, 800));
    }

    #[test]
    fn user_dirs_file_is_parsed() {
        let text = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Schreibtisch\"\nXDG_DOWNLOAD_DIR=\"/data/dl\"\nXDG_MUSIC_DIR=\"$HOME/\"\nXDG_VIDEOS_DIR=relative\nXDG_BAD=\"/x\"\n";
        let home = Path::new("/home/u");
        assert_eq!(
            parse_user_dirs(text, home),
            vec![
                ("DESKTOP".to_owned(), PathBuf::from("/home/u/Schreibtisch")),
                ("DOWNLOAD".to_owned(), PathBuf::from("/data/dl")),
                ("MUSIC".to_owned(), PathBuf::from("/home/u")),
            ]
        );
    }

    #[test]
    fn profiles_cap_at_half_host() {
        assert_eq!(Profile::Productivity.capped(32 * 1024, 16), (8192, 4, 128));
        assert_eq!(Profile::Development.capped(16 * 1024, 8), (8192, 4, 256));
        // Never below the Windows 11 minimum, even on a tiny host.
        assert_eq!(Profile::Games.capped(6 * 1024, 2), (4096, 2, 256));
    }
}
