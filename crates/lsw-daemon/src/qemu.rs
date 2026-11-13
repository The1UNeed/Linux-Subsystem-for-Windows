//! QEMU command-line construction.
//!
//! The command line is the contract with QEMU, so it is built in one pure
//! function and covered by tests. Every user-controlled value that lands in
//! a comma-separated option is escaped with [`esc`].

use crate::config::{Network, VmConfig};
use crate::paths::VmPaths;
use lsw_proto::{CONTROL_PORT_NAME, SESSION_PORT_NAME};
use std::path::{Path, PathBuf};

/// Hyper-V enlightenments for Windows guests. These are the set QEMU's
/// documentation recommends for modern Windows; each one removes a class of
/// VM exits or gives Windows a paravirtual clock/IPI/TLB path.
pub const HYPERV_FLAGS: &[&str] = &[
    "hv-relaxed",
    "hv-vapic",
    "hv-spinlocks=0x1fff",
    "hv-vpindex",
    "hv-runtime",
    "hv-time",
    "hv-synic",
    "hv-stimer",
    "hv-stimer-direct",
    "hv-tlbflush",
    "hv-ipi",
    "hv-frequencies",
    "hv-reenlightenment",
];

/// How the guest display leaves QEMU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Display {
    /// No output; screenshots via QMP `screendump` only.
    Headless,
    /// VNC on a Unix socket (debugging).
    VncUnix(PathBuf),
    /// QEMU's D-Bus display in peer-to-peer mode: no bus, the LSW app
    /// connects through QMP `add_client` (Window and Full Screen views).
    DbusP2p,
}

/// Fixed PCI slots on the q35 root complex. Windows identifies devices by
/// location: a device that moves gets reinstalled as a "new" device (stale
/// duplicates in Device Manager, "Network 2", lost settings). Every device
/// therefore has a fixed address that never changes as features are added.
pub mod slot {
    pub const VGA: &str = "0x01";
    pub const DISK: &str = "0x02";
    pub const NET: &str = "0x03";
    pub const BALLOON: &str = "0x04";
    pub const SERIAL: &str = "0x05";
    pub const USB: &str = "0x06";
    pub const SOUND: &str = "0x07";
    /// Shared folders take consecutive slots from here (up to
    /// [`crate::config::MAX_SHARES`]).
    pub const FIRST_SHARE: u8 = 0x10;
}

/// QEMU device ids of the agent ports, as reported in `VSERPORT_CHANGE`.
pub const CONTROL_PORT_ID: &str = "lswctl";
pub const SESSION_PORT_ID: &str = "lswsession";

/// Where guest audio goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audio {
    /// No sound device (headless tests).
    None,
    /// ICH9 HD Audio → PipeWire.
    PipeWire,
}

/// Block I/O backend for the system disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskIo {
    /// O_DIRECT + io_uring: lowest overhead, needs liburing in QEMU and a
    /// filesystem that supports O_DIRECT.
    IoUring,
    /// Portable fallback (page cache + thread pool).
    Threads,
}

/// Options that are not part of the persistent VM configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOptions {
    pub display: Display,
    pub disk_io: DiskIo,
    pub audio: Audio,
    /// LSW Tools CD to attach (agent install and auto-update).
    pub tools_iso: Option<PathBuf>,
    /// Enable QEMU's seccomp sandbox.
    pub sandbox: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            display: Display::Headless,
            disk_io: DiskIo::IoUring,
            audio: Audio::None,
            tools_iso: None,
            sandbox: true,
        }
    }
}

/// Escape a value for QEMU's comma-separated option syntax (`,` → `,,`).
pub fn esc(value: &str) -> String {
    value.replace(',', ",,")
}

fn p(path: &Path) -> String {
    esc(&path.to_string_lossy())
}

/// Locally administered, stable MAC derived from the VM name.
pub fn mac_for(name: &str) -> String {
    // FNV-1a: tiny, deterministic, good enough to keep MACs stable per VM.
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    let b = h.to_be_bytes();
    format!("52:54:00:{:02x}:{:02x}:{:02x}", b[1], b[2], b[3])
}

/// Build the full argument vector (without the binary itself).
pub fn build_args(cfg: &VmConfig, paths: &VmPaths, opts: &LaunchOptions) -> Vec<String> {
    let mut a: Vec<String> = Vec::with_capacity(96);
    let mut push = |args: &[&str]| a.extend(args.iter().map(|s| (*s).to_owned()));

    let mem = cfg.memory_mib;
    let cpus = cfg.vcpus;

    push(&[
        "-name",
        &format!("guest=lsw-{},debug-threads=on", esc(&cfg.name)),
    ]);
    push(&["-nodefaults", "-no-user-config", "-msg", "timestamp=on"]);
    push(&[
        "-machine",
        "q35,accel=kvm,smm=on,vmport=off,hpet=off,memory-backend=mem0",
    ]);
    push(&[
        "-object",
        // memfd + share=on lets virtiofsd map guest memory (shared folders).
        &format!("memory-backend-memfd,id=mem0,size={mem}M,share=on"),
    ]);
    push(&["-m", &format!("{mem}M")]);
    push(&["-cpu", &format!("host,{}", HYPERV_FLAGS.join(","))]);
    push(&["-smp", &format!("{cpus},sockets=1,cores={cpus},threads=1")]);
    push(&["-rtc", "base=localtime,driftfix=slew"]);
    push(&["-global", "kvm-pit.lost_tick_policy=delay"]);
    // LSW pauses vCPUs instead of guest sleep; S3/S4 inside the VM only
    // cause trouble (and break GPU passthrough).
    push(&[
        "-global",
        "ICH9-LPC.disable_s3=1",
        "-global",
        "ICH9-LPC.disable_s4=1",
    ]);

    // UEFI with SMM-protected variables (required for Secure Boot).
    push(&["-global", "driver=cfi.pflash01,property=secure,value=on"]);
    push(&[
        "-drive",
        &format!(
            "if=pflash,format=raw,unit=0,readonly=on,file={}",
            p(&cfg.firmware.code)
        ),
    ]);
    push(&[
        "-drive",
        &format!("if=pflash,format=raw,unit=1,file={}", p(&cfg.firmware.vars)),
    ]);

    // TPM 2.0 through swtpm.
    push(&[
        "-chardev",
        &format!("socket,id=chrtpm,path={}", p(&paths.tpm_socket())),
    ]);
    push(&["-tpmdev", "emulator,id=tpm0,chardev=chrtpm"]);
    push(&["-device", "tpm-crb,tpmdev=tpm0"]);

    // System disk: raw, virtio-blk with a dedicated iothread.
    let io = match opts.disk_io {
        DiskIo::IoUring => "cache=none,aio=io_uring",
        DiskIo::Threads => "cache=writeback,aio=threads",
    };
    push(&["-object", "iothread,id=io0"]);
    push(&[
        "-drive",
        &format!(
            "file={},if=none,id=disk0,format=raw,{io},discard=unmap,detect-zeroes=unmap",
            p(&cfg.disk_path)
        ),
    ]);
    let queues = cpus.clamp(1, 8);
    push(&[
        "-device",
        &format!(
            "virtio-blk-pci,drive=disk0,iothread=io0,num-queues={queues},bootindex=1,bus=pcie.0,addr={}",
            slot::DISK
        ),
    ]);

    // USB: xHCI with an absolute pointer (needed until the guest agent owns input).
    push(&[
        "-device",
        &format!("qemu-xhci,id=xhci,p2=8,p3=8,bus=pcie.0,addr={}", slot::USB),
    ]);
    push(&["-device", "usb-tablet,bus=xhci.0"]);

    // Installation media.
    if let Some(install) = &cfg.install {
        push(&[
            "-drive",
            &format!(
                "file={},if=none,id=cdwin,media=cdrom,readonly=on",
                p(&install.windows_iso)
            ),
        ]);
        push(&["-device", "ide-cd,drive=cdwin,bus=ide.0,bootindex=0"]);
        push(&[
            "-drive",
            &format!(
                "file={},if=none,id=cdvirtio,media=cdrom,readonly=on",
                p(&install.virtio_win_iso)
            ),
        ]);
        push(&["-device", "ide-cd,drive=cdvirtio,bus=ide.1"]);
        // Answer file + first-boot payload on a CD, never a disk: see
        // `lsw_installer::media` for why.
        push(&[
            "-drive",
            &format!(
                "file={},if=none,id=cdsetup,media=cdrom,readonly=on",
                p(&install.setup_iso)
            ),
        ]);
        push(&["-device", "ide-cd,drive=cdsetup,bus=ide.2"]);
    }

    // LSW Tools CD, present on every boot.
    if let Some(tools) = &opts.tools_iso {
        push(&[
            "-drive",
            &format!(
                "file={},if=none,id=cdtools,media=cdrom,readonly=on",
                p(tools)
            ),
        ]);
        push(&["-device", "ide-cd,drive=cdtools,bus=ide.3"]);
    }

    // Network.
    match cfg.network {
        Network::Passt => {
            push(&[
                "-netdev",
                &format!(
                    "stream,id=net0,server=off,addr.type=unix,addr.path={}",
                    p(&paths.passt_socket())
                ),
            ]);
            push(&[
                "-device",
                &format!(
                    "virtio-net-pci,netdev=net0,mac={},bus=pcie.0,addr={}",
                    mac_for(&cfg.name),
                    slot::NET
                ),
            ]);
        }
        Network::User => {
            push(&["-netdev", "user,id=net0"]);
            push(&[
                "-device",
                &format!(
                    "virtio-net-pci,netdev=net0,mac={},bus=pcie.0,addr={}",
                    mac_for(&cfg.name),
                    slot::NET
                ),
            ]);
        }
        Network::None => {}
    }

    // Memory returned to the host continuously.
    push(&[
        "-device",
        &format!(
            "virtio-balloon-pci,id=balloon0,free-page-reporting=on,bus=pcie.0,addr={}",
            slot::BALLOON
        ),
    ]);

    // Guest agent control channel: a virtio-serial port. Windows opens it
    // as a plain device (`\\.\Global\<name>`) through the signed vioserial
    // driver, the same transport QEMU's own guest agent uses.
    push(&[
        "-device",
        &format!(
            "virtio-serial-pci,id=vser0,bus=pcie.0,addr={}",
            slot::SERIAL
        ),
    ]);
    push(&[
        "-chardev",
        &format!(
            "socket,id=agent,path={},server=on,wait=off",
            p(&paths.agent_socket())
        ),
    ]);
    push(&[
        "-device",
        &format!(
            "virtserialport,bus=vser0.0,chardev=agent,id={CONTROL_PORT_ID},name={CONTROL_PORT_NAME}"
        ),
    ]);
    push(&[
        "-chardev",
        &format!(
            "socket,id=session,path={},server=on,wait=off",
            p(&paths.session_socket())
        ),
    ]);
    push(&[
        "-device",
        &format!(
            "virtserialport,bus=vser0.0,chardev=session,id={SESSION_PORT_ID},name={SESSION_PORT_NAME}"
        ),
    ]);

    // Shared folders: one vhost-user-fs device per virtiofsd.
    for (i, share) in cfg.shares.iter().enumerate() {
        push(&[
            "-chardev",
            &format!("socket,id=fs{i},path={}", p(&paths.virtiofs_socket(i))),
        ]);
        push(&[
            "-device",
            &format!(
                "vhost-user-fs-pci,chardev=fs{i},tag={},queue-size=1024,bus=pcie.0,addr={:#04x}",
                share.tag,
                slot::FIRST_SHARE + i as u8
            ),
        ]);
    }

    // Sound: HD Audio, which Windows drives with its inbox driver. Output
    // only unless the user enabled the microphone.
    if opts.audio == Audio::PipeWire {
        push(&["-audiodev", "pipewire,id=snd0"]);
        push(&[
            "-device",
            &format!("ich9-intel-hda,id=hda0,bus=pcie.0,addr={}", slot::SOUND),
        ]);
        let codec = if cfg.microphone {
            "hda-duplex"
        } else {
            "hda-output"
        };
        push(&["-device", &format!("{codec},bus=hda0.0,audiodev=snd0")]);
    }

    // Display (tier D baseline; higher tiers add their devices in M3).
    push(&[
        "-device",
        &format!(
            "virtio-vga,id=video0,xres=1280,yres=800,bus=pcie.0,addr={}",
            slot::VGA
        ),
    ]);
    match &opts.display {
        Display::Headless => push(&["-display", "none"]),
        Display::VncUnix(sock) => push(&["-display", "none", "-vnc", &format!("unix:{}", p(sock))]),
        Display::DbusP2p => push(&["-display", "dbus,p2p=yes,gl=off"]),
    }

    // Serial console: first-boot scripts report progress on COM1.
    push(&[
        "-chardev",
        &format!("file,id=ser0,path={}", p(&paths.serial_log())),
    ]);
    push(&["-device", "isa-serial,chardev=ser0"]);

    // Control.
    push(&[
        "-chardev",
        &format!(
            "socket,id=qmp,path={},server=on,wait=off",
            p(&paths.qmp_socket())
        ),
    ]);
    push(&["-mon", "chardev=qmp,mode=control"]);
    // Second monitor for `lsw ctl` and tests while the supervisor owns the first.
    push(&[
        "-chardev",
        &format!(
            "socket,id=qmpctl,path={},server=on,wait=off",
            p(&paths.ctl_socket())
        ),
    ]);
    push(&["-mon", "chardev=qmpctl,mode=control"]);
    // Third monitor for the UI (passes the display socket with `add_client`).
    push(&[
        "-chardev",
        &format!(
            "socket,id=qmpui,path={},server=on,wait=off",
            p(&paths.ui_qmp_socket())
        ),
    ]);
    push(&["-mon", "chardev=qmpui,mode=control"]);
    push(&["-pidfile", &p(&paths.qemu_pidfile())]);
    push(&["-boot", "menu=off,strict=on"]);

    if opts.sandbox {
        push(&[
            "-sandbox",
            "on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny",
        ]);
    }
    a
}

/// Arguments for swtpm (TPM 2.0 emulator) serving `paths.tpm_socket()`.
pub fn swtpm_args(cfg: &VmConfig, paths: &VmPaths) -> Vec<String> {
    vec![
        "socket".into(),
        "--tpm2".into(),
        "--tpmstate".into(),
        format!("dir={}", p(&cfg.tpm_state_dir)),
        "--ctrl".into(),
        format!("type=unixio,path={}", p(&paths.tpm_socket())),
        "--flags".into(),
        "startup-clear".into(),
        "--terminate".into(),
    ]
}

/// Arguments for one virtiofsd: unprivileged, in its own user namespace
/// with an identity mapping for the user, so files the guest creates belong
/// to the Linux user.
pub fn virtiofsd_args(shared_dir: &Path, socket: &Path, uid: u32, gid: u32) -> Vec<String> {
    vec![
        format!("--socket-path={}", socket.display()),
        format!("--shared-dir={}", shared_dir.display()),
        "--sandbox=namespace".into(),
        format!("--uid-map=:{uid}:{uid}:1:"),
        format!("--gid-map=:{gid}:{gid}:1:"),
        "--cache=auto".into(),
        "--inode-file-handles=never".into(),
        "--announce-submounts".into(),
        "--log-level=warn".into(),
    ]
}

/// Arguments for passt serving `paths.passt_socket()` in the foreground.
pub fn passt_args(paths: &VmPaths) -> Vec<String> {
    vec![
        "--foreground".into(),
        "--quiet".into(),
        "--socket".into(),
        paths.passt_socket().to_string_lossy().into_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Firmware, GpuTier, InstallMedia};

    fn cfg(dir: &Path) -> VmConfig {
        VmConfig {
            name: "win".into(),
            memory_mib: 8192,
            vcpus: 4,
            disk_path: dir.join("disk.raw"),
            disk_size_gib: 128,
            firmware: Firmware {
                code: "/fw/CODE.fd".into(),
                vars: dir.join("VARS.fd"),
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

    fn joined(args: &[String]) -> String {
        args.join(" ")
    }

    #[test]
    fn baseline_windows_11_machine() {
        let dir = Path::new("/data/win");
        let paths = VmPaths::with_bases(
            Path::new("/data"),
            Path::new("/run/user/1000"),
            Path::new("/home/u/.cache/lsw/sock"),
            "win",
        );
        let args = build_args(&cfg(dir), &paths, &LaunchOptions::default());
        let s = joined(&args);
        assert!(s.contains("-machine q35,accel=kvm,smm=on"));
        assert!(s.contains("memory-backend-memfd,id=mem0,size=8192M,share=on"));
        assert!(s.contains("-cpu host,hv-relaxed,"));
        assert!(s.contains("-smp 4,sockets=1,cores=4,threads=1"));
        assert!(s.contains("driver=cfi.pflash01,property=secure,value=on"));
        assert!(s.contains("if=pflash,format=raw,unit=0,readonly=on,file=/fw/CODE.fd"));
        assert!(s.contains("-device tpm-crb,tpmdev=tpm0"));
        assert!(s.contains("virtio-blk-pci,drive=disk0,iothread=io0,num-queues=4,bootindex=1"));
        assert!(s.contains("cache=none,aio=io_uring"));
        assert!(s.contains("-device virtio-serial-pci,id=vser0"));
        assert!(
            s.contains("socket,id=agent,path=/run/user/1000/lsw/win/agent.sock,server=on,wait=off")
        );
        assert!(
            s.contains("virtserialport,bus=vser0.0,chardev=agent,id=lswctl,name=org.lsw.control.0")
        );
        assert!(s.contains(
            "virtserialport,bus=vser0.0,chardev=session,id=lswsession,name=org.lsw.session.0"
        ));
        assert!(
            s.contains("socket,id=qmpui,path=/run/user/1000/lsw/win/ui.sock,server=on,wait=off")
        );
        assert!(!s.contains("vsock"));
        assert!(!s.contains("vhost-user-fs"));
        assert!(!s.contains("-audiodev"));
        assert!(s.contains("virtio-balloon-pci,id=balloon0,free-page-reporting=on"));
        assert!(s.contains("path=/run/user/1000/lsw/win/qmp.sock,server=on,wait=off"));
        assert!(s.contains("addr.path=/home/u/.cache/lsw/sock/win/passt.sock"));
        assert!(s.contains("-display none"));
        assert!(s.contains("-sandbox on,"));
        assert!(!s.contains("ide-cd"), "no install media after setup");
    }

    #[test]
    fn install_media_attached_and_cd_boots_first() {
        let dir = Path::new("/data/win");
        let paths = VmPaths::with_bases(
            Path::new("/data"),
            Path::new("/run"),
            Path::new("/home/u/.cache/lsw/sock"),
            "win",
        );
        let mut c = cfg(dir);
        c.install = Some(InstallMedia {
            windows_iso: "/isos/win11.iso".into(),
            virtio_win_iso: "/isos/virtio-win.iso".into(),
            setup_iso: "/data/win/setup.iso".into(),
        });
        let s = joined(&build_args(&c, &paths, &LaunchOptions::default()));
        assert!(s.contains("file=/isos/win11.iso,if=none,id=cdwin,media=cdrom,readonly=on"));
        assert!(s.contains("ide-cd,drive=cdwin,bus=ide.0,bootindex=0"));
        assert!(s.contains("ide-cd,drive=cdvirtio,bus=ide.1"));
        assert!(s.contains("file=/data/win/setup.iso,if=none,id=cdsetup,media=cdrom,readonly=on"));
        assert!(s.contains("ide-cd,drive=cdsetup,bus=ide.2"));
        // The only disk in the VM is the system disk.
        assert!(!s.contains("usb-storage"));
    }

    #[test]
    fn commas_in_paths_are_escaped() {
        let paths = VmPaths::with_bases(
            Path::new("/d"),
            Path::new("/r"),
            Path::new("/home/u/.cache/lsw/sock"),
            "win",
        );
        let mut c = cfg(Path::new("/d"));
        c.disk_path = "/home/me/odd,name/disk.raw".into();
        let s = joined(&build_args(&c, &paths, &LaunchOptions::default()));
        assert!(s.contains("file=/home/me/odd,,name/disk.raw,if=none"));
    }

    #[test]
    fn optional_devices_follow_options() {
        let paths = VmPaths::with_bases(
            Path::new("/d"),
            Path::new("/r"),
            Path::new("/home/u/.cache/lsw/sock"),
            "win",
        );
        let mut c = cfg(Path::new("/d"));
        c.network = Network::None;
        let opts = LaunchOptions {
            audio: Audio::None,
            tools_iso: Some("/d/tools.iso".into()),
            sandbox: false,
            disk_io: DiskIo::Threads,
            display: Display::VncUnix("/r/vnc.sock".into()),
        };
        let s = joined(&build_args(&c, &paths, &opts));
        assert!(!s.contains("virtio-net"));
        assert!(!s.contains("-sandbox"));
        assert!(s.contains("cache=writeback,aio=threads"));
        assert!(s.contains("-vnc unix:/r/vnc.sock"));
        assert!(s.contains("file=/d/tools.iso,if=none,id=cdtools,media=cdrom,readonly=on"));
        assert!(s.contains("ide-cd,drive=cdtools,bus=ide.3"));
    }

    #[test]
    fn shares_audio_and_dbus_display() {
        use crate::config::Share;
        use lsw_proto::KnownFolder;
        let paths = VmPaths::with_bases(
            Path::new("/d"),
            Path::new("/r"),
            Path::new("/home/u/.cache/lsw/sock"),
            "win",
        );
        let mut c = cfg(Path::new("/d"));
        c.shares = vec![
            Share {
                tag: "lsw-documents".into(),
                host_path: "/home/u/Documents".into(),
                folder: KnownFolder::Documents,
            },
            Share {
                tag: "lsw-music".into(),
                host_path: "/home/u/Music".into(),
                folder: KnownFolder::Music,
            },
        ];
        let opts = LaunchOptions {
            audio: Audio::PipeWire,
            display: Display::DbusP2p,
            ..LaunchOptions::default()
        };
        let s = joined(&build_args(&c, &paths, &opts));
        assert!(s.contains("socket,id=fs0,path=/home/u/.cache/lsw/sock/win/fs0.sock"));
        assert!(s.contains(
            "vhost-user-fs-pci,chardev=fs1,tag=lsw-music,queue-size=1024,bus=pcie.0,addr=0x11"
        ));
        assert!(s.contains("-audiodev pipewire,id=snd0"));
        assert!(s.contains("hda-output,bus=hda0.0,audiodev=snd0"));
        assert!(s.contains("-display dbus,p2p=yes,gl=off"));
        c.microphone = true;
        let s = joined(&build_args(&c, &paths, &opts));
        assert!(s.contains("hda-duplex,bus=hda0.0,audiodev=snd0"));
    }

    #[test]
    fn virtiofsd_is_sandboxed_with_identity_mapping() {
        let a = joined(&virtiofsd_args(
            Path::new("/home/u/Documents"),
            Path::new("/home/u/.cache/lsw/sock/win/fs0.sock"),
            1000,
            1001,
        ));
        assert!(a.contains("--shared-dir=/home/u/Documents"));
        assert!(a.contains("--sandbox=namespace"));
        assert!(a.contains("--uid-map=:1000:1000:1: --gid-map=:1001:1001:1:"));
    }

    #[test]
    fn every_pci_device_has_a_fixed_unique_slot() {
        let paths = VmPaths::with_bases(
            Path::new("/d"),
            Path::new("/r"),
            Path::new("/home/u/.cache/lsw/sock"),
            "win",
        );
        let opts = LaunchOptions {
            audio: Audio::PipeWire,
            ..LaunchOptions::default()
        };
        let args = build_args(&cfg(Path::new("/d")), &paths, &opts);
        let mut slots = Vec::new();
        for pair in args.windows(2).filter(|w| w[0] == "-device") {
            let dev = &pair[1];
            let is_pci = ["-pci,", "virtio-vga", "qemu-xhci", "ich9-intel-hda"]
                .iter()
                .any(|k| dev.contains(k));
            if is_pci {
                let addr = dev
                    .split(',')
                    .find_map(|kv| kv.strip_prefix("addr="))
                    .unwrap_or_else(|| panic!("{dev} has no fixed address"));
                assert!(dev.contains("bus=pcie.0"), "{dev}");
                slots.push(addr.to_owned());
            }
        }
        let n = slots.len();
        slots.sort();
        slots.dedup();
        assert_eq!(slots.len(), n, "duplicate slots");
        assert!(n >= 7);
    }

    #[test]
    fn mac_is_stable_and_local() {
        assert_eq!(mac_for("win"), mac_for("win"));
        assert_ne!(mac_for("win"), mac_for("win2"));
        assert!(mac_for("win").starts_with("52:54:00:"));
    }

    #[test]
    fn helper_args() {
        let paths = VmPaths::with_bases(
            Path::new("/d"),
            Path::new("/r"),
            Path::new("/home/u/.cache/lsw/sock"),
            "win",
        );
        let c = cfg(Path::new("/d/lsw/vms/win"));
        let t = joined(&swtpm_args(&c, &paths));
        assert!(t.starts_with("socket --tpm2 --tpmstate dir=/d/lsw/vms/win/tpm"));
        assert!(t.contains("type=unixio,path=/home/u/.cache/lsw/sock/win/tpm.sock"));
        assert!(
            joined(&passt_args(&paths)).contains("--socket /home/u/.cache/lsw/sock/win/passt.sock")
        );
    }
}
