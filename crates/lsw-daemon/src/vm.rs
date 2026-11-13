//! VM supervisor: prepares the disk, launches swtpm / passt / QEMU, connects
//! QMP and tears everything down again.

use crate::config::{Network, Share, VmConfig};
use crate::host::Binaries;
use crate::paths::VmPaths;
use crate::qemu::{self, LaunchOptions};
use crate::qmp::{Qmp, QmpError};
use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::{Child, Command};

const HELPER_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
const QMP_TIMEOUT: Duration = Duration::from_secs(20);

/// Exclusive lock on a VM, held by its supervisor (`lsw run`, `lsw install`)
/// for its whole lifetime: a second supervisor must not touch the sockets
/// or disk of a running VM.
pub fn lock_vm(paths: &VmPaths) -> anyhow::Result<std::fs::File> {
    paths.create()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.data.join("supervisor.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("this VM is already running (another `lsw run` or `lsw install` owns it)")
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Create the raw system disk as a sparse file (no space used until Windows
/// writes). An existing disk is never touched.
pub fn create_sparse_disk(path: &Path, size_gib: u64) -> std::io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.set_len(size_gib * 1024 * 1024 * 1024)?;
    Ok(true)
}

/// A VM that is not running.
#[derive(Debug, Clone)]
pub struct Vm {
    pub cfg: VmConfig,
    pub paths: VmPaths,
}

/// A running VM and its helper processes. Dropping it kills everything.
pub struct Running {
    pub qmp: Qmp,
    qemu: Child,
    swtpm: Child,
    passt: Option<Child>,
    virtiofsd: Vec<Child>,
    pub qemu_log: PathBuf,
    /// Shares whose virtiofsd started (announced to the guest agent).
    pub shares: Vec<Share>,
}

/// Effective uid and gid of this process, from `/proc/self/status` (the
/// workspace forbids `unsafe`, so no `geteuid()`).
pub fn current_ids() -> Option<(u32, u32)> {
    parse_effective_ids(&std::fs::read_to_string("/proc/self/status").ok()?)
}

/// `Uid:`/`Gid:` lines list real, effective, saved and filesystem ids.
fn parse_effective_ids(status: &str) -> Option<(u32, u32)> {
    let field = |name: &str| -> Option<u32> {
        let line = status.lines().find(|l| l.starts_with(name))?;
        line.split_whitespace().nth(2)?.parse().ok()
    };
    Some((field("Uid:")?, field("Gid:")?))
}

/// How long a helper must stay alive after creating its socket to count as
/// started (virtiofsd creates the socket before it sets up its sandbox and
/// exits if that fails).
const HELPER_SETTLE: Duration = Duration::from_millis(300);

impl Vm {
    pub fn new(cfg: VmConfig, paths: VmPaths) -> Self {
        Self { cfg, paths }
    }

    /// Launch the VM. `bins` must contain QEMU and swtpm; passt is used when
    /// configured and present (otherwise the caller should have picked
    /// [`Network::User`]).
    pub async fn start(&self, bins: &Binaries, opts: &LaunchOptions) -> anyhow::Result<Running> {
        let qemu_bin = bins.qemu.as_ref().context("qemu-system-x86_64 not found")?;
        let swtpm_bin = bins.swtpm.as_ref().context("swtpm not found")?;
        self.cfg.validate()?;
        self.paths.create()?;
        for sock in [
            self.paths.qmp_socket(),
            self.paths.ctl_socket(),
            self.paths.tpm_socket(),
            self.paths.passt_socket(),
        ] {
            let _ = std::fs::remove_file(sock);
        }
        std::fs::create_dir_all(&self.cfg.tpm_state_dir)?;
        create_sparse_disk(&self.cfg.disk_path, self.cfg.disk_size_gib)?;

        let swtpm = spawn_logged(
            swtpm_bin,
            &qemu::swtpm_args(&self.cfg, &self.paths),
            &self.paths.data.join("swtpm.log"),
        )?;
        wait_for_socket(&self.paths.tpm_socket(), HELPER_SOCKET_TIMEOUT)
            .await
            .context("swtpm did not create its socket")?;

        let passt = match (self.cfg.network, &bins.passt) {
            (Network::Passt, Some(bin)) => {
                let child = spawn_logged(
                    bin,
                    &qemu::passt_args(&self.paths),
                    &self.paths.data.join("passt.log"),
                )?;
                wait_for_socket(&self.paths.passt_socket(), HELPER_SOCKET_TIMEOUT)
                    .await
                    .context("passt did not create its socket")?;
                Some(child)
            }
            (Network::Passt, None) => bail!("network is set to passt but passt is not installed"),
            _ => None,
        };

        let (virtiofsd, shares) = self.start_virtiofsd(bins).await;
        let mut cfg = self.cfg.clone();
        cfg.shares = shares.clone();

        let args = qemu::build_args(&cfg, &self.paths, opts);
        tracing::info!(target: "lsw::qemu", "launching {} {}", qemu_bin.display(), args.join(" "));
        let qemu_log = self.paths.qemu_log();
        let mut qemu = spawn_logged(qemu_bin, &args, &qemu_log)?;

        let qmp = match Qmp::connect_with_retry(&self.paths.qmp_socket(), QMP_TIMEOUT).await {
            Ok(q) => q,
            Err(e) => {
                let status = qemu.try_wait().ok().flatten();
                let _ = qemu.start_kill();
                bail!(
                    "QEMU did not come up ({e}); exit status {status:?}; log tail:\n{}",
                    tail(&qemu_log, 40)
                );
            }
        };
        tracing::info!(
            "QEMU {}.{}.{} is up",
            qmp.version.0,
            qmp.version.1,
            qmp.version.2
        );
        Ok(Running {
            qmp,
            qemu,
            swtpm,
            passt,
            virtiofsd,
            qemu_log,
            shares,
        })
    }

    /// Start one virtiofsd per configured share. A share whose daemon does
    /// not come up is left out (logged) instead of failing the whole VM.
    async fn start_virtiofsd(&self, bins: &Binaries) -> (Vec<Child>, Vec<Share>) {
        let mut children = Vec::new();
        let mut active = Vec::new();
        if self.cfg.shares.is_empty() {
            return (children, active);
        }
        let (Some(bin), Some(_), Some((uid, gid))) =
            (&bins.virtiofsd, &bins.newuidmap, current_ids())
        else {
            tracing::warn!("virtiofsd or newuidmap missing: shared folders are off");
            return (children, active);
        };
        for share in &self.cfg.shares {
            // Index in the *active* list: it names the socket and the device.
            let socket = self.paths.virtiofs_socket(active.len());
            let _ = std::fs::remove_file(&socket);
            // Checked again at every start: the folder may have become a
            // symlink to the home directory since the VM was created.
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            let root = match crate::config::share_root(&share.host_path, &home) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("share {}: not exported: {e}", share.tag);
                    continue;
                }
            };
            let args = qemu::virtiofsd_args(&root, &socket, uid, gid);
            let log = self.paths.data.join(format!("virtiofsd-{}.log", share.tag));
            let child = match spawn_logged(bin, &args, &log) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("share {}: {e:#}", share.tag);
                    continue;
                }
            };
            let mut child = child;
            let started = match wait_for_socket(&socket, HELPER_SOCKET_TIMEOUT).await {
                Ok(()) => {
                    tokio::time::sleep(HELPER_SETTLE).await;
                    match child.try_wait() {
                        Ok(None) => Ok(()),
                        Ok(Some(status)) => Err(format!("exited with {status}")),
                        Err(e) => Err(e.to_string()),
                    }
                }
                Err(e) => Err(e.to_string()),
            };
            match started {
                Ok(()) => {
                    children.push(child);
                    active.push(share.clone());
                }
                Err(e) => {
                    let _ = child.start_kill();
                    let _ = std::fs::remove_file(&socket);
                    tracing::warn!(
                        "share {}: virtiofsd did not start ({e}): {}",
                        share.tag,
                        tail(&log, 3)
                    );
                }
            }
        }
        (children, active)
    }
}

/// Something happened to a running VM.
pub enum Activity {
    /// QEMU exited.
    Exited(std::io::Result<ExitStatus>),
    /// A QMP event arrived.
    Event(Result<crate::qmp::Event, QmpError>),
}

impl Running {
    /// Wait for QEMU to exit or for the next QMP event. Cancel-safe.
    pub async fn next_activity(&mut self) -> Activity {
        tokio::select! {
            status = self.qemu.wait() => Activity::Exited(status),
            event = self.qmp.next_event() => Activity::Event(event),
        }
    }

    /// Freeze all vCPUs (0 % host CPU).
    pub async fn pause(&mut self) -> Result<(), QmpError> {
        self.qmp.execute("stop", None).await.map(|_| ())
    }

    pub async fn resume(&mut self) -> Result<(), QmpError> {
        self.qmp.execute("cont", None).await.map(|_| ())
    }

    /// Press `key` every `period` for `duration`. Used to get past the
    /// Windows ISO's "Press any key to boot from CD or DVD" prompt without
    /// rebuilding the ISO.
    pub async fn press_key_repeatedly(&mut self, key: &str, period: Duration, duration: Duration) {
        let deadline = tokio::time::Instant::now() + duration;
        while tokio::time::Instant::now() < deadline {
            if let Err(e) = self.qmp.send_key(key).await {
                tracing::debug!("send-key failed: {e}");
            }
            tokio::time::sleep(period).await;
        }
    }

    /// Whether QEMU has exited, without blocking.
    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.qemu.try_wait().ok().flatten()
    }

    /// Wait for QEMU to exit.
    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.qemu.wait().await
    }

    /// Ask Windows to shut down (ACPI power button); force-quit QEMU if it
    /// has not exited after `grace`.
    pub async fn shutdown(mut self, grace: Duration) -> anyhow::Result<ExitStatus> {
        // A paused guest (idle) cannot react to the power button: run it.
        if let Err(e) = self.qmp.execute("cont", None).await {
            tracing::warn!("cont before shutdown failed: {e}");
        }
        if let Err(e) = self.qmp.execute("system_powerdown", None).await {
            tracing::warn!("system_powerdown failed: {e}");
        }
        match tokio::time::timeout(grace, self.qemu.wait()).await {
            Ok(status) => {
                self.reap_helpers().await;
                Ok(status?)
            }
            Err(_) => {
                tracing::warn!("guest did not power off within {grace:?}; quitting QEMU");
                self.kill().await
            }
        }
    }

    /// Stop QEMU immediately (no guest shutdown).
    pub async fn kill(mut self) -> anyhow::Result<ExitStatus> {
        let _ = self.qmp.execute("quit", None).await;
        let status = match tokio::time::timeout(Duration::from_secs(5), self.qemu.wait()).await {
            Ok(s) => s?,
            Err(_) => {
                self.qemu.start_kill()?;
                self.qemu.wait().await?
            }
        };
        self.reap_helpers().await;
        Ok(status)
    }

    async fn reap_helpers(&mut self) {
        // swtpm exits by itself (`--terminate`) once QEMU disconnects.
        if tokio::time::timeout(Duration::from_secs(3), self.swtpm.wait())
            .await
            .is_err()
        {
            let _ = self.swtpm.start_kill();
        }
        if let Some(p) = &mut self.passt {
            let _ = p.start_kill();
            let _ = p.wait().await;
        }
        // virtiofsd exits when QEMU disconnects; make sure.
        for v in &mut self.virtiofsd {
            if tokio::time::timeout(Duration::from_secs(2), v.wait())
                .await
                .is_err()
            {
                let _ = v.start_kill();
            }
        }
    }
}

fn spawn_logged(bin: &Path, args: &[String], log: &Path) -> anyhow::Result<Child> {
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    let err_file = log_file.try_clone()?;
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(err_file))
        .kill_on_drop(true)
        // Own process group: a terminal's Ctrl+C reaches only `lsw`, which
        // shuts Windows down; QEMU would otherwise quit on the same SIGINT.
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {}", bin.display()))
}

async fn wait_for_socket(path: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while !path.exists() {
        if tokio::time::Instant::now() >= deadline {
            bail!("{} did not appear within {timeout:?}", path.display());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// Last `lines` lines of a log file (for error messages).
pub fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Poll `path` (the serial console log) until a line contains `marker`.
/// Returns that line. First-boot scripts in the guest report progress this
/// way before the guest agent exists.
pub async fn wait_for_serial_marker(
    path: &Path,
    marker: &str,
    timeout: Duration,
    mut on_line: impl FnMut(&str),
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut seen = 0usize;
    loop {
        if let Ok(bytes) = tokio::fs::read(path).await {
            // The serial log is guest-controlled: decode lossily, never trust it.
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            // A partial last line is re-read on the next poll.
            let complete = if text.ends_with('\n') {
                lines.len()
            } else {
                lines.len().saturating_sub(1)
            };
            for line in &lines[seen.min(complete)..complete] {
                let clean = sanitize_console_line(line);
                on_line(&clean);
                if clean.contains(marker) {
                    return Ok(clean);
                }
            }
            seen = complete;
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("serial marker {marker:?} not seen within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Make one guest console line safe to print: drop ANSI/VT escape sequences
/// (the firmware clears the screen with them) and other control characters,
/// and cap the length.
pub fn sanitize_console_line(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                // CSI: parameters/intermediates up to a final byte @..~.
                chars.next();
                for f in chars.by_ref() {
                    if ('@'..='~').contains(&f) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else if !c.is_control() {
            out.push(c);
            if out.chars().count() >= 512 {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_supervisor_per_vm() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = VmPaths::with_bases(
            &tmp.path().join("d"),
            &tmp.path().join("r"),
            &tmp.path().join("h"),
            "win",
        );
        let first = lock_vm(&paths).unwrap();
        assert!(lock_vm(&paths).is_err());
        drop(first);
        assert!(lock_vm(&paths).is_ok());
    }

    #[test]
    fn sparse_disk_is_created_once() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk.raw");
        assert!(create_sparse_disk(&disk, 64).unwrap());
        assert_eq!(std::fs::metadata(&disk).unwrap().len(), 64 << 30);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Sparse: almost no blocks allocated.
            assert!(std::fs::metadata(&disk).unwrap().blocks() < 1024);
        }
        assert!(!create_sparse_disk(&disk, 128).unwrap());
        assert_eq!(std::fs::metadata(&disk).unwrap().len(), 64 << 30);
    }

    #[tokio::test]
    async fn serial_marker_found_and_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("serial.log");
        let writer = {
            let log = log.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                tokio::fs::write(&log, b"LSW_STAGE drivers\r\n\x1b[2JLSW_INSTALL_DONE ok\n")
                    .await
                    .unwrap();
            })
        };
        let mut lines = Vec::new();
        let hit = wait_for_serial_marker(&log, "LSW_INSTALL_DONE", Duration::from_secs(5), |l| {
            lines.push(l.to_owned())
        })
        .await
        .unwrap();
        writer.await.unwrap();
        assert_eq!(hit, "LSW_INSTALL_DONE ok");
        assert_eq!(lines[0], "LSW_STAGE drivers");
    }

    #[tokio::test]
    async fn serial_marker_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let r = wait_for_serial_marker(
            &dir.path().join("none.log"),
            "X",
            Duration::from_millis(50),
            |_| {},
        )
        .await;
        assert!(r.is_err());
    }

    #[test]
    fn console_lines_are_sanitized() {
        assert_eq!(
            sanitize_console_line("\u{1b}[2J\u{1b}[01;01H\u{1b}[=3hBdsDxe: boot\u{7}"),
            "BdsDxe: boot"
        );
        assert_eq!(sanitize_console_line(&"x".repeat(2000)).len(), 512);
    }

    #[test]
    fn effective_ids_are_parsed() {
        let status = "Name:\tlsw\nUid:\t1000\t1001\t1000\t1001\nGid:\t1000\t994\t1000\t994\n";
        assert_eq!(parse_effective_ids(status), Some((1001, 994)));
        assert_eq!(parse_effective_ids("Name: x\n"), None);
    }

    #[test]
    fn tail_returns_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("log");
        std::fs::write(&f, "a\nb\nc\n").unwrap();
        assert_eq!(tail(&f, 2), "b\nc");
        assert_eq!(tail(&dir.path().join("missing"), 2), "");
    }
}
