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
  (`dev.lsw.helper.gpu`, administrator authentication), which only binds
  validated display controllers to `vfio-pci` or creates SR-IOV VFs.
* **Local sockets only.** All sockets live in `0700` directories
  (`$XDG_RUNTIME_DIR/lsw`, `~/.cache/lsw/sock`); the display uses QEMU's
  peer-to-peer D-Bus, never the session bus.
* **Shared folders** expose only the paired XDG folders, never the home
  directory or dotfiles.
* **Downloads** are HTTPS-only from Microsoft, Fedora and GitHub release
  hosts (every redirect re-checked) and verified by SHA-256 (published or
  pinned).
* **Secrets.** The Windows password lives in the VM directory (`0600`); the
  setup medium that contains it is deleted after installation, and the
  answer-file copies inside Windows are removed by the first-boot script.
* **Secure Boot** stays on (Microsoft keys); LSW never enables test signing
  unless a test-signed driver it ships needs it (none in this release).
* No telemetry.

## Reporting

Please report vulnerabilities privately through GitHub's "Report a
vulnerability" (Security tab) rather than a public issue.
