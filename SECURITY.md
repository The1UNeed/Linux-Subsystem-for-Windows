# Security

## Model

* **The Windows guest is untrusted.** Everything it sends (control,
  clipboard, Coherence frames, app lists, framebuffer updates) is decoded
  with size limits fixed before allocation, validated field by field, and a
  malformed stream ends the connection. Guest-provided strings never reach
  a shell; launcher files escape them per the Desktop Entry specification.
* **No privileges.** The VM, QEMU (seccomp sandbox), swtpm, passt and
  virtiofsd (its own user namespace) run as the logged-in user. The only
  privileged component is `lsw-helper`, gated by polkit
  (`dev.lsw.helper.gpu`, administrator authentication), which only binds a
  validated non-boot display controller and every function of its PCI slot
  (whose IOMMU group must hold nothing else but bridges) to `vfio-pci`, or
  creates SR-IOV VFs.
* **Local sockets only.** All sockets live in `0700` directories
  (`$XDG_RUNTIME_DIR/lsw`, `~/.cache/lsw/sock`); the display uses QEMU's
  peer-to-peer D-Bus, never the session bus.
* **Shared folders** expose only the paired XDG folders. Each root is
  resolved through symlinks at creation and at every start, and refused if
  it is the home directory or one of its ancestors, or lies inside a hidden
  directory such as `~/.ssh`. Hidden files *inside* a shared folder (for
  example a `.git` in Documents) are visible to Windows like any other file.
* **Downloads** are HTTPS-only from Microsoft, Fedora and GitHub release
  hosts, with every redirect re-checked. The consumer Windows ISO is refused
  unless it matches the SHA-256 on Microsoft's download page; virtio-win and
  WinFsp are pinned by SHA-256 and re-verified every time the cached copy is
  used. Microsoft publishes no checksum for the Enterprise Evaluation ISO,
  so it is trusted on HTTPS from Microsoft alone.
* **Guest-provided app names and ids** are refused (never cleaned) if they
  contain control characters, so they cannot add keys to a launcher file;
  `Exec` arguments are quoted per the Desktop Entry specification.
* **Resource limits** bound what the guest can make the host allocate:
  frame sizes before allocation, at most 128 mirrored windows and 768 MiB of
  window contents, and a bounded queue that slows the guest down instead of
  buffering.
* **Fuzzing.** The decoders of guest data (control/session channel,
  Coherence channel with tile decompression, clipboard reassembly) have
  cargo-fuzz targets in `fuzz/`, and a deterministic randomized test over
  100,000 mutated inputs runs with every `cargo test`.
* **Secrets.** The Windows password lives in the VM directory (`0600`); the
  setup medium that contains it is deleted after installation, and the
  answer-file copies inside Windows are removed by the first-boot script.
* **Secure Boot** stays on (Microsoft keys); without enrolled firmware LSW
  stops instead of silently turning it off (`--no-secure-boot` is an explicit
  choice). LSW never enables test signing unless a test-signed driver it
  ships needs it (none in this release).
* **One supervisor per VM**: an exclusive lock prevents a second `lsw run`
  from touching a running VM's sockets or disk.
* No telemetry.

## Reporting

Please report vulnerabilities privately through GitHub's "Report a
vulnerability" (Security tab) rather than a public issue.
