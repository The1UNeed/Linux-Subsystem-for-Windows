//! Guest software: the Windows agent build and WinFsp, packed onto the setup
//! medium and the LSW Tools CD.

use lsw_daemon::paths::VmPaths;
use lsw_installer::{download, media};
use std::path::{Path, PathBuf};

/// Where the Windows agent build is looked for, in order:
/// `$LSW_GUEST_DIR`, the packaged `/usr/lib/lsw/guest`, then the
/// cross-compiled build next to this binary in a source tree
/// (`target/x86_64-pc-windows-gnu/release`).
pub fn find_agent_exe() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("LSW_GUEST_DIR") {
        dirs.push(d.into());
    }
    dirs.push("/usr/lib/lsw/guest".into());
    if let Some(target) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent()?.parent().map(Path::to_path_buf))
    {
        dirs.push(target.join("x86_64-pc-windows-gnu/release"));
    }
    dirs.into_iter()
        .map(|d| d.join("lsw-agent.exe"))
        .find(|p| p.is_file())
}

/// `$XDG_DATA_HOME` (the VM data directory is `<it>/lsw/vms/<name>`).
pub fn data_home(paths: &VmPaths) -> anyhow::Result<PathBuf> {
    paths
        .data
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("unexpected data directory layout"))
}

/// Start an LSW program in the background: as a transient systemd user
/// unit when available (survives this process, logs to the journal),
/// otherwise as a plain child. The binary is looked up next to `lsw`.
pub fn start_detached(program: &str, args: &[&str], unit: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(program)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(program));
    let systemd = std::process::Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet", "--unit", unit])
        .arg(&exe)
        .args(args)
        .status();
    match systemd {
        Ok(s) if s.success() => Ok(()),
        _ => std::process::Command::new(&exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ()),
    }
}

/// Whether no Coherence client holds the VM's Coherence lock.
pub fn coherence_idle(paths: &VmPaths) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.coherence_lock())
    else {
        return true;
    };
    file.try_lock().is_ok()
}

/// Put LSW launchers into a "Windows Apps" folder in the GNOME app grid
/// (no-op elsewhere or when it already exists).
pub fn gnome_app_folder() {
    let get = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.app-folders", "folder-children"])
        .output();
    let Ok(out) = get else { return };
    if !out.status.success() {
        return;
    }
    let current = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let path = "/org/gnome/desktop/app-folders/folders/lsw-windows/";
    let schema = format!("org.gnome.desktop.app-folders.folder:{path}");
    let set = |args: &[&str]| {
        let _ = std::process::Command::new("gsettings").args(args).status();
    };
    set(&["set", &schema, "name", "Windows Apps"]);
    set(&["set", &schema, "categories", "['X-LSW-Windows']"]);
    if !current.contains("'lsw-windows'") {
        let list = if current.starts_with("@as []") || current == "[]" {
            "['lsw-windows']".to_owned()
        } else {
            current.replacen(']', ", 'lsw-windows']", 1)
        };
        set(&[
            "set",
            "org.gnome.desktop.app-folders",
            "folder-children",
            &list,
        ]);
    }
}

/// Shared download cache (`$XDG_DATA_HOME/lsw/cache`).
pub fn cache_dir(paths: &VmPaths) -> anyhow::Result<PathBuf> {
    let cache = paths
        .data
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("unexpected data directory layout"))?
        .join("cache");
    std::fs::create_dir_all(&cache)?;
    Ok(cache)
}

/// The pinned WinFsp MSI, downloaded once into the cache.
pub async fn winfsp_msi(cache: &Path) -> anyhow::Result<PathBuf> {
    let dest = cache.join(format!("winfsp-{}.msi", download::WINFSP_VERSION));
    download::ensure_pinned(
        &download::client()?,
        download::WINFSP_URL,
        &dest,
        download::WINFSP_SHA256,
        |_, _| {},
    )
    .await?;
    Ok(dest)
}

/// Agent bytes, build id and WinFsp bytes, if an agent build exists.
pub struct Payload {
    pub agent: Vec<u8>,
    pub build_id: String,
    pub winfsp: Option<Vec<u8>>,
}

impl Payload {
    /// `require_winfsp`: fail instead of warning when WinFsp cannot be
    /// fetched (setup media must be complete; a Tools CD can be partial).
    pub async fn load(cache: &Path, require_winfsp: bool) -> anyhow::Result<Option<Self>> {
        let Some(exe) = find_agent_exe() else {
            return Ok(None);
        };
        let agent = std::fs::read(&exe)?;
        let build_id = lsw_proto::agent_build_id(env!("CARGO_PKG_VERSION"), &agent);
        let winfsp = match winfsp_msi(cache).await {
            Ok(p) => Some(std::fs::read(p)?),
            Err(e) if require_winfsp => {
                return Err(e.context("WinFsp (needed by shared folders) could not be downloaded"));
            }
            Err(e) => {
                eprintln!("warning: WinFsp unavailable ({e:#}); shared folders will not work");
                None
            }
        };
        Ok(Some(Self {
            agent,
            build_id,
            winfsp,
        }))
    }

    pub fn as_guest(&self) -> media::GuestPayload<'_> {
        media::GuestPayload {
            agent_exe: &self.agent,
            build_id: &self.build_id,
            winfsp_msi: self.winfsp.as_deref(),
        }
    }
}

/// Build the LSW Tools CD for this boot; `None` when no agent build exists.
pub async fn prepare_tools(paths: &VmPaths) -> anyhow::Result<Option<PathBuf>> {
    let cache = cache_dir(paths)?;
    let Some(payload) = Payload::load(&cache, false).await? else {
        eprintln!("note: no lsw-agent.exe found; starting without the LSW Tools CD");
        return Ok(None);
    };
    let iso = paths.tools_iso();
    media::build_tools_media(&iso, payload.as_guest())?;
    tracing::info!("LSW Tools CD with agent {}", payload.build_id);
    Ok(Some(iso))
}
