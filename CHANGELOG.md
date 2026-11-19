# Changelog

## Unreleased

Found by testing on Ubuntu 24.04 and CachyOS (QEMU 11.1, btrfs).

* CachyOS/Arch: shared folders work (`virtiofsd` is found in `/usr/lib`);
  Secure Boot works (`lsw create` enrolls Microsoft's keys with
  `virt-fw-vars`; the package depends on `virt-firmware`); the VM disk is
  created with copy-on-write off on btrfs; missing Desktop/Documents/…
  folders are created instead of silently sharing nothing.
* QEMU 11+: QMP monitors use `-object monitor-qmp` (`-mon` is deprecated).
* Faster unattended installation, most of all on slow disks: the system
  disk uses the host's write-back cache while Windows installs (the VM is
  marked installed only after Windows has shut down and the disk is synced),
  and Windows' automatic BitLocker device encryption, which rewrote the
  whole volume during setup and kept a clear key, is turned off: new
  installations are not encrypted (encrypt the Linux disk instead).
* The VM config is written durably (fsync before and after the rename).
* Window view: it no longer disconnects when Windows changes resolution
  under the pointer; the clipboard keeps working both ways after the window
  opens (it waits for a click or key press before taking the Linux
  clipboard); images copied on Linux paste in every Windows app (bottom-up
  DIB); the Start button explains a missing or uninstalled VM, says when
  Windows stopped while starting, and works again after a shutdown.
* `lsw`: VM names are checked before anything is created (`--name ../x` was
  accepted); `run`, `install`, `launch` and `gpu` say how to create a missing
  VM instead of "No such file or directory" and no longer leave an empty VM
  folder; `launch` reports a supervisor that stopped instead of waiting
  180 s; `create` checks `--source`/`--iso` before downloading, stores the
  ISO path absolute, and creates the VM folder only after its checks pass.
* `lsw doctor` shows how Secure Boot is provided.
* Debian source packages no longer include `target/` when built from a
  compiled tree.

## 0.0.1-alpha.1 (Ubuntu package only)

* The `.deb` installs beside Ubuntu's `suckless-tools` (dmenu), which also
  has a `/usr/bin/lsw`; that tool becomes `lsw.suckless-tools` while LSW is
  installed. The programs are unchanged.
* The README's install steps use the release `.deb`; there is no PPA yet.

## 0.0.1-alpha

First alpha. See the README for what works and the known limits.

* Unattended Windows 11 install (Microsoft download verified by SHA-256,
  Enterprise Evaluation, or your ISO); Secure Boot, TPM 2.0, virtio drivers.
* Coherence: Windows windows as native Wayland windows with native
  move/resize and input.
* Windows apps as Linux launchers (app grid, icons, dock grouping).
* Window and Full Screen views (GTK 4 / libadwaita), dynamic resolution,
  native Full Screen resolution.
* Shared folders, clipboard (text, HTML, images), PipeWire sound, USB
  passthrough, idle pause.
* LSW Tools CD with automatic agent updates.
* GPU tiers B/C plumbing (`lsw gpu`, polkit helper); tier A not included.
* Windows' clock and time zone follow Linux (also after idle pauses).
* Clean shutdown on Ctrl+C, SIGTERM and SIGHUP; `lsw install` stops its VM
  when interrupted.
* Debian packaging, Arch PKGBUILD, CI.
