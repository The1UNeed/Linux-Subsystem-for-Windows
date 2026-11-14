# Building LSW

## Toolchain

`rust-toolchain.toml` pins Rust 1.100 with the `x86_64-pc-windows-gnu`
target; `rustup` picks it up automatically.

Ubuntu 24.04 packages:

```sh
sudo apt install build-essential pkgconf libgtk-4-dev libadwaita-1-dev \
  libxkbcommon-dev gcc-mingw-w64-x86-64
```

Arch / CachyOS: `base-devel gtk4 libadwaita libxkbcommon mingw-w64-gcc`.

## Build

```sh
cargo build --release --workspace                                   # Linux programs
cargo build --release -p lsw-guest-agent --target x86_64-pc-windows-gnu   # Windows agent
cargo test --workspace
```

From a source tree, `lsw` finds the Windows agent next to itself
(`target/x86_64-pc-windows-gnu/release/lsw-agent.exe`); set `LSW_GUEST_DIR`
to use another build. `lsw create` stops if it finds no agent build: a
Windows installed without it would have no Coherence, shared folders or
clipboard.

`cargo build` without `--workspace` builds only the crates that need no
desktop libraries (useful on macOS for the protocol and logic crates).

## Packages

* Debian/Ubuntu: `dpkg-buildpackage -b -us -uc` (see `debian/`).
* Arch/CachyOS: `packaging/arch/PKGBUILD`.

## Crates

| Crate | Role |
|---|---|
| `lsw-proto` | Guest↔host protocol: framing, validation, clipboard chunks, Coherence tiles. |
| `lsw-daemon` | Host library: QEMU command line, QMP, VM supervisor, channels, shares, idle policy, apps, GPU tiers B/C. |
| `lsw-installer` | Windows download and verification, answer file, setup and Tools media. |
| `lsw-cli` | The `lsw` command and VM supervisor (`lsw run`). |
| `lsw-app` | GTK 4 / libadwaita app: Window and Full Screen views. |
| `lsw-coherence` | Wayland client for Coherence. |
| `lsw-guest-agent` | The Windows agent (`lsw-agent.exe`). |
| `lsw-helper` | The polkit-gated privileged helper (VFIO, SR-IOV). |

## Headless testing

`crates/lsw-coherence/examples/virtual_pointer.rs` drives a wlroots
compositor's pointer from stdin, so Coherence can be tested under a headless
`sway` with `grim` screenshots.
