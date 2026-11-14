# LSW user guide

## Views

| View | How | What you see |
|---|---|---|
| Coherence | Open any Windows app from the app grid, or `lsw-coherence` | Each Windows window as a Linux window. |
| Window | Open **Windows** from the app grid (`lsw-app`) | The Windows desktop in a resizable window. |
| Full Screen | **Ctrl+Super+F** in the LSW window | The Windows desktop on the whole monitor; every key goes to Windows. Press Ctrl+Super+F again to leave. |

Windows' own title bars and buttons work as usual in every view.

## Setting up

```sh
lsw doctor
```

lists what your machine has (virtualization, KVM, memory, GPUs) and anything
that blocks LSW, with the fix.

```sh
lsw create [--profile productivity|development|games] [--source consumer|evaluation|iso --iso FILE]
```

prepares the VM. By default it downloads Windows 11 from Microsoft and
refuses it unless it matches the SHA-256 Microsoft publishes
(`--source evaluation` downloads the 90-day Enterprise Evaluation, for which
Microsoft publishes no checksum). Some networks (VPNs,
cloud machines) are refused by Microsoft; then download the ISO in a browser
and use `--source iso --iso Win11.iso`. The Windows account is named after
your Linux user; its password is stored in the VM directory
(`credentials.json`, readable only by you).

Secure Boot needs UEFI firmware with Microsoft's keys enrolled (Ubuntu's
`ovmf` package). Where it is missing (Arch's `edk2-ovmf`), `lsw create` stops
and explains; `--no-secure-boot` creates the VM with Secure Boot off.

Profiles: Productivity 8 GB / 4 cores / 128 GB, Development and Games
16 GB / 8 cores / 256 GB, never more than half of your machine. The disk is a
sparse file: it only takes the space Windows uses.

```sh
lsw install
```

installs Windows without questions and reports progress. When it finishes,
Windows has the virtio drivers, the LSW agent and WinFsp, and is shut down.

Keep Windows supervised with the systemd user unit:

```sh
systemctl --user enable --now lsw@windows
```

or run it in a terminal with `lsw run` (Ctrl+C shuts Windows down cleanly).

## Everyday use

* **Apps**: every Windows app is in the app grid ("Windows Apps" folder on
  GNOME). `lsw apps sync` refreshes the launchers after you install apps in
  Windows; it also happens automatically each time Windows starts.
* **Files**: Desktop, Documents, Downloads, Pictures, Music and Videos are
  the same folders on both sides. Turn this off at creation with
  `--no-shared-folders`.
* **Clipboard**: copy on one side, paste on the other (text, formatted text,
  images) while the Window or Full Screen view is open; not yet in Coherence.
* **USB**: `lsw usb list`, then `lsw usb attach 1-4` (or `VID:PID`);
  `lsw usb detach …` gives it back. Your user needs access to the device
  node; the error message says how.
* **Idle**: when no Windows window has been visible for a minute, Windows
  pauses and uses no CPU. Opening an app resumes it instantly. It never
  pauses while Windows starts, restarts or installs updates (until the LSW
  agent is running).
* **Status**: `lsw ctl status`, screenshots with `lsw ctl screenshot FILE`,
  ACPI shutdown with `lsw ctl powerdown`.
* **Clock**: Windows takes its time and time zone from Linux when it
  starts and after every idle pause.
* **Switching views**: **Ctrl+Super+C** switches between the Window view
  and Coherence; **Ctrl+Super+F** toggles Full Screen.

## GPU

`lsw gpu list` shows your GPUs.

* **Tier D (default)**: a 2D display. Office, browsers and most apps run
  well.
* **Tier B (experimental)**: give a *second* GPU to Windows with `lsw gpu
  assign 0000:01:00.0` (asks for your password once; takes effect at the
  next start); `lsw gpu release` gives it back. Every function of the GPU's
  slot goes with it, and its IOMMU group must hold nothing else. Needs IOMMU
  (VT-d / AMD-Vi) enabled. Not yet tested on real hardware in this alpha,
  and two things are still manual:
  * VFIO locks all of Windows' memory, so the locked-memory limit of your
    session must allow it: as root, create
    `/etc/systemd/system/user@.service.d/memlock.conf` with
    `[Service]` and `LimitMEMLOCK=infinity`, then log in again.
  * The assignment does not survive a host restart: run `lsw gpu assign`
    again. Until then `lsw run` stops with that advice instead of starting.
* **Tier C**: Intel SR-IOV virtual functions (11th gen and later; gen 11–13
  need the out-of-tree `i915-sriov-dkms`). Same commands, with a VF address.
* **Tier A (paravirtualized 3D)**: not in this release.

## Troubleshooting

| Symptom | Look at |
|---|---|
| `lsw run` fails at start | `~/.local/share/lsw/vms/windows/qemu.log`, `swtpm.log`, `passt.log` |
| A shared folder is missing | `virtiofsd-<tag>.log` in the same directory; `lsw doctor` (needs `virtiofsd` and `newuidmap`) |
| Inside Windows | `C:\ProgramData\LSW\agent.log`, `firstboot.log`, `install-agent.log` |
| The agent does not connect | It starts at logon; check `C:\ProgramData\LSW\agent.log` |

## Privacy

LSW sends nothing anywhere. The only downloads are Windows (from
Microsoft), virtio-win (from Fedora) and WinFsp (from GitHub), each checked
against a pinned or published SHA-256.
