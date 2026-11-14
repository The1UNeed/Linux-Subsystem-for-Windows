# LSW — Linux Subsystem for Windows: design

Status: agreed design for v0.0.1-alpha (2026-11-12). This document is the
single source of truth for scope. Anything not listed here is out of scope
for the alpha.

## 1. Product

LSW gives Linux desktop users the experience Parallels Desktop gives Mac users:
Windows 11 runs as a guest, and Windows apps appear on the Linux desktop as
native windows.

| Item | Decision |
|---|---|
| Host | Ubuntu 26.04 LTS (GNOME, tier 1), Ubuntu 24.04 LTS (tier 2), CachyOS KDE Plasma. Wayland first. x86-64 only. |
| Guest | Windows 11 Pro / Enterprise / Education, user's own license. One VM. |
| License | Apache-2.0 for LSW code. GPL components (QEMU, swtpm) run as separate processes. |
| Name | "Linux Subsystem for Windows", identifier `lsw`. Name to be reviewed before 1.0 (trademark risk acknowledged by the owner). |

### Performance priorities (in order)

1. Latency (input-to-photon)
2. Idle power / battery
3. CPU overhead
4. Memory footprint
5. 3D throughput

Every Linux↔Windows crossing (display, input, files, audio, USB) must lose as
little as possible.

### Performance budgets

| Metric | Budget |
|---|---|
| Coherence added latency (guest frame ready → host present) | ≤ 4 ms p95 |
| Paused VM host CPU | 0 % |
| Resume from pause | < 100 ms |
| Resume from saved state (NVMe) | < 5 s |
| Cold start to first Coherence window | < 30 s |
| Shared-folder sequential throughput | ≥ 70 % of native |
| Disk I/O throughput | ≥ 90 % of native |
| Free guest memory returned to host | within 60 s |

## 2. User experience (Parallels parity)

### View modes

All four, as in Parallels: **Coherence** (default after setup), **Window**,
**Full Screen**, **Picture-in-Picture**. `Ctrl+Super+C` cycles modes.

### Coherence

- Windows desktop, wallpaper and taskbar hidden.
- Each Windows top-level window becomes a native Wayland `xdg_toplevel`,
  drawn with **Windows' own title bar and controls**; LSW adds shadows.
- Each Windows app is a **separate Linux app**: own `.desktop` entry, icon,
  dash / task-manager entry, Alt+Tab entry, workspace moves. Launchers are
  always visible (clicking one resumes or cold-starts the VM).
- "Windows Apps" folder in the GNOME app grid / KDE menu, kept in sync.
- Start menu: from a "Windows" launcher entry and from the LSW tray icon,
  rendered as a native floating window.
- Each Windows tray icon becomes its own StatusNotifierItem (option: one
  aggregated icon).
- Windows notifications are mirrored to freedesktop notifications and
  suppressed in the guest.
- Links clicked in Windows open in the Linux default browser (`http`/`https`
  only).
- Windows apps appear in "Open With" for registered file types.
- Drag and drop in both directions. Linux→Windows: shared-folder path or a
  staged copy. Windows→Linux: the agent catches the OLE drag at the window
  edge and copies the file out.
- Clipboard: text, rich text/HTML, images, files.
- No reverse integration (Linux apps are not exposed inside Windows).

### Input and display

- Coherence: Linux keeps its shortcuts (Super, Alt+Tab, workspaces); Windows
  apps get everything else.
- Window / Full Screen: all keys go to Windows while focused (Wayland
  `keyboard-shortcuts-inhibit`), released with `Ctrl+Alt`. *(Changed: see
  "Decisions made during implementation".)*
- Keyboard layout follows Linux.
- One virtual display per Linux monitor with matching resolution, refresh
  rate and (fractional) scale.

### Lifecycle

- Start on demand (launcher click or opening the desktop).
- No visible Windows windows for ~60 s → vCPUs paused (0 % CPU).
- ~15 min paused (shorter on battery) → state saved to disk, QEMU exits.
- Free-page reporting returns guest memory continuously.
- Tier B (GPU passthrough) cannot save state: idle means guest S3 or shutdown.

### Shared folders

- Paired folders (Desktop, Documents, Downloads, Pictures, Music, Videos)
  between Windows known folders and Linux XDG folders, over virtiofs
  (virtiofsd + WinFsp in the guest). Opt-out checkbox at setup.
- Never the whole home, never dotfiles.
- Windows `C:` visible on Linux read-only.

### Setup wizard

1. Host check: CPU virtualization, `/dev/kvm`, IOMMU, GPU tier detection,
   disk space.
2. Windows source: download from Microsoft or use own ISO; Pro edition;
   language.
3. Account: local account named after the Linux user, password stored in the
   Secret Service (libsecret), autologon. MS account can be linked later.
4. Profile: Productivity (8 GB / 4 vCPU / 128 GB), Development (16 / 8 / 256),
   Games (16 / 8 / 256 + passthrough if available); capped at half of host.
   Sparse disk.
5. Integration: paired folders (on), launchers, tray.
6. Unattended install with progress, ending in Coherence.

### Minimum host

16 GB RAM, ≥ 4 cores with VT-x/AMD-V, 64 GB free SSD. IOMMU only for tiers
B/C.

## 3. Architecture

```
 Linux host                                             Windows 11 guest
┌──────────────────────────────────────────────┐      ┌──────────────────────────────┐
│ lsw-app (GTK4/libadwaita)  lsw-coherence     │      │ lsw-guest-agent              │
│   wizard, settings,        (Wayland client:  │      │  service (SYSTEM) +          │
│   Window/FullScreen/PiP    1 toplevel per    │      │  per-user session helper     │
│        │                   Windows window)   │      │  - window tracking + WGC     │
│        └──── sockets ──┐        │            │      │    capture                   │
│                 lsw-daemon (user service)    │      │  - input injection           │
│   VM lifecycle · QMP · devices · idle ·      │      │  - clipboard, apps, icons,   │
│   GPU tier choice · guest channel            │      │    tray, notifications, DnD  │
│        │            │             │          │      │                              │
│      QEMU  ◄──QMP── swtpm   virtiofsd  passt │      │ virtio-win drivers (signed)  │
│        │                                     │      │ GPU driver (tier A, C/C++)   │
└────────┼─────────────────────────────────────┘      └──────────────▲───────────────┘
         │ virtio-serial: control + session + frames (lsw-proto)    │
         │ (ivshmem / blob memory for frames after the alpha)       │
         └───────────────────────────────────────────────────────────┘
```

### Components (Cargo workspace)

| Crate | Role |
|---|---|
| `lsw-proto` | Versioned guest↔host protocol. Length-prefixed frames with strict size limits; decoders have fuzz targets (`fuzz/`) and a randomized test. |
| `lsw-daemon` | Host user service: VM lifecycle, QEMU command line, QMP client, swtpm/virtiofsd/passt supervision, idle/suspend, GPU tier detection, guest channel. |
| `lsw-installer` | Windows ISO acquisition (Microsoft download or user ISO), `autounattend.xml` generation, driver + agent injection. |
| `lsw-cli` | `lsw` command for headless use and tests. |
| `lsw-app` | GTK4/libadwaita app: wizard, settings, Window / Full Screen / PiP views. |
| `lsw-coherence` | Wayland client that maps Windows windows to native toplevels and popups. |
| `lsw-guest-agent` | Windows service + session helper (Rust, `windows` crate). |
| `lsw-helper` | Small polkit-gated privileged helper (VFIO binding, SR-IOV VF creation, hugepages). |

The Windows GPU kernel driver and its Mesa user-mode stack stay C/C++
(forked; decided permanently).

### Virtual machine

KVM + **QEMU driven directly over QMP** (no libvirt). LSW ships pinned builds
of QEMU and virglrenderer under `/usr/lib/lsw/`. Machine: `q35`, OVMF with
Secure Boot (Microsoft keys enrolled), swtpm TPM 2.0, Hyper-V enlightenments,
virtio-blk with iothread and `io_uring`, virtio-net via passt, virtio-balloon
with free-page reporting, virtio-serial (agent ports), virtiofs, xHCI, ICH9
HDA → PipeWire. Every PCI device has a fixed slot so Windows never sees
hardware move when features are toggled.

### Coherence pipeline (zero copy)

1. The guest agent tracks top-level windows (`SetWinEventHook`) and captures
   each one with Windows.Graphics.Capture (no occlusion, no border on
   Windows 11).
2. Only damaged regions are written into a shared-memory ring (ivshmem, or
   virtio-gpu blob memory under tier A).
3. `lsw-coherence` presents each window as a Wayland surface; popups,
   menus and tooltips map to `xdg_popup` relative to their owner; owned
   dialogs become child toplevels.
4. Input goes through QEMU's absolute pointer and keyboard (D-Bus display),
   at the guest window's position.

### GPU tiers (all in v1)

| Tier | Path | Notes |
|---|---|---|
| A | Paravirtualized: virgl first (D3D9–11), Venus later (D3D12/Vulkan) | Default on any host GPU (AMD RADV, Intel ANV, NVIDIA ≥ 570.86). Custom guest driver, test-signed in the alpha (guest Secure Boot off, "Test Mode" watermark) — chosen by the owner as default-on. |
| B | VFIO passthrough of a second GPU | Near-native. Frames shared to the host. |
| C | Intel SR-IOV VF (11th gen+) | Gen 11–13 needs out-of-tree `i915-sriov-dkms`. |
| D | 2D (virtio-gpu / ramfb) | Always available fallback. |

The daemon probes the host at startup and falls back to a lower tier when a
tier is unusable.

GPU driver source: two-week bake-off between osy's Triton (virgl, BSD-3 KMD +
Mesa) and Yttrium (Venus, BSD-3); fork the winner, rebase on upstream
virtio-win, contribute fixes back. Projects without a license (Venera,
vgpu3d) are not used.

### Packaging

Native packages only: Ubuntu `.deb` (PPA) and CachyOS/AUR `PKGBUILD`.
No Flatpak (USB, VFIO, udev and SR-IOV need host integration).

### Signing

Releases: Microsoft attestation signing with an EV certificate (sole
proprietor if Microsoft accepts it, otherwise a company). The alpha uses test
signing.

### Security model

- The guest is **untrusted**. Every guest message is validated, sizes are
  bounded, decoders have fuzz targets, shared memory is bounds-checked.
- File access only within shared folders; drag-out writes only to the drop
  target.
- Only `http`/`https` URLs are opened on the host.
- VMs run unprivileged as the logged-in user; only `lsw-helper` is privileged
  and gated by polkit.
- Networking: NAT via passt by default; bridged optional.
- Windows Update is blocked from replacing LSW's GPU and virtio drivers.
- No telemetry.

### Decisions made during implementation

Each replaces a line of the original design; the reason is the evidence
from running it.

| Decision | Why |
|---|---|
| Guest channel is **virtio-serial**, not vsock. | virtio-win's vsock needs a separately registered Winsock provider with a dynamic address family; virtio-serial is a signed driver installed by the MSI and appears as a plain device file (the transport QEMU's own guest agent uses). vhost-vsock was removed. |
| Two ports: **control** (supervisor) and **session** (UI). | On Wayland only a focused client may own the clipboard, so the UI process speaks for the host desktop directly; no D-Bus relay through the supervisor. |
| The host speaks first (`Attach`), once per (re)open reported by QEMU's `VSERPORT_CHANGE`. | Frames written while the guest has the port closed queue up; periodic retries arrived as a burst of 158 handshakes on the first run. |
| UI ↔ VM uses **QEMU's peer-to-peer D-Bus display** (fd passed over QMP), not a daemon D-Bus service. | No bus exposure; QEMU already provides display, input and resize. |
| Dynamic resolution = `SetUIInfo` (EDID + display event) **plus** an agent hint. | viogpudo learns the size from the EDID but Windows only switches when asked; the agent switches once the mode is listed. |
| Answer file on a **CD**, not a USB stick. | A USB stick is a disk that Windows PE enumerates before the virtio disk, so `DiskID 0` pointed at the stick. |
| swtpm, passt, virtiofsd sockets under `$HOME/.cache/lsw/sock`. | Ubuntu's AppArmor profiles for swtpm and passt deny `/run/user`. |
| Shared folders: virtiofsd in an unprivileged user namespace (identity uid/gid map, needs `newuidmap`); WinFsp directory mount points under `%USERPROFILE%\Linux\`, redirected as known folders. | Verified both ways; files keep the Linux user's ownership. |
| **LSW Tools CD** on every boot; the agent updates itself from it. | Same path installs and updates; no network or store needed. |
| The Windows password is stored in the VM directory (`credentials.json`, 0600), not the Secret Service. | The supervisor runs headless (systemd user unit) and must not depend on an unlocked keyring; Secret Service storage is planned. |
| No read-only view of Windows' `C:` on Linux yet. | Needs a guest-side file server; not in this release. |
| The keyboard follows Windows' own layout setting (scancodes are forwarded), not the Linux layout. | Synchronizing layouts needs a mapping from XKB layouts to Windows keyboard layouts; planned. |
| One virtual display, sized to the first monitor. | Multi-monitor needs one virtio-gpu head per monitor in Coherence and the views; planned. |
| Full Screen is toggled with **Ctrl+Super+F** (not released with Ctrl+Alt). | Ctrl+Alt is a common Windows chord (AltGr on many layouts); Ctrl+Super is free on both systems. |
| Alpha Coherence frames go over a third virtio-serial port (LZ4, damaged regions only). | virtio-win 0.1.302 ships no IVSHMEM driver and the only other build (2021 "prewhql") cannot be relied on to load on current Windows 11. The transport is behind an interface; ivshmem (or tier A blob memory) replaces it when a signed driver is available. |
| The supervisor stops Windows with an ACPI shutdown on Ctrl+C, SIGTERM **and** SIGHUP (the terminal closed), handled from the start of `lsw run` and `lsw install` (SIGHUP not when it is ignored, as under `nohup`); QEMU and its helpers run in their own process group, and the units use `KillMode=mixed`. The power button is pressed every 10 s until Windows is off; in the first 4 s (UEFI firmware) the VM is stopped directly. On SIGHUP output goes to /dev/null, and the supervisor never panics on a failed write. | QEMU quits on SIGINT: a terminal's Ctrl+C reaches the whole process group and systemd's stop signal the whole cgroup, which cut Windows off instead of shutting it down. A booting Windows ignores a single power-button press, and printing to a closed terminal panicked (killing QEMU on unwind). |
| Windows is never idle-paused until the agent is connected (no fixed boot grace). | A Windows that installs updates while shutting down boots, finishes them and powers off by itself; a 3-minute grace paused it mid-update. A reboot closes the agent's port, so the agent counts as gone until it says hello again. |
| Coherence keeps its socket when the guest's Coherence port closes or reopens; it clears its windows and sends a new `Start`. | Dropping the socket looked like a host departure to the guest, which reopened its port: a reconnect loop. |
| A shared folder counts as mounted only when WinFsp's mount point (a reparse point) is there and readable; a previous agent's mount is kept. | A plain folder of the same name passed a readability check, so the share silently wrote to the local disk. Remounting made the folder vanish briefly, which Explorer reports as an error. |
| Windows' clock and time zone follow Linux: the supervisor sends the time and the host's IANA zone when the agent connects and after every idle pause (protocol 2); the agent sets the clock and maps the zone with the ICU built into Windows. | QEMU keeps the virtual clock on Linux local time, and a paused VM's clock stands still; Windows set to UTC was hours off. |
| System shortcuts (Super, Alt+Tab) go to Windows only in Full Screen; in the Window view Linux keeps them. Ctrl+Super+C switches between the Window view and Coherence. | In a window among other Linux windows, taking the desktop's shortcuts away surprised users; Full Screen is where Windows owns the screen. |
| Windows' DPI scale stays at 100 %; the Window view and Full Screen give Windows the monitor's native pixels (fractional scaling included). | Changing Windows' scale needs a sign-out for many apps; native pixels keep the picture sharp. |
| After installation COM1 goes to a 64 KiB ring buffer in QEMU, not a file. | Setup reports progress there; afterwards a guest could fill the host disk through it. |

## 4. Delivery

| Milestone | Content |
|---|---|
| M0 Foundations | Repo, CI, daemon boots Windows 11 (Secure Boot, TPM, virtio) from an auto-downloaded ISO with an unattended install. |
| M1 Window mode | Guest agent over virtio-serial, 2D display, dynamic resolution, clipboard, audio, shared folders, USB passthrough, Full Screen. |
| M2 Coherence | Per-window capture, Wayland client, input, launchers, Start menu, tray, notifications, links, file associations, drag and drop, PiP. |
| M3 GPU | Tier B, tier C, tier A (virgl, then Venus). GPU driver work runs in parallel from M0. |
| M4 Release | Idle/suspend lifecycle, power tuning, packages, signing pipeline, guest auto-update, docs → v0.0.1-alpha. |

### Status of v0.0.1-alpha

Verified means: run on Ubuntu 24.04 (KVM) with Windows 11 26H2 Enterprise
Evaluation (build 26300), with screenshots or recordings as evidence.

| Milestone | Delivered and verified | Delivered, not verified on hardware | Not in this release |
|---|---|---|---|
| M0 | Host check, media download and verification, unattended install (20.8 min), Secure Boot + TPM, virtio drivers. | — | Graphical setup wizard (CLI only). |
| M1 | Agent over virtio-serial, Window view, dynamic resolution, native Full Screen resolution, clipboard both ways, PipeWire audio, shared folders both ways, USB hot-plug (emulated device; the test host has no USB bus). | Passthrough of a physical USB device. | — |
| M2 | Coherence (separate native windows, input, native move/resize via hit test), app launchers with icons (70 written), launch from Linux. | — | Clipboard in Coherence (the Window view has it), Start menu window, tray icons, notifications, links to the Linux browser, file associations, drag and drop, Picture-in-Picture. |
| M3 | — | Tier B (VFIO) and tier C (SR-IOV), experimental: helper, polkit, QEMU arguments, start-up checks (unit-tested on a fake sysfs and kernel; no IOMMU/GPU on the test host). Known gaps: the locked-memory limit must be raised by hand, and an assignment does not survive a host restart (`lsw run` then stops and says what to do). | Tier A: the paravirtualized 3D guest driver (C/C++ fork) is not ready, so tier A is disabled and never turns Secure Boot off. |
| M4 | Idle pause (0 % CPU), LSW Tools auto-update, Debian package (built, installed and run), CI, docs. | Arch package: built, checked and installed in an `archlinux:base-devel` container (QEMU devices present); not run on CachyOS hardware. | Save state to disk when idle (QEMU 8.2 blocks migration with virtiofs), PPA publication, EV signing pipeline (nothing to sign until tier A), bundled pinned QEMU. |

### Testing

- Rust unit tests for protocol, configuration, QEMU arguments, policies and
  parsers (CI on every push).
- Decoders of guest data: cargo-fuzz targets in `fuzz/` (coverage-guided,
  nightly) and a deterministic randomized test over 100,000 mutated inputs
  in the normal test suite.
- Windows boot and integration tests are run on x86-64 KVM hosts (boat.dev
  sandboxes) with the Windows 11 Enterprise Evaluation image, driven by
  scripts and `lsw ctl`; they are not yet automated in CI. The release check
  covers: agent auto-update from the Tools CD; a shared-folder round trip
  (Linux → Windows → Linux) after boot, after an agent restart and after a
  Windows reboot; Coherence across the same three events (under headless
  sway); a clean shutdown on Ctrl+C and on `systemctl --user stop`.
- GPU tiers and latency on hardware with GPUs; a frame-timestamp latency rig
  (planned).

### Release gate

Before v0.0.1-alpha is made public, the whole repository is reviewed by a
council: GPT-6.1 Sol first until it scores 10/10, then Claude Opus 5.5 and
Claude Fable 5.2 independently, each to 10/10. Results go in the release PR.
