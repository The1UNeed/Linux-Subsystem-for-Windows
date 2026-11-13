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
    if !dest.exists() {
        eprintln!("Downloading WinFsp {}…", download::WINFSP_VERSION);
        download::download(
            &download::client()?,
            download::WINFSP_URL,
            &dest,
            Some(download::WINFSP_SHA256),
            |_, _| {},
        )
        .await?;
    }
    Ok(dest)
}

/// Agent bytes, build id and WinFsp bytes, if an agent build exists.
pub struct Payload {
    pub agent: Vec<u8>,
    pub build_id: String,
    pub winfsp: Option<Vec<u8>>,
}

impl Payload {
    pub async fn load(cache: &Path) -> anyhow::Result<Option<Self>> {
        let Some(exe) = find_agent_exe() else {
            return Ok(None);
        };
        let agent = std::fs::read(&exe)?;
        let build_id = lsw_proto::agent_build_id(env!("CARGO_PKG_VERSION"), &agent);
        let winfsp = match winfsp_msi(cache).await {
            Ok(p) => Some(std::fs::read(p)?),
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
    let Some(payload) = Payload::load(&cache).await? else {
        eprintln!("note: no lsw-agent.exe found; starting without the LSW Tools CD");
        return Ok(None);
    };
    let iso = paths.tools_iso();
    media::build_tools_media(&iso, payload.as_guest())?;
    tracing::info!("LSW Tools CD with agent {}", payload.build_id);
    Ok(Some(iso))
}
