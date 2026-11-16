# Changelog

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
