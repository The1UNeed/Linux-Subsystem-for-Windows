//! The setup medium: a small FAT image attached as a USB stick during
//! installation. Windows setup picks up `autounattend.xml` from the root of
//! removable media; the `lsw\` folder carries the first-boot script, its
//! configuration and (from M1) the guest agent.

use serde::Serialize;
use std::io::Write;
use std::path::Path;

/// Image size. FAT16 needs at least ~16 MiB; 64 MiB leaves room for the agent.
pub const IMAGE_SIZE: u64 = 64 * 1024 * 1024;

/// The first-boot script, embedded at build time.
pub const FIRSTBOOT_PS1: &str = include_str!("../assets/firstboot.ps1");

/// Settings read by `firstboot.ps1` from `lsw\config.json`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FirstBootConfig {
    /// Enable Windows test signing (tier A GPU driver in the alpha).
    pub test_signing: bool,
}

/// A file to place on the medium: `(path with '/' separators, contents)`.
pub type MediaFile<'a> = (&'a str, &'a [u8]);

/// Build the FAT image at `path` (replacing any previous one).
pub fn build_image(path: &Path, files: &[MediaFile<'_>]) -> anyhow::Result<()> {
    let tmp = path.with_extension("img.tmp");
    {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.set_len(IMAGE_SIZE)?;
        fatfs::format_volume(
            &mut file,
            fatfs::FormatVolumeOptions::new()
                .volume_label(*b"LSWSETUP   ")
                .fat_type(fatfs::FatType::Fat16),
        )?;
        let fs = fatfs::FileSystem::new(&mut file, fatfs::FsOptions::new())?;
        {
            let root = fs.root_dir();
            for (name, data) in files {
                anyhow::ensure!(
                    !name.is_empty()
                        && !name.starts_with('/')
                        && !name
                            .split('/')
                            .any(|c| c.is_empty() || c == "." || c == ".."),
                    "invalid media path {name:?}"
                );
                let (dirs, leaf) = match name.rsplit_once('/') {
                    Some((d, l)) => (Some(d), l),
                    None => (None, *name),
                };
                let dir = match dirs {
                    Some(d) => {
                        let mut cur = root.clone();
                        for part in d.split('/') {
                            cur = match cur.open_dir(part) {
                                Ok(existing) => existing,
                                Err(_) => cur.create_dir(part)?,
                            };
                        }
                        cur
                    }
                    None => root.clone(),
                };
                let mut f = dir.create_file(leaf)?;
                f.truncate()?;
                f.write_all(data)?;
                f.flush()?;
            }
        }
        fs.unmount()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Build the standard LSW setup medium.
pub fn build_setup_media(
    path: &Path,
    autounattend_xml: &str,
    config: &FirstBootConfig,
    agent_exe: Option<&[u8]>,
) -> anyhow::Result<()> {
    let config_json = serde_json::to_vec_pretty(config)?;
    // Windows PowerShell 5.1 reads BOM-less scripts as ANSI; keep it ASCII.
    anyhow::ensure!(FIRSTBOOT_PS1.is_ascii(), "firstboot.ps1 must be ASCII");
    let mut files: Vec<MediaFile<'_>> = vec![
        ("autounattend.xml", autounattend_xml.as_bytes()),
        ("lsw/firstboot.ps1", FIRSTBOOT_PS1.as_bytes()),
        ("lsw/config.json", &config_json),
    ];
    if let Some(exe) = agent_exe {
        files.push(("lsw/agent/lsw-agent.exe", exe));
    }
    build_image(path, &files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn read_back(img: &Path, path: &str) -> Vec<u8> {
        let mut file = std::fs::File::open(img).unwrap();
        let fs = fatfs::FileSystem::new(&mut file, fatfs::FsOptions::new()).unwrap();
        let mut f = fs.root_dir().open_file(path).unwrap();
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn image_contains_files_with_long_names() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("setup.img");
        build_setup_media(
            &img,
            "<unattend/>",
            &FirstBootConfig { test_signing: true },
            Some(b"MZ"),
        )
        .unwrap();
        assert_eq!(std::fs::metadata(&img).unwrap().len(), IMAGE_SIZE);
        assert_eq!(read_back(&img, "autounattend.xml"), b"<unattend/>");
        assert_eq!(
            read_back(&img, "lsw/firstboot.ps1"),
            FIRSTBOOT_PS1.as_bytes()
        );
        assert_eq!(read_back(&img, "lsw/agent/lsw-agent.exe"), b"MZ");
        let cfg: serde_json::Value =
            serde_json::from_slice(&read_back(&img, "lsw/config.json")).unwrap();
        assert_eq!(cfg["test_signing"], true);
        // The long name is stored as such (Windows setup matches it by name).
        let mut file = std::fs::File::open(&img).unwrap();
        let fs = fatfs::FileSystem::new(&mut file, fatfs::FsOptions::new()).unwrap();
        let names: Vec<String> = fs
            .root_dir()
            .iter()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(names.contains(&"autounattend.xml".to_owned()), "{names:?}");
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("x.img");
        assert!(build_image(&img, &[("../evil", b"x")]).is_err());
        assert!(build_image(&img, &[("/abs", b"x")]).is_err());
    }

    #[test]
    fn firstboot_reports_completion_marker() {
        assert!(FIRSTBOOT_PS1.contains(crate::unattend::INSTALL_DONE_MARKER));
        assert!(FIRSTBOOT_PS1.is_ascii());
    }
}
