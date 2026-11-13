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

/// A PCI function's state before `bind_vfio` touched it.
struct Original {
    addr: PciAddr,
    driver_override: String,
    driver: Option<String>,
}

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
        // Never take the GPU that drives the Linux console/desktop.
        let boot_vga = std::fs::read_to_string(self.devices().join(gpu.as_str()).join("boot_vga"))
            .is_ok_and(|v| v.trim() == "1");
        if boot_vga {
            return Err(format!(
                "{gpu} drives the Linux display; assign a second GPU"
            ));
        }
        let set = passthrough_set(&self.devices(), gpu)?;
        // Each function's state is recorded before it is touched; any later
        // failure (including the failing function itself and the VFIO node
        // ownership) restores every touched function.
        let mut touched: Vec<Original> = Vec::new();
        let result = (|| {
            for f in &set {
                touched.push(self.snapshot(f));
                self.bind_one(f)?;
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
        })();
        if result.is_err() {
            for original in touched.iter().rev() {
                self.restore(original);
            }
        }
        result
    }

    fn current_driver(&self, addr: &PciAddr) -> Option<String> {
        std::fs::read_link(self.devices().join(addr.as_str()).join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    }

    fn snapshot(&self, f: &lsw_daemon::gpu::PciFunction) -> Original {
        let dev = self.devices().join(f.addr.as_str());
        Original {
            addr: f.addr.clone(),
            driver_override: std::fs::read_to_string(dev.join("driver_override"))
                .unwrap_or_default()
                .trim()
                .to_owned(),
            driver: f.driver.clone(),
        }
    }

    /// Best effort: put a function back as it was before `bind_vfio`.
    fn restore(&self, o: &Original) {
        let dev = self.devices().join(o.addr.as_str());
        let ov = if o.driver_override.is_empty() || o.driver_override == "(null)" {
            "\n"
        } else {
            o.driver_override.as_str()
        };
        let _ = self.write(&dev.join("driver_override"), ov);
        let now = self.current_driver(&o.addr);
        if now.is_some() && now != o.driver {
            let _ = self.write(&dev.join("driver/unbind"), o.addr.as_str());
        }
        match &o.driver {
            Some(d) if self.current_driver(&o.addr).as_deref() != Some(d.as_str()) => {
                let _ = self.write(
                    &self.pci.join("drivers").join(d).join("bind"),
                    o.addr.as_str(),
                );
            }
            Some(_) => {}
            None => {
                let _ = self.write(&self.pci.join("drivers_probe"), o.addr.as_str());
            }
        }
    }

    fn bind_one(&self, f: &lsw_daemon::gpu::PciFunction) -> Result<(), String> {
        let dev = self.devices().join(f.addr.as_str());
        self.write(&dev.join("driver_override"), "vfio-pci")?;
        if f.driver.is_some() && f.driver.as_deref() != Some("vfio-pci") {
            self.write(&dev.join("driver/unbind"), f.addr.as_str())?;
        }
        self.write(&self.pci.join("drivers_probe"), f.addr.as_str())
    }

    fn release_one(&self, addr: &PciAddr, on_vfio: bool) -> Result<(), String> {
        let dev = self.devices().join(addr.as_str());
        self.write(&dev.join("driver_override"), "\n")?;
        if on_vfio {
            self.write(&dev.join("driver/unbind"), addr.as_str())?;
        }
        self.write(&self.pci.join("drivers_probe"), addr.as_str())
    }

    fn release(&self, gpu: &PciAddr) -> Result<(), String> {
        let set = passthrough_set(&self.devices(), gpu)?;
        for f in &set {
            self.release_one(&f.addr, f.driver.as_deref() == Some("vfio-pci"))?;
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
        std::fs::write(pci.join("devices/0000:00:02.0/boot_vga"), "1\n").unwrap();
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
        let e = run(&s(&["bind-vfio", "0000:00:02.0"]), &sys, None).unwrap_err();
        assert!(e.contains("drives the Linux display"), "{e}");
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

    fn override_of(sys: &Sys, addr: &str) -> String {
        std::fs::read_to_string(sys.devices().join(addr).join("driver_override"))
            .unwrap_or_default()
    }

    #[test]
    fn a_failing_function_is_rolled_back_with_the_ones_before_it() {
        let (_t, sys) = fake();
        // The audio function's override cannot be written.
        let ov = sys.devices().join("0000:01:00.1/driver_override");
        std::fs::remove_file(&ov).unwrap();
        std::fs::create_dir(&ov).unwrap();
        assert!(run(&s(&["bind-vfio", "0000:01:00.0"]), &sys, None).is_err());
        assert_eq!(
            override_of(&sys, "0000:01:00.0"),
            "\n",
            "GPU override restored"
        );
        // The GPU was given back to its original driver.
        let bind = sys.pci.join("drivers/nouveau/bind");
        assert_eq!(
            std::fs::read_to_string(bind).unwrap_or_default(),
            "",
            "still bound: nothing to rebind"
        );
    }

    #[test]
    fn an_ownership_failure_rolls_everything_back() {
        let (_t, sys) = fake();
        // The VFIO node for group 14 does not exist: chown fails at the end.
        assert!(run(&s(&["bind-vfio", "0000:01:00.0"]), &sys, Some(1000)).is_err());
        for f in ["0000:01:00.0", "0000:01:00.1"] {
            assert_eq!(override_of(&sys, f), "\n", "{f} override restored");
        }
    }
}
