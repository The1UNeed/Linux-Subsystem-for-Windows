//! Host capability probing: KVM, CPU, memory, IOMMU, GPUs and helper binaries.
//!
//! Everything is read relative to a `root` so tests can build a fake
//! `/proc` + `/sys` tree.

use crate::config::{GpuTier, Profile};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const VENDOR_AMD: u16 = 0x1002;
pub const VENDOR_INTEL: u16 = 0x8086;
pub const VENDOR_NVIDIA: u16 = 0x10de;

/// PCI class code prefix for display controllers.
const PCI_CLASS_DISPLAY: u32 = 0x03;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CpuVendor {
    Intel,
    Amd,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gpu {
    /// PCI address, e.g. `0000:03:00.0`.
    pub pci_addr: String,
    pub vendor: u16,
    pub device: u16,
    pub driver: Option<String>,
    /// Firmware console device; never passed through.
    pub boot_vga: bool,
    pub sriov_total_vfs: u32,
    pub iommu_group: Option<u32>,
    /// `renderD128` etc., when the host driver exposes one.
    pub render_node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct Binaries {
    pub qemu: Option<PathBuf>,
    pub swtpm: Option<PathBuf>,
    pub passt: Option<PathBuf>,
    pub virtiofsd: Option<PathBuf>,
    /// Needed by virtiofsd's unprivileged namespace sandbox.
    pub newuidmap: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostReport {
    pub kvm_present: bool,
    pub kvm_accessible: bool,
    pub cpu_vendor: CpuVendor,
    pub virt_extensions: bool,
    pub logical_cpus: u16,
    pub mem_total_mib: u64,
    pub iommu_groups: usize,
    /// A PipeWire server is reachable for the user (guest audio).
    pub pipewire: bool,
    /// QEMU has the D-Bus display module (needed by the LSW app).
    pub qemu_dbus_display: bool,
    pub gpus: Vec<Gpu>,
    /// Usable GPU tiers, in [`GpuTier`] order (A first, D last; D is always present).
    pub tiers: Vec<GpuTier>,
    pub binaries: Binaries,
}

/// A blocking problem found by [`HostReport::problems`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub what: String,
    pub fix: String,
}

impl HostReport {
    /// Probe the real host.
    pub fn probe() -> Self {
        let mut report = Self::probe_root(Path::new("/"), &search_path());
        report.pipewire = pipewire_socket().is_some_and(|s| s.exists());
        report.qemu_dbus_display = report
            .binaries
            .qemu
            .as_deref()
            .is_some_and(qemu_has_dbus_display);
        report
    }

    /// Probe a (possibly fake) root. `path` is the binary search path.
    pub fn probe_root(root: &Path, path: &[PathBuf]) -> Self {
        let cpuinfo = std::fs::read_to_string(root.join("proc/cpuinfo")).unwrap_or_default();
        let (cpu_vendor, virt_extensions, logical_cpus) = parse_cpuinfo(&cpuinfo);
        let meminfo = std::fs::read_to_string(root.join("proc/meminfo")).unwrap_or_default();
        let kvm = root.join("dev/kvm");
        let gpus = probe_gpus(root);
        let iommu_groups = std::fs::read_dir(root.join("sys/kernel/iommu_groups"))
            .map(|d| d.count())
            .unwrap_or(0);
        let mut report = Self {
            kvm_present: kvm.exists(),
            kvm_accessible: open_rw(&kvm),
            cpu_vendor,
            virt_extensions,
            logical_cpus,
            mem_total_mib: parse_meminfo_total_mib(&meminfo),
            iommu_groups,
            pipewire: false,
            qemu_dbus_display: false,
            gpus,
            tiers: Vec::new(),
            binaries: find_binaries(path),
        };
        report.tiers = report.usable_tiers();
        report
    }

    fn usable_tiers(&self) -> Vec<GpuTier> {
        let mut tiers = Vec::new();
        if self.gpus.iter().any(|g| g.render_node.is_some()) {
            tiers.push(GpuTier::A);
        }
        let iommu = self.iommu_groups > 0;
        if iommu && self.passthrough_candidate().is_some() {
            tiers.push(GpuTier::B);
        }
        if iommu
            && self
                .gpus
                .iter()
                .any(|g| g.vendor == VENDOR_INTEL && g.sriov_total_vfs > 0)
        {
            tiers.push(GpuTier::C);
        }
        tiers.push(GpuTier::D);
        tiers
    }

    /// A GPU that can be handed to Windows while the host keeps another one
    /// for its own display: never the boot VGA device, and only when at
    /// least one other GPU remains.
    pub fn passthrough_candidate(&self) -> Option<&Gpu> {
        if self.gpus.len() < 2 {
            return None;
        }
        self.gpus
            .iter()
            .filter(|g| !g.boot_vga && g.iommu_group.is_some())
            .max_by_key(|g| {
                (
                    g.vendor != VENDOR_INTEL,
                    g.vendor == VENDOR_NVIDIA || g.vendor == VENDOR_AMD,
                )
            })
    }

    /// Default tier for a profile: Games prefers passthrough, everything
    /// else prefers paravirtualized 3D, and 2D is the universal fallback.
    pub fn recommended_tier(&self, profile: Profile) -> GpuTier {
        let has = |t| self.tiers.contains(&t);
        match profile {
            Profile::Games if has(GpuTier::B) => GpuTier::B,
            _ if has(GpuTier::A) => GpuTier::A,
            _ if has(GpuTier::C) => GpuTier::C,
            _ => GpuTier::D,
        }
    }

    /// Problems that block creating or starting a VM, with fixes.
    pub fn problems(&self) -> Vec<Problem> {
        let mut out = Vec::new();
        let mut add = |what: &str, fix: &str| {
            out.push(Problem {
                what: what.into(),
                fix: fix.into(),
            })
        };
        if !self.virt_extensions {
            add(
                "CPU virtualization (VT-x / AMD-V) is not available",
                "Enable Intel VT-x or AMD SVM in the firmware setup",
            );
        }
        if !self.kvm_present {
            add(
                "/dev/kvm is missing",
                "Load the kvm_intel or kvm_amd kernel module",
            );
        } else if !self.kvm_accessible {
            add(
                "/dev/kvm is not accessible to this user",
                "Add yourself to the 'kvm' group and log in again",
            );
        }
        if self.mem_total_mib < 8 * 1024 {
            add(
                "Less than 8 GiB of RAM; Windows 11 needs 4 GiB for itself",
                "Use a machine with at least 16 GiB of RAM",
            );
        }
        if self.binaries.qemu.is_none() {
            add(
                "qemu-system-x86_64 not found",
                "Install the lsw package (it bundles QEMU) or your distribution's QEMU",
            );
        }
        if self.binaries.swtpm.is_none() {
            add(
                "swtpm not found (Windows 11 requires TPM 2.0)",
                "Install swtpm",
            );
        }
        out
    }

    /// Non-blocking issues: LSW runs, with a feature missing.
    pub fn warnings(&self) -> Vec<Problem> {
        let mut out = Vec::new();
        if self.binaries.virtiofsd.is_none() || self.binaries.newuidmap.is_none() {
            out.push(Problem {
                what: "virtiofsd or newuidmap not found: shared folders are off".into(),
                fix: "Install virtiofsd and uidmap (Ubuntu) or virtiofsd and shadow (Arch)".into(),
            });
        }
        if self.binaries.qemu.is_some() && !self.qemu_dbus_display {
            out.push(Problem {
                what: "QEMU lacks the D-Bus display: the LSW window cannot show Windows".into(),
                fix: "Install qemu-ui-dbus (Arch) or qemu-system-modules-opengl (Debian/Ubuntu)"
                    .into(),
            });
        }
        if self.binaries.passt.is_none() {
            out.push(Problem {
                what: "passt not found: using QEMU's slower built-in networking".into(),
                fix: "Install passt".into(),
            });
        }
        out
    }
}

/// Whether `qemu -display help` lists the D-Bus display (a loadable module
/// some distributions package separately, e.g. Arch's `qemu-ui-dbus`).
fn qemu_has_dbus_display(qemu: &Path) -> bool {
    std::process::Command::new(qemu)
        .args(["-display", "help"])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == "dbus")
        })
}

/// The user's PipeWire socket (`$PIPEWIRE_RUNTIME_DIR` or
/// `$XDG_RUNTIME_DIR`, then `pipewire-0`).
pub fn pipewire_socket() -> Option<PathBuf> {
    let dir =
        std::env::var_os("PIPEWIRE_RUNTIME_DIR").or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))?;
    let name = std::env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into());
    Some(PathBuf::from(dir).join(name))
}

/// Binary search path: LSW's bundled tools first, then `$PATH`.
pub fn search_path() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/usr/lib/lsw/bin")];
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    // virtiofsd lives in libexec on several distributions.
    dirs.push(PathBuf::from("/usr/libexec"));
    dirs.push(PathBuf::from("/usr/lib/qemu"));
    dirs
}

fn find_binaries(path: &[PathBuf]) -> Binaries {
    let find = |name: &str| path.iter().map(|d| d.join(name)).find(|p| p.is_file());
    Binaries {
        qemu: find("qemu-system-x86_64"),
        swtpm: find("swtpm"),
        passt: find("passt"),
        virtiofsd: find("virtiofsd"),
        newuidmap: find("newuidmap"),
    }
}

fn open_rw(p: &Path) -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(p)
        .is_ok()
}

fn parse_cpuinfo(s: &str) -> (CpuVendor, bool, u16) {
    let mut vendor = CpuVendor::Other;
    let mut virt = false;
    let mut cpus: u16 = 0;
    for line in s.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "processor" => cpus = cpus.saturating_add(1),
            "vendor_id" => {
                vendor = match value.trim() {
                    "GenuineIntel" => CpuVendor::Intel,
                    "AuthenticAMD" => CpuVendor::Amd,
                    _ => CpuVendor::Other,
                }
            }
            "flags" => {
                virt |= value.split_whitespace().any(|f| f == "vmx" || f == "svm");
            }
            _ => {}
        }
    }
    (vendor, virt, cpus)
}

fn parse_meminfo_total_mib(s: &str) -> u64 {
    s.lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

fn read_hex(p: &Path) -> Option<u32> {
    read_trim(p).and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
}

fn probe_gpus(root: &Path) -> Vec<Gpu> {
    let pci = root.join("sys/bus/pci/devices");
    let Ok(entries) = std::fs::read_dir(&pci) else {
        return Vec::new();
    };
    let render_nodes = render_node_map(root);
    let mut gpus: Vec<Gpu> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let dev = e.path();
            let class = read_hex(&dev.join("class"))?;
            if class >> 16 != PCI_CLASS_DISPLAY {
                return None;
            }
            let addr = e.file_name().to_string_lossy().into_owned();
            let link_name = |link: &str| {
                std::fs::read_link(dev.join(link))
                    .ok()
                    .and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned()))
            };
            Some(Gpu {
                vendor: read_hex(&dev.join("vendor"))? as u16,
                device: read_hex(&dev.join("device"))? as u16,
                driver: link_name("driver"),
                boot_vga: read_trim(&dev.join("boot_vga")).as_deref() == Some("1"),
                sriov_total_vfs: read_trim(&dev.join("sriov_totalvfs"))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                iommu_group: link_name("iommu_group").and_then(|s| s.parse().ok()),
                render_node: render_nodes
                    .iter()
                    .find(|(pci_addr, _)| *pci_addr == addr)
                    .map(|(_, node)| node.clone()),
                pci_addr: addr,
            })
        })
        .collect();
    gpus.sort_by(|a, b| a.pci_addr.cmp(&b.pci_addr));
    gpus
}

/// `(pci address, renderDxxx)` pairs from `/sys/class/drm`.
fn render_node_map(root: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(root.join("sys/class/drm")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("renderD") {
                return None;
            }
            let target = std::fs::read_link(e.path().join("device")).ok()?;
            let pci = target.file_name()?.to_string_lossy().into_owned();
            Some((pci, name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(tempfile::TempDir);

    impl Fake {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }
        fn root(&self) -> &Path {
            self.0.path()
        }
        fn write(&self, rel: &str, content: &str) {
            let p = self.root().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        #[cfg(unix)]
        fn link(&self, rel: &str, target: &str) {
            let p = self.root().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, p).unwrap();
        }
        #[cfg(unix)]
        fn gpu(&self, addr: &str, vendor: &str, boot: bool, group: u32, render: Option<&str>) {
            let d = format!("sys/bus/pci/devices/{addr}");
            self.write(&format!("{d}/class"), "0x030000\n");
            self.write(&format!("{d}/vendor"), vendor);
            self.write(&format!("{d}/device"), "0x1234");
            self.write(&format!("{d}/boot_vga"), if boot { "1" } else { "0" });
            self.link(
                &format!("{d}/iommu_group"),
                &format!("../../../kernel/iommu_groups/{group}"),
            );
            std::fs::create_dir_all(self.root().join(format!("sys/kernel/iommu_groups/{group}")))
                .unwrap();
            if let Some(node) = render {
                self.link(
                    &format!("sys/class/drm/{node}/device"),
                    &format!("../../../bus/pci/devices/{addr}"),
                );
            }
        }
    }

    const CPUINFO: &str = "processor\t: 0\nvendor_id\t: AuthenticAMD\nflags\t\t: fpu svm sse\n\nprocessor\t: 1\nvendor_id\t: AuthenticAMD\nflags\t\t: fpu svm sse\n";

    #[test]
    fn cpu_and_memory() {
        let f = Fake::new();
        f.write("proc/cpuinfo", CPUINFO);
        f.write(
            "proc/meminfo",
            "MemTotal:       32768000 kB\nMemFree: 1 kB\n",
        );
        let r = HostReport::probe_root(f.root(), &[]);
        assert_eq!(r.cpu_vendor, CpuVendor::Amd);
        assert!(r.virt_extensions);
        assert_eq!(r.logical_cpus, 2);
        assert_eq!(r.mem_total_mib, 32000);
        assert_eq!(r.tiers, vec![GpuTier::D]);
        // No KVM, no QEMU, no swtpm.
        let problems: Vec<_> = r.problems().into_iter().map(|p| p.what).collect();
        assert!(problems.iter().any(|p| p.contains("/dev/kvm")));
        assert!(problems.iter().any(|p| p.contains("qemu")));
        assert!(problems.iter().any(|p| p.contains("swtpm")));
    }

    #[cfg(unix)]
    #[test]
    fn single_gpu_gets_tier_a_not_b() {
        let f = Fake::new();
        f.gpu("0000:00:02.0", "0x1002", true, 1, Some("renderD128"));
        let r = HostReport::probe_root(f.root(), &[]);
        assert_eq!(r.gpus.len(), 1);
        assert_eq!(r.gpus[0].render_node.as_deref(), Some("renderD128"));
        assert_eq!(r.tiers, vec![GpuTier::A, GpuTier::D]);
        assert_eq!(r.recommended_tier(Profile::Games), GpuTier::A);
    }

    #[cfg(unix)]
    #[test]
    fn igpu_plus_dgpu_offers_passthrough_of_the_dgpu() {
        let f = Fake::new();
        f.gpu("0000:00:02.0", "0x8086", true, 1, Some("renderD128"));
        f.gpu("0000:01:00.0", "0x10de", false, 12, Some("renderD129"));
        let r = HostReport::probe_root(f.root(), &[]);
        assert_eq!(r.tiers, vec![GpuTier::A, GpuTier::B, GpuTier::D]);
        assert_eq!(r.passthrough_candidate().unwrap().pci_addr, "0000:01:00.0");
        assert_eq!(r.recommended_tier(Profile::Games), GpuTier::B);
        assert_eq!(r.recommended_tier(Profile::Productivity), GpuTier::A);
    }

    #[cfg(unix)]
    #[test]
    fn intel_sriov_is_tier_c() {
        let f = Fake::new();
        f.gpu("0000:00:02.0", "0x8086", true, 1, Some("renderD128"));
        f.write("sys/bus/pci/devices/0000:00:02.0/sriov_totalvfs", "7\n");
        let r = HostReport::probe_root(f.root(), &[]);
        assert_eq!(r.tiers, vec![GpuTier::A, GpuTier::C, GpuTier::D]);
    }

    #[test]
    fn finds_binaries_in_order() {
        let f = Fake::new();
        f.write("bundled/qemu-system-x86_64", "");
        f.write("usr/bin/qemu-system-x86_64", "");
        f.write("usr/bin/swtpm", "");
        let path = [f.root().join("bundled"), f.root().join("usr/bin")];
        let b = find_binaries(&path);
        assert_eq!(b.qemu.unwrap(), f.root().join("bundled/qemu-system-x86_64"));
        assert!(b.swtpm.is_some());
        assert!(b.passt.is_none());
    }
}
