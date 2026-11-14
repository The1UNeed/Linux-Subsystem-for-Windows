//! `lsw-helper`: the only privileged part of LSW, run through polkit
//! (`pkexec /usr/lib/lsw/lsw-helper …`, action `dev.lsw.helper.gpu`).
//!
//! It does three things and nothing else, each on validated PCI addresses
//! of display controllers (`bind-vfio` refuses the boot GPU):
//!
//! * `bind-vfio ADDR` – hand a GPU and every function of its slot (HDMI/DP
//!   audio, USB-C…; its IOMMU group must hold nothing else but bridges) to
//!   `vfio-pci` and give the invoking user its `/dev/vfio/<group>` node.
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

/// What the helper asks of the kernel: sysfs writes and module loading.
/// Tests substitute a stateful fake that emulates driver binding.
trait Kernel {
    fn write(&self, path: &Path, value: &str) -> Result<(), String>;
    fn load_module(&self, name: &str) -> Result<(), String>;
}

struct RealKernel;

impl Kernel for RealKernel {
    fn write(&self, path: &Path, value: &str) -> Result<(), String> {
        std::fs::write(path, value).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn load_module(&self, name: &str) -> Result<(), String> {
        // A fixed module name and absolute paths; never user input.
        let modprobe = ["/usr/sbin/modprobe", "/sbin/modprobe"]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .ok_or("modprobe not found")?;
        let status = std::process::Command::new(modprobe)
            .arg(name)
            .env_clear()
            .status()
            .map_err(|e| format!("{modprobe}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("modprobe {name} failed ({status})"))
        }
    }
}

struct Sys {
    /// `/sys/bus/pci`
    pci: PathBuf,
    /// `/dev/vfio`
    vfio: PathBuf,
    kernel: Box<dyn Kernel>,
}

impl Sys {
    fn devices(&self) -> PathBuf {
        self.pci.join("devices")
    }

    fn write(&self, path: &Path, value: &str) -> Result<(), String> {
        self.kernel.write(path, value)
    }

    /// `vfio-pci` must be loaded: an override does not load its driver,
    /// and probing would then leave the device unbound.
    fn ensure_vfio_pci(&self) -> Result<(), String> {
        let driver = self.pci.join("drivers/vfio-pci");
        if driver.exists() {
            return Ok(());
        }
        self.kernel.load_module("vfio-pci")?;
        if driver.exists() {
            Ok(())
        } else {
            Err("the vfio-pci driver is not available in this kernel".into())
        }
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
        self.ensure_vfio_pci()?;
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
        // A function that had no driver stays unbound (probing would bind
        // a driver the user had kept off it).
        if let Some(d) = &o.driver
            && self.current_driver(&o.addr).as_deref() != Some(d.as_str())
        {
            let _ = self.write(
                &self.pci.join("drivers").join(d).join("bind"),
                o.addr.as_str(),
            );
        }
    }

    fn bind_one(&self, f: &lsw_daemon::gpu::PciFunction) -> Result<(), String> {
        let dev = self.devices().join(f.addr.as_str());
        self.write(&dev.join("driver_override"), "vfio-pci")?;
        if f.driver.is_some() && f.driver.as_deref() != Some("vfio-pci") {
            self.write(&dev.join("driver/unbind"), f.addr.as_str())?;
        }
        self.write(&self.pci.join("drivers_probe"), f.addr.as_str())?;
        // A probe can succeed without binding (driver refused the device).
        match self.current_driver(&f.addr) {
            Some(d) if d == "vfio-pci" => Ok(()),
            other => Err(format!(
                "{}: bound to {} instead of vfio-pci",
                f.addr,
                other.as_deref().unwrap_or("no driver")
            )),
        }
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
        kernel: Box::new(RealKernel),
    };
    if let Err(e) = run(&args, &sys, user) {
        eprintln!("lsw-helper: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Emulates the sysfs side effects the helper relies on: overrides,
    /// unbind, probe (override first, else the device's native driver, if
    /// loaded), explicit bind, and loading vfio-pci.
    struct FakeKernel {
        pci: PathBuf,
        /// The driver each device binds to on its own.
        native: std::collections::HashMap<String, Option<String>>,
        /// Whether `modprobe vfio-pci` works.
        can_load_vfio: bool,
        /// Whether vfio-pci accepts devices on probe.
        vfio_accepts: bool,
    }

    impl FakeKernel {
        fn bind(&self, addr: &str, driver: &str) {
            let dev = self.pci.join("devices").join(addr);
            let _ = std::fs::remove_file(dev.join("driver"));
            std::os::unix::fs::symlink(self.pci.join("drivers").join(driver), dev.join("driver"))
                .unwrap();
        }
    }

    impl Kernel for FakeKernel {
        fn write(&self, path: &Path, value: &str) -> Result<(), String> {
            std::fs::write(path, value).map_err(|e| format!("{}: {e}", path.display()))?;
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let parent = path.parent().unwrap();
            match name.as_str() {
                "unbind" => {
                    let _ =
                        std::fs::remove_file(self.pci.join("devices").join(value).join("driver"));
                }
                "bind" => {
                    let driver = parent.file_name().unwrap().to_string_lossy().into_owned();
                    self.bind(value, &driver);
                }
                "drivers_probe" => {
                    let dev = self.pci.join("devices").join(value);
                    if dev.join("driver").exists() {
                        return Ok(());
                    }
                    let ov =
                        std::fs::read_to_string(dev.join("driver_override")).unwrap_or_default();
                    let ov = ov.trim();
                    let target = if !ov.is_empty() && ov != "(null)" {
                        let ok = self.pci.join("drivers").join(ov).exists()
                            && (ov != "vfio-pci" || self.vfio_accepts);
                        ok.then(|| ov.to_owned())
                    } else {
                        self.native.get(value).cloned().flatten()
                    };
                    if let Some(d) = target {
                        self.bind(value, &d);
                    }
                }
                _ => {}
            }
            Ok(())
        }

        fn load_module(&self, name: &str) -> Result<(), String> {
            if name == "vfio-pci" && self.can_load_vfio {
                std::fs::create_dir_all(self.pci.join("drivers/vfio-pci")).unwrap();
                std::fs::write(self.pci.join("drivers/vfio-pci/unbind"), "").unwrap();
                Ok(())
            } else {
                Err(format!("modprobe {name} failed"))
            }
        }
    }

    /// A fake /sys/bus/pci with one GPU + audio and one Intel iGPU, and a
    /// fake kernel. vfio-pci is not loaded yet.
    fn fake_with(can_load_vfio: bool, vfio_accepts: bool) -> (tempfile::TempDir, Sys) {
        let tmp = tempfile::tempdir().unwrap();
        let pci = tmp.path().join("pci");
        let mut native = std::collections::HashMap::new();
        let mut mk = |addr: &str, vendor: &str, class: &str, group: &str, driver: Option<&str>| {
            let d = pci.join("devices").join(addr);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("vendor"), vendor).unwrap();
            std::fs::write(d.join("class"), class).unwrap();
            std::fs::write(d.join("driver_override"), "(null)").unwrap();
            let g = tmp.path().join("groups").join(group);
            std::fs::create_dir_all(&g).unwrap();
            std::os::unix::fs::symlink(&g, d.join("iommu_group")).unwrap();
            if let Some(driver) = driver {
                let drv = pci.join("drivers").join(driver);
                std::fs::create_dir_all(&drv).unwrap();
                std::fs::write(drv.join("unbind"), "").unwrap();
                std::fs::write(drv.join("bind"), "").unwrap();
                std::os::unix::fs::symlink(&drv, d.join("driver")).unwrap();
            }
            native.insert(addr.to_owned(), driver.map(str::to_owned));
        };
        mk("0000:01:00.0", "0x10de", "0x030000", "14", Some("nouveau"));
        mk(
            "0000:01:00.1",
            "0x10de",
            "0x040300",
            "14",
            Some("snd_hda_intel"),
        );
        mk("0000:00:02.0", "0x8086", "0x030000", "0", Some("i915"));
        std::fs::write(pci.join("devices/0000:00:02.0/boot_vga"), "1\n").unwrap();
        std::fs::write(pci.join("devices/0000:00:02.0/sriov_totalvfs"), "7\n").unwrap();
        std::fs::write(pci.join("drivers_probe"), "").unwrap();
        let vfio = tmp.path().join("vfio");
        std::fs::create_dir_all(&vfio).unwrap();
        let kernel = FakeKernel {
            pci: pci.clone(),
            native,
            can_load_vfio,
            vfio_accepts,
        };
        (
            tmp,
            Sys {
                pci,
                vfio,
                kernel: Box::new(kernel),
            },
        )
    }

    fn fake() -> (tempfile::TempDir, Sys) {
        fake_with(true, true)
    }

    fn driver_of(sys: &Sys, addr: &str) -> Option<String> {
        sys.current_driver(&PciAddr::parse(addr).unwrap())
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
            assert_eq!(driver_of(&sys, f).as_deref(), Some("vfio-pci"), "{f}");
        }
        // vfio-pci was loaded on demand.
        assert!(sys.pci.join("drivers/vfio-pci").exists());
    }

    #[test]
    fn nothing_is_touched_when_vfio_pci_cannot_be_loaded() {
        let (_t, sys) = fake_with(false, true);
        let e = run(&s(&["bind-vfio", "0000:01:00.0"]), &sys, None).unwrap_err();
        assert!(e.contains("vfio-pci"), "{e}");
        assert_eq!(driver_of(&sys, "0000:01:00.0").as_deref(), Some("nouveau"));
        assert_eq!(override_of(&sys, "0000:01:00.0"), "(null)");
    }

    #[test]
    fn a_probe_that_does_not_bind_vfio_is_a_failure_and_is_rolled_back() {
        let (_t, sys) = fake_with(true, false);
        let e = run(&s(&["bind-vfio", "0000:01:00.0"]), &sys, None).unwrap_err();
        assert!(e.contains("instead of vfio-pci"), "{e}");
        assert_eq!(driver_of(&sys, "0000:01:00.0").as_deref(), Some("nouveau"));
        assert_eq!(override_of(&sys, "0000:01:00.0"), "\n");
    }

    #[test]
    fn a_function_that_had_no_driver_stays_unbound_after_rollback() {
        let (_t, sys) = fake();
        // The audio function was deliberately left unbound, though its
        // driver is loaded.
        std::fs::remove_file(sys.devices().join("0000:01:00.1/driver")).unwrap();
        // Ownership fails at the end: everything is rolled back.
        assert!(run(&s(&["bind-vfio", "0000:01:00.0"]), &sys, Some(1000)).is_err());
        assert_eq!(driver_of(&sys, "0000:01:00.1"), None, "left unbound");
        assert_eq!(driver_of(&sys, "0000:01:00.0").as_deref(), Some("nouveau"));
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
        // The GPU, already on vfio-pci, went back to its original driver.
        assert_eq!(driver_of(&sys, "0000:01:00.0").as_deref(), Some("nouveau"));
        assert_eq!(
            driver_of(&sys, "0000:01:00.1").as_deref(),
            Some("snd_hda_intel")
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
        assert_eq!(driver_of(&sys, "0000:01:00.0").as_deref(), Some("nouveau"));
        assert_eq!(
            driver_of(&sys, "0000:01:00.1").as_deref(),
            Some("snd_hda_intel")
        );
    }
}
