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
checks it against the SHA-256 Microsoft publishes. Some networks (VPNs,
cloud machines) are refused by Microsoft; then download the ISO in a browser
and use `--source iso --iso Win11.iso`. The Windows account is named after
your Linux user; its password is stored in the VM directory
(`credentials.json`, readable only by you).

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
  images).
* **USB**: `lsw usb list`, then `lsw usb attach 1-4` (or `VID:PID`);
  `lsw usb detach …` gives it back. Your user needs access to the device
  node; the error message says how.
* **Idle**: when no Windows window has been visible for a minute, Windows
  pauses and uses no CPU. Opening an app resumes it instantly.
* **Status**: `lsw ctl status`, screenshots with `lsw ctl screenshot FILE`,
  ACPI shutdown with `lsw ctl powerdown`.

## GPU

`lsw gpu list` shows your GPUs.

* **Tier D (default)**: a 2D display. Office, browsers and most apps run
  well.
* **Tier B**: give a *second* GPU to Windows with `lsw gpu assign
  0000:01:00.0` (asks for your password once; takes effect at the next
  start); `lsw gpu release` gives it back. Needs IOMMU (VT-d / AMD-Vi)
  enabled. Not yet tested on real hardware in this alpha.
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
