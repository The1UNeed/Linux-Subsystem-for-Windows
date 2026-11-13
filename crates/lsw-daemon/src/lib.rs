//! LSW host service.
//!
//! M0 scope: host probing, VM configuration, QEMU command line, QMP control
//! and the process supervisor used to boot and install Windows 11.

pub mod channel;
pub mod config;
pub mod firmware;
pub mod host;
pub mod paths;
pub mod qemu;
pub mod qmp;
pub mod session;
pub mod vm;
