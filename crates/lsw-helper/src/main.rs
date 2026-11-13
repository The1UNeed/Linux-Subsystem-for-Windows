//! `lsw-helper`: the only privileged part of LSW, run through polkit
//! (`pkexec /usr/lib/lsw/lsw-helper …`, action `dev.lsw.helper.gpu`).
//!
//! It does three things and nothing else, each on validated PCI addresses
//! of display controllers (and their same-slot audio functions):
//!
//! * `bind-vfio ADDR` – hand a GPU (and its HDMI/DP audio) to `vfio-pci`
//!   and give the invoking user its `/dev/vfio/<group>` node.
//! * `release ADDR` – give the devices back to their normal drivers.
//! * `create-vfs ADDR N` – create N SR-IOV virtual functions on an Intel GPU.

use lsw_daemon::gpu::{PciAddr, passthrough_set, read_function};
use std::path::{Path, PathBuf};

const INTEL: u32 = 0x8086;

struct Sys {
    /// `/sys/bus/pci`
    pci: PathBuf,
    /// `/dev/vfio`
    vfio: PathBuf,
}

impl Sys {
    fn devices(&self) -> PathBuf {
        self.pci.join("devices")
    }

    fn write(&self, path: &Path, value: &str) -> Result<(), String> {
        std::fs::write(path, value).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn bind_vfio(&self, gpu: &PciAddr, user: Option<u32>) -> Result<(), String> {
        let set = passthrough_set(&self.devices(), gpu)?;
        for f in &set {
            let dev = self.devices().join(f.addr.as_str());
            self.write(&dev.join("driver_override"), "vfio-pci")?;
            if f.driver.is_some() && f.driver.as_deref() != Some("vfio-pci") {
                self.write(&dev.join("driver/unbind"), f.addr.as_str())?;
            }
            self.write(&self.pci.join("drivers_probe"), f.addr.as_str())?;
        }
        if let (Some(uid), Some(group)) = (user, set[0].iommu_group.as_deref()) {
            // Only a numeric group name can come from sysfs; never a path.
            if !group.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("unexpected IOMMU group {group:?}"));
            }
            let node = self.vfio.join(group);
            std::os::unix::fs::chown(&node, Some(uid), None)
                .map_err(|e| format!("{}: {e}", node.display()))?;
        }
        Ok(())
    }

    fn release(&self, gpu: &PciAddr) -> Result<(), String> {
        let set = passthrough_set(&self.devices(), gpu)?;
        for f in &set {
            let dev = self.devices().join(f.addr.as_str());
            self.write(&dev.join("driver_override"), "\n")?;
            if f.driver.as_deref() == Some("vfio-pci") {
                self.write(&dev.join("driver/unbind"), f.addr.as_str())?;
            }
            self.write(&self.pci.join("drivers_probe"), f.addr.as_str())?;
        }
        Ok(())
    }

    fn create_vfs(&self, gpu: &PciAddr, n: u32) -> Result<(), String> {
        let f =
            read_function(&self.devices(), gpu).ok_or_else(|| format!("{gpu}: no such device"))?;
        if !f.is_display() || f.vendor != INTEL {
            return Err(format!("{gpu} is not an Intel GPU"));
        }
        let dev = self.devices().join(gpu.as_str());
        let total: u32 = std::fs::read_to_string(dev.join("sriov_totalvfs"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| format!("{gpu} does not support SR-IOV"))?;
        if n == 0 || n > total {
            return Err(format!("{gpu} supports 1..={total} virtual functions"));
        }
        // The count can only change from zero.
        self.write(&dev.join("sriov_numvfs"), "0")?;
        self.write(&dev.join("sriov_numvfs"), &n.to_string())
    }
}

fn run(args: &[String], sys: &Sys, user: Option<u32>) -> Result<(), String> {
    let addr = |i: usize| {
        args.get(i)
            .and_then(|a| PciAddr::parse(a))
            .ok_or_else(|| "expected a PCI address like 0000:01:00.0".to_owned())
    };
    match args.first().map(String::as_str) {
        Some("bind-vfio") if args.len() == 2 => sys.bind_vfio(&addr(1)?, user),
        Some("release") if args.len() == 2 => sys.release(&addr(1)?),
        Some("create-vfs") if args.len() == 3 => {
            let n = args[2]
                .parse()
                .map_err(|_| "expected a number of VFs".to_owned())?;
            sys.create_vfs(&addr(1)?, n)
        }
        _ => Err("usage: lsw-helper bind-vfio ADDR | release ADDR | create-vfs ADDR N".into()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // pkexec tells us who asked; the VFIO node goes to that user.
    let user = std::env::var("PKEXEC_UID")
        .ok()
        .and_then(|u| u.parse().ok());
    let sys = Sys {
        pci: "/sys/bus/pci".into(),
        vfio: "/dev/vfio".into(),
    };
    if let Err(e) = run(&args, &sys, user) {
        eprintln!("lsw-helper: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake /sys/bus/pci with one GPU + audio and one Intel iGPU.
    fn fake() -> (tempfile::TempDir, Sys) {
        let tmp = tempfile::tempdir().unwrap();
        let pci = tmp.path().join("pci");
        let mk = |addr: &str, vendor: &str, class: &str, group: &str, driver: &str| {
            let d = pci.join("devices").join(addr);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("vendor"), vendor).unwrap();
            std::fs::write(d.join("class"), class).unwrap();
            std::fs::write(d.join("driver_override"), "(null)").unwrap();
            let g = tmp.path().join("groups").join(group);
            std::fs::create_dir_all(&g).unwrap();
            std::os::unix::fs::symlink(&g, d.join("iommu_group")).unwrap();
            let drv = tmp.path().join("drivers").join(driver);
            std::fs::create_dir_all(&drv).unwrap();
            std::fs::write(drv.join("unbind"), "").unwrap();
            std::os::unix::fs::symlink(&drv, d.join("driver")).unwrap();
        };
        mk("0000:01:00.0", "0x10de", "0x030000", "14", "nouveau");
        mk("0000:01:00.1", "0x10de", "0x040300", "14", "snd_hda_intel");
        mk("0000:00:02.0", "0x8086", "0x030000", "0", "i915");
        std::fs::write(pci.join("devices/0000:00:02.0/sriov_totalvfs"), "7\n").unwrap();
        std::fs::write(pci.join("drivers_probe"), "").unwrap();
        let vfio = tmp.path().join("vfio");
        std::fs::create_dir_all(&vfio).unwrap();
        (tmp, Sys { pci, vfio })
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn bind_overrides_and_unbinds_gpu_and_audio() {
        let (_t, sys) = fake();
        run(&s(&["bind-vfio", "0000:01:00.0"]), &sys, None).unwrap();
        for f in ["0000:01:00.0", "0000:01:00.1"] {
            let o = std::fs::read_to_string(sys.devices().join(f).join("driver_override")).unwrap();
            assert_eq!(o, "vfio-pci");
        }
        assert_eq!(
            std::fs::read_to_string(sys.pci.join("drivers_probe")).unwrap(),
            "0000:01:00.1"
        );
    }

    #[test]
    fn refuses_anything_but_gpus_and_bad_input() {
        let (_t, sys) = fake();
        assert!(
            run(&s(&["bind-vfio", "0000:01:00.1"]), &sys, None).is_err(),
            "audio alone"
        );
        assert!(run(&s(&["bind-vfio", "../../etc"]), &sys, None).is_err());
        assert!(run(&s(&["bind-vfio"]), &sys, None).is_err());
        assert!(run(&s(&["rm", "-rf"]), &sys, None).is_err());
        assert!(
            run(&s(&["create-vfs", "0000:01:00.0", "2"]), &sys, None).is_err(),
            "not Intel"
        );
        assert!(
            run(&s(&["create-vfs", "0000:00:02.0", "8"]), &sys, None).is_err(),
            "too many"
        );
        run(&s(&["create-vfs", "0000:00:02.0", "2"]), &sys, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(sys.devices().join("0000:00:02.0/sriov_numvfs")).unwrap(),
            "2"
        );
    }
}
