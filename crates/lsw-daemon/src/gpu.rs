//! GPU tiers B (VFIO passthrough of a second GPU) and C (Intel SR-IOV
//! virtual function): PCI address validation, sysfs inspection and the QEMU
//! arguments. Binding devices to `vfio-pci` needs root and is done by
//! `lsw-helper` (polkit).
//!
//! Tier A (paravirtualized 3D) needs a Windows display driver that is not
//! part of this release; [`TIER_A_DRIVER_READY`] keeps it unselectable so
//! no VM turns Secure Boot off for a driver it does not have.

use std::path::Path;

/// Whether the tier A guest driver ships with this build.
pub const TIER_A_DRIVER_READY: bool = false;

/// First PCI slot used for passed-through functions (after the fixed
/// slots in [`crate::qemu::slot`] and the shared-folder range).
pub const PASSTHROUGH_SLOT: u8 = 0x08;

/// A validated PCI address `dddd:bb:ss.f` (lower-case hex).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PciAddr(String);

impl PciAddr {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        let (rest, func) = s.rsplit_once('.')?;
        let mut parts = rest.split(':');
        let (domain, bus, slot) = (parts.next()?, parts.next()?, parts.next()?);
        let hex = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit());
        let ok = parts.next().is_none()
            && hex(domain, 4)
            && hex(bus, 2)
            && hex(slot, 2)
            && func.len() == 1
            && matches!(func.as_bytes()[0], b'0'..=b'7')
            && u8::from_str_radix(slot, 16).is_ok_and(|s| s < 0x20);
        ok.then_some(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `dddd:bb:ss` without the function.
    pub fn slot(&self) -> &str {
        self.0.rsplit_once('.').map_or(&self.0, |(s, _)| s)
    }

    pub fn function(&self) -> u8 {
        self.0.as_bytes()[self.0.len() - 1] - b'0'
    }
}

impl TryFrom<String> for PciAddr {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        PciAddr::parse(&s).ok_or_else(|| format!("invalid PCI address {s:?}"))
    }
}

impl From<PciAddr> for String {
    fn from(a: PciAddr) -> String {
        a.0
    }
}

impl std::fmt::Display for PciAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn read_hex(dev: &Path, name: &str) -> Option<u32> {
    let s = std::fs::read_to_string(dev.join(name)).ok()?;
    u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

/// A PCI function as seen in sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciFunction {
    pub addr: PciAddr,
    pub vendor: u32,
    pub class: u32,
    pub driver: Option<String>,
    pub iommu_group: Option<String>,
}

impl PciFunction {
    pub fn is_display(&self) -> bool {
        self.class >> 16 == 0x03
    }
    pub fn is_audio(&self) -> bool {
        self.class >> 8 == 0x0403
    }
}

/// Read one function from `sysfs` (`/sys/bus/pci/devices`).
pub fn read_function(sysfs: &Path, addr: &PciAddr) -> Option<PciFunction> {
    let dev = sysfs.join(addr.as_str());
    let name_of = |link: &str| {
        std::fs::read_link(dev.join(link))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    };
    Some(PciFunction {
        addr: addr.clone(),
        vendor: read_hex(&dev, "vendor")?,
        class: read_hex(&dev, "class")?,
        driver: name_of("driver"),
        iommu_group: name_of("iommu_group"),
    })
}

/// The functions to pass through for `gpu`. A GPU at function 0 takes
/// every function of its slot (HDMI/DP audio, USB-C, serial bus…); an
/// SR-IOV virtual function goes alone. All must share one IOMMU group, and
/// that group must hold nothing else but PCI bridges, or VFIO cannot use it.
pub fn passthrough_set(sysfs: &Path, gpu: &PciAddr) -> Result<Vec<PciFunction>, String> {
    let main = read_function(sysfs, gpu).ok_or_else(|| format!("{gpu}: no such PCI device"))?;
    if !main.is_display() {
        return Err(format!("{gpu} is not a display controller"));
    }
    let group = main
        .iommu_group
        .clone()
        .ok_or_else(|| format!("{gpu} has no IOMMU group (enable VT-d / AMD-Vi)"))?;
    let mut set = vec![main];
    if gpu.function() == 0 {
        for f in 1..8u8 {
            let Some(addr) = PciAddr::parse(&format!("{}.{f}", gpu.slot())) else {
                continue;
            };
            if let Some(func) = read_function(sysfs, &addr) {
                if func.iommu_group.as_deref() != Some(group.as_str()) {
                    return Err(format!("{addr} is not in {gpu}'s IOMMU group"));
                }
                set.push(func);
            }
        }
    }
    // Every other member of the group would go with it.
    if let Ok(members) = std::fs::read_dir(sysfs.join(gpu.as_str()).join("iommu_group/devices")) {
        for m in members.flatten() {
            let name = m.file_name().to_string_lossy().into_owned();
            let Some(addr) = PciAddr::parse(&name) else {
                continue;
            };
            if set.iter().any(|f| f.addr == addr) {
                continue;
            }
            let bridge = read_function(sysfs, &addr).is_some_and(|f| f.class >> 8 == 0x0604);
            if !bridge {
                return Err(format!(
                    "{addr} shares {gpu}'s IOMMU group; the group cannot be given to Windows \
                     (try another PCIe slot)"
                ));
            }
        }
    }
    Ok(set)
}

/// Before QEMU starts with passed-through `functions`: each must still be
/// on vfio-pci (a host restart gives it back to its driver), its VFIO group
/// node must be open to us, and the locked-memory limit must cover the
/// guest's RAM (VFIO pins all of it). `None` when ready, else what to do.
pub fn passthrough_problem(
    sysfs: &Path,
    vfio: &Path,
    functions: &[PciAddr],
    memlock_limit: Option<u64>,
    guest_bytes: u64,
) -> Option<String> {
    for addr in functions {
        let Some(f) = read_function(sysfs, addr) else {
            return Some(format!(
                "{addr} is no longer present; run `lsw gpu release`"
            ));
        };
        if f.driver.as_deref() != Some("vfio-pci") {
            return Some(format!(
                "{addr} is not handed to Windows (driver: {}); after a host restart run \
                 `lsw gpu assign {}` again, or `lsw gpu release`",
                f.driver.as_deref().unwrap_or("none"),
                functions[0]
            ));
        }
        if let Some(group) = f.iommu_group.as_deref() {
            let node = vfio.join(group);
            if std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&node)
                .is_err()
            {
                return Some(format!(
                    "{} is not accessible; run `lsw gpu assign {}` again",
                    node.display(),
                    functions[0]
                ));
            }
        }
    }
    // VFIO also pins ROMs, VGA and firmware memory (libvirt allows 1 GiB).
    let needed = guest_bytes + (1 << 30);
    match memlock_limit {
        Some(limit) if limit < needed => Some(format!(
            "GPU passthrough locks all of Windows' memory ({} MiB with device memory), but the locked-memory \
             limit is {} MiB. Raise it for your user session (e.g. LimitMEMLOCK=infinity in \
             a drop-in for user@.service, then log in again); see the user guide",
            needed >> 20,
            limit >> 20
        )),
        _ => None,
    }
}

/// QEMU arguments passing `functions` through on fixed slots, keeping the
/// multi-function layout (GPU at .0, its audio at .1 …).
pub fn qemu_args(functions: &[PciAddr]) -> Vec<String> {
    // Group by host slot; each group gets one guest slot.
    let mut groups: Vec<(&str, Vec<&PciAddr>)> = Vec::new();
    for f in functions {
        match groups.iter_mut().find(|(s, _)| *s == f.slot()) {
            Some((_, g)) => g.push(f),
            None => groups.push((f.slot(), vec![f])),
        }
    }
    let mut args = Vec::new();
    for (idx, (_, group)) in groups.iter().enumerate() {
        let slot = PASSTHROUGH_SLOT + idx as u8;
        // A GPU with its audio keeps its layout (.0 + .1). A group without
        // a function 0 (e.g. an SR-IOV VF at 00:02.1) is renumbered from
        // .0, or the guest's PCI scan would never find it.
        let keep = group.iter().any(|f| f.function() == 0);
        for (i, f) in group.iter().enumerate() {
            let func = if keep { f.function() } else { i as u8 };
            let multi = if func == 0 && group.len() > 1 {
                ",multifunction=on"
            } else {
                ""
            };
            args.push("-device".into());
            args.push(format!(
                "vfio-pci,host={},bus=pcie.0,addr={slot:#04x}.{func}{multi}",
                f.as_str()
            ));
        }
    }
    args
}

/// Why `new` cannot be assigned while `current` (the functions assigned
/// now) is: one assignment at a time, so `lsw gpu release` always knows
/// what to give back. Re-assigning the same device is allowed.
pub fn assignment_conflict(current: &[PciAddr], new: &PciAddr) -> Option<String> {
    match current.first() {
        Some(first) if first != new => Some(format!(
            "{first} is already assigned to Windows; run `lsw gpu release` first"
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_assignment_at_a_time() {
        let a = PciAddr::parse("0000:00:02.1").unwrap();
        let b = PciAddr::parse("0000:00:02.2").unwrap();
        assert_eq!(assignment_conflict(&[], &a), None);
        assert_eq!(
            assignment_conflict(std::slice::from_ref(&a), &a),
            None,
            "same device"
        );
        let e = assignment_conflict(std::slice::from_ref(&a), &b).unwrap();
        assert!(
            e.contains("0000:00:02.1") && e.contains("lsw gpu release"),
            "{e}"
        );
    }

    fn func(sys: &Path, addr: &str, vendor: &str, class: &str, group: &str, driver: &str) {
        let d = sys.join(addr);
        std::fs::create_dir_all(&d).unwrap();
        let _ = std::fs::remove_file(d.join("iommu_group"));
        std::fs::write(d.join("vendor"), vendor).unwrap();
        std::fs::write(d.join("class"), class).unwrap();
        let groups = sys.join("../groups").join(group);
        std::fs::create_dir_all(&groups).unwrap();
        std::os::unix::fs::symlink(&groups, d.join("iommu_group")).unwrap();
        let drv = sys.join("../drivers").join(driver);
        std::fs::create_dir_all(&drv).unwrap();
        std::os::unix::fs::symlink(&drv, d.join("driver")).unwrap();
    }

    #[test]
    fn pci_addresses_are_strict() {
        assert!(PciAddr::parse("0000:01:00.0").is_some());
        assert_eq!(
            PciAddr::parse("0000:0A:1f.7").unwrap().as_str(),
            "0000:0a:1f.7"
        );
        for bad in [
            "01:00.0",
            "0000:01:00.8",
            "0000:01:20.0",
            "0000:01:00",
            "../../x",
            "0000:01:00.0;rm",
        ] {
            assert!(PciAddr::parse(bad).is_none(), "{bad}");
        }
        let a = PciAddr::parse("0000:01:00.1").unwrap();
        assert_eq!((a.slot(), a.function()), ("0000:01:00", 1));
    }

    #[test]
    fn passthrough_set_includes_hdmi_audio_in_the_same_group() {
        let tmp = tempfile::tempdir().unwrap();
        let sys = tmp.path().join("devices");
        func(&sys, "0000:01:00.0", "0x10de", "0x030000", "14", "nouveau");
        func(
            &sys,
            "0000:01:00.1",
            "0x10de",
            "0x040300",
            "14",
            "snd_hda_intel",
        );
        func(&sys, "0000:00:02.0", "0x8086", "0x030000", "0", "i915");
        let gpu = PciAddr::parse("0000:01:00.0").unwrap();
        let set = passthrough_set(&sys, &gpu).unwrap();
        assert_eq!(set.len(), 2);
        assert_eq!(set[0].driver.as_deref(), Some("nouveau"));
        let args = qemu_args(&set.iter().map(|f| f.addr.clone()).collect::<Vec<_>>()).join(" ");
        assert_eq!(
            args,
            "-device vfio-pci,host=0000:01:00.0,bus=pcie.0,addr=0x08.0,multifunction=on \
             -device vfio-pci,host=0000:01:00.1,bus=pcie.0,addr=0x08.1"
        );
        // An isolated SR-IOV VF lands on function 0 of its own slot.
        let vf = qemu_args(&[PciAddr::parse("0000:00:02.1").unwrap()]).join(" ");
        assert_eq!(
            vf,
            "-device vfio-pci,host=0000:00:02.1,bus=pcie.0,addr=0x08.0"
        );
        // Audio in a different group cannot be passed through safely.
        func(&sys, "0000:02:00.0", "0x1002", "0x030000", "20", "amdgpu");
        func(
            &sys,
            "0000:02:00.1",
            "0x1002",
            "0x040300",
            "21",
            "snd_hda_intel",
        );
        assert!(passthrough_set(&sys, &PciAddr::parse("0000:02:00.0").unwrap()).is_err());
        // Not a GPU.
        assert!(passthrough_set(&sys, &PciAddr::parse("0000:01:00.1").unwrap()).is_err());
    }

    #[test]
    fn a_gpu_takes_its_whole_slot_and_its_group_must_be_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let sys = tmp.path().join("devices");
        // A GPU with audio, USB-C (xHCI) and a serial-bus function.
        func(&sys, "0000:01:00.0", "0x10de", "0x030000", "1", "nouveau");
        func(
            &sys,
            "0000:01:00.1",
            "0x10de",
            "0x040300",
            "1",
            "snd_hda_intel",
        );
        func(&sys, "0000:01:00.2", "0x10de", "0x0c0330", "1", "xhci_hcd");
        func(
            &sys,
            "0000:01:00.3",
            "0x10de",
            "0x0c8000",
            "1",
            "nvidia-gpu",
        );
        let members = tmp.path().join("groups/1/devices");
        std::fs::create_dir_all(&members).unwrap();
        for m in [
            "0000:01:00.0",
            "0000:01:00.1",
            "0000:01:00.2",
            "0000:01:00.3",
        ] {
            std::fs::create_dir_all(members.join(m)).unwrap();
        }
        let gpu = PciAddr::parse("0000:01:00.0").unwrap();
        assert_eq!(passthrough_set(&sys, &gpu).unwrap().len(), 4);
        // A bridge in the group is fine; another device is not.
        func(&sys, "0000:00:01.0", "0x8086", "0x060400", "1", "pcieport");
        std::fs::create_dir_all(members.join("0000:00:01.0")).unwrap();
        assert!(passthrough_set(&sys, &gpu).is_ok());
        func(&sys, "0000:03:00.0", "0x8086", "0x020000", "1", "e1000e");
        std::fs::create_dir_all(members.join("0000:03:00.0")).unwrap();
        let e = passthrough_set(&sys, &gpu).unwrap_err();
        assert!(e.contains("0000:03:00.0"), "{e}");
        // An SR-IOV VF goes alone, not with its sibling VFs.
        func(&sys, "0000:00:02.1", "0x8086", "0x030000", "7", "i915");
        func(&sys, "0000:00:02.2", "0x8086", "0x030000", "8", "i915");
        let vf = PciAddr::parse("0000:00:02.1").unwrap();
        assert_eq!(passthrough_set(&sys, &vf).unwrap().len(), 1);
    }

    #[test]
    fn passthrough_needs_vfio_its_node_and_enough_locked_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let sys = tmp.path().join("devices");
        let vfio = tmp.path().join("vfio");
        std::fs::create_dir_all(&vfio).unwrap();
        func(&sys, "0000:01:00.0", "0x10de", "0x030000", "14", "nouveau");
        let set = [PciAddr::parse("0000:01:00.0").unwrap()];
        let gib8 = 8u64 << 30;
        // After a host restart the GPU is back on its own driver.
        let e = passthrough_problem(&sys, &vfio, &set, None, gib8).unwrap();
        assert!(e.contains("lsw gpu assign 0000:01:00.0"), "{e}");
        // On vfio-pci, but the node is missing.
        std::fs::remove_file(sys.join("0000:01:00.0/driver")).unwrap();
        func(&sys, "0000:01:00.0", "0x10de", "0x030000", "14", "vfio-pci");
        assert!(
            passthrough_problem(&sys, &vfio, &set, None, gib8)
                .unwrap()
                .contains("not accessible")
        );
        std::fs::write(vfio.join("14"), "").unwrap();
        // The default 8 MiB locked-memory limit cannot hold 8 GiB.
        let e = passthrough_problem(&sys, &vfio, &set, Some(8 << 20), gib8).unwrap();
        assert!(e.contains("LimitMEMLOCK"), "{e}");
        assert_eq!(
            passthrough_problem(&sys, &vfio, &set, Some(u64::MAX), gib8),
            None
        );
    }
}
