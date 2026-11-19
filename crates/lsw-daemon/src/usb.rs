//! USB passthrough: list host devices and hot-plug them into the VM's xHCI
//! controller with QMP (`device_add usb-host`).

use crate::qmp::{Qmp, QmpError};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};

/// A USB device on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsbDevice {
    pub bus: u8,
    pub addr: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    /// `/dev/bus/usb/BBB/DDD`, which QEMU opens.
    pub node: PathBuf,
}

impl UsbDevice {
    /// QEMU device id used for this device in the VM.
    pub fn qemu_id(&self) -> String {
        format!("usb-{}-{}", self.bus, self.addr)
    }

    pub fn label(&self) -> String {
        let name = match (&self.manufacturer, &self.product) {
            (Some(m), Some(p)) => format!("{m} {p}"),
            (None, Some(p)) => p.clone(),
            (Some(m), None) => m.clone(),
            (None, None) => "USB device".into(),
        };
        format!("{name} ({:04x}:{:04x})", self.vendor_id, self.product_id)
    }
}

fn read_trim(p: &Path) -> Option<String> {
    let s = std::fs::read_to_string(p).ok()?;
    let t: String = s
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect();
    (!t.is_empty()).then_some(t)
}

/// List host USB devices from `sysfs` (`/sys/bus/usb/devices`), skipping
/// hubs (root hubs included), which cannot be passed through.
pub fn list(sysfs: &Path, dev: &Path) -> Vec<UsbDevice> {
    let Ok(entries) = std::fs::read_dir(sysfs) else {
        return Vec::new();
    };
    let mut out: Vec<UsbDevice> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let d = e.path();
            let hex = |name: &str| u16::from_str_radix(&read_trim(&d.join(name))?, 16).ok();
            let dec = |name: &str| read_trim(&d.join(name))?.parse::<u8>().ok();
            if read_trim(&d.join("bDeviceClass")).as_deref() == Some("09") {
                return None;
            }
            let (bus, addr) = (dec("busnum")?, dec("devnum")?);
            Some(UsbDevice {
                bus,
                addr,
                vendor_id: hex("idVendor")?,
                product_id: hex("idProduct")?,
                manufacturer: read_trim(&d.join("manufacturer")),
                product: read_trim(&d.join("product")),
                node: dev.join(format!("{bus:03}/{addr:03}")),
            })
        })
        .collect();
    out.sort_by_key(|d| (d.bus, d.addr));
    out
}

/// Find a device by `BUS-ADDR` or `VID:PID` (first match).
pub fn find<'a>(devices: &'a [UsbDevice], spec: &str) -> Option<&'a UsbDevice> {
    if let Some((v, p)) = spec.split_once(':') {
        let (v, p) = (
            u16::from_str_radix(v, 16).ok()?,
            u16::from_str_radix(p, 16).ok()?,
        );
        return devices
            .iter()
            .find(|d| d.vendor_id == v && d.product_id == p);
    }
    let (b, a) = spec.split_once('-')?;
    let (b, a) = (b.parse::<u8>().ok()?, a.parse::<u8>().ok()?);
    devices.iter().find(|d| d.bus == b && d.addr == a)
}

#[derive(Debug, thiserror::Error)]
pub enum UsbError {
    #[error(
        "no permission to open {path}. Give your user access with a udev rule, e.g. in \
         /etc/udev/rules.d/70-lsw-usb.rules:\n  SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{vendor:04x}\", \
         ATTR{{idProduct}}==\"{product:04x}\", TAG+=\"uaccess\"\nthen run `sudo udevadm control --reload` \
         and plug the device in again"
    )]
    Permission {
        path: PathBuf,
        vendor: u16,
        product: u16,
    },
    #[error("cannot open {path}: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} is not plugged into Windows")]
    NotAttached(String),
    #[error(transparent)]
    Qmp(#[from] QmpError),
}

/// Plug `device` into the VM.
pub async fn attach(qmp: &mut Qmp, device: &UsbDevice) -> Result<(), UsbError> {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&device.node)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(UsbError::Permission {
                path: device.node.clone(),
                vendor: device.vendor_id,
                product: device.product_id,
            });
        }
        Err(source) => {
            return Err(UsbError::Open {
                path: device.node.clone(),
                source,
            });
        }
    }
    qmp.execute(
        "device_add",
        Some(json!({
            "driver": "usb-host",
            "id": device.qemu_id(),
            "bus": "xhci.0",
            "hostbus": device.bus,
            "hostaddr": device.addr,
        })),
    )
    .await?;
    Ok(())
}

/// Unplug a device previously attached with [`attach`].
pub async fn detach(qmp: &mut Qmp, device: &UsbDevice) -> Result<(), UsbError> {
    match qmp
        .execute("device_del", Some(json!({ "id": device.qemu_id() })))
        .await
    {
        Ok(_) => Ok(()),
        Err(QmpError::Command { class, .. }) if class == "DeviceNotFound" => {
            Err(UsbError::NotAttached(format!(
                "{:04x}:{:04x}",
                device.vendor_id, device.product_id
            )))
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_permission_error_gives_a_working_udev_rule() {
        let e = UsbError::Permission {
            path: "/dev/bus/usb/001/004".into(),
            vendor: 0x046d,
            product: 0xc52b,
        }
        .to_string();
        assert!(e.contains(r#"ATTR{idVendor}=="046d""#), "{e}");
        assert!(e.contains(r#"ATTR{idProduct}=="c52b""#), "{e}");
        assert!(e.contains("uaccess"), "{e}");
    }

    fn dev(root: &Path, name: &str, fields: &[(&str, &str)]) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        for (k, v) in fields {
            std::fs::write(d.join(k), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn lists_devices_and_skips_hubs() {
        let tmp = tempfile::tempdir().unwrap();
        let sys = tmp.path();
        dev(
            sys,
            "usb1",
            &[
                ("bDeviceClass", "09"),
                ("busnum", "1"),
                ("devnum", "1"),
                ("idVendor", "1d6b"),
                ("idProduct", "0002"),
            ],
        );
        dev(
            sys,
            "1-2",
            &[
                ("bDeviceClass", "00"),
                ("busnum", "1"),
                ("devnum", "5"),
                ("idVendor", "046d"),
                ("idProduct", "c52b"),
                ("manufacturer", "Logitech"),
                ("product", "USB Receiver"),
            ],
        );
        // An interface directory has no busnum: ignored.
        dev(sys, "1-2:1.0", &[("bInterfaceClass", "03")]);
        let list = list(sys, Path::new("/dev/bus/usb"));
        assert_eq!(list.len(), 1);
        let d = &list[0];
        assert_eq!(
            (d.bus, d.addr, d.vendor_id, d.product_id),
            (1, 5, 0x046d, 0xc52b)
        );
        assert_eq!(d.node, Path::new("/dev/bus/usb/001/005"));
        assert_eq!(d.label(), "Logitech USB Receiver (046d:c52b)");
        assert_eq!(d.qemu_id(), "usb-1-5");
        assert_eq!(find(&list, "046d:c52b"), Some(d));
        assert_eq!(find(&list, "1-5"), Some(d));
        assert_eq!(find(&list, "1-6"), None);
        assert_eq!(find(&list, "zz:1"), None);
    }
}
