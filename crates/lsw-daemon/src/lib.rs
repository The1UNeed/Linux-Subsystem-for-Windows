//! LSW host service.
//!
//! M0 scope: host probing, VM configuration, QEMU command line, QMP control
//! and the process supervisor used to boot and install Windows 11.

pub mod channel;
pub mod coherence;
pub mod config;
pub mod display;
pub mod firmware;
pub mod host;
pub mod keymap;
pub mod paths;
pub mod qemu;
pub mod qmp;
pub mod session;
pub mod usb;
pub mod vm;
