//! The setup medium: a small read-only CD image (ISO 9660 + Joliet) attached
//! during installation. Windows Setup picks up `autounattend.xml` from the
//! root of removable read-only media; the `lsw\` folder carries the
//! first-boot script, its configuration and (from M1) the guest agent.
//!
//! A CD rather than a USB stick: a USB stick is a *disk*, and Windows PE
//! enumerates it before the virtio disk whose driver it loads later, so the
//! answer file's `DiskID 0` would point at the stick. With the setup files on
//! a CD the virtio system disk is the only disk in the VM.

use hadris_iso::joliet::JolietLevel;
use hadris_iso::read::PathSeparator;
use hadris_iso::write::options::{BaseIsoLevel, CreationFeatures, IsoFormatOptions};
use hadris_iso::write::{File as IsoFile, InputFiles, IsoImageWriter};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

/// Volume label shown in Windows.
pub const VOLUME_LABEL: &str = "LSWSETUP";

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

/// Insert `contents` at `path` into a directory tree.
fn insert(tree: &mut Vec<IsoFile>, path: &[&str], contents: &[u8]) -> anyhow::Result<()> {
    let (first, rest) = path.split_first().expect("non-empty path");
    if rest.is_empty() {
        anyhow::ensure!(
            !tree.iter().any(|f| *f.name() == *first),
            "duplicate media path component {first:?}"
        );
        tree.push(IsoFile::File {
            name: Arc::new((*first).to_owned()),
            contents: contents.to_vec(),
        });
        return Ok(());
    }
    let existing = tree.iter_mut().find_map(|f| match f {
        IsoFile::Directory { name, children } if **name == *first => Some(children),
        _ => None,
    });
    match existing {
        Some(children) => insert(children, rest, contents),
        None => {
            let mut children = Vec::new();
            insert(&mut children, rest, contents)?;
            tree.push(IsoFile::Directory {
                name: Arc::new((*first).to_owned()),
                children,
            });
            Ok(())
        }
    }
}

/// Build the ISO image at `path` (replacing any previous one atomically).
pub fn build_image(path: &Path, files: &[MediaFile<'_>]) -> anyhow::Result<()> {
    let mut tree = Vec::new();
    for (name, data) in files {
        let parts: Vec<&str> = name.split('/').collect();
        anyhow::ensure!(
            !name.is_empty()
                && !parts
                    .iter()
                    .any(|c| c.is_empty() || *c == "." || *c == ".." || c.contains('\\')),
            "invalid media path {name:?}"
        );
        insert(&mut tree, &parts, data)?;
    }

    let options = IsoFormatOptions {
        volume_name: VOLUME_LABEL.to_owned(),
        sector_size: 2048,
        path_separator: PathSeparator::ForwardSlash,
        features: CreationFeatures {
            filenames: BaseIsoLevel::Level2 {
                supports_lowercase: false,
                supports_rrip: false,
            },
            // Windows reads the Joliet tree: real, lower-case long names.
            joliet: Some(JolietLevel::Level3),
            ..CreationFeatures::default()
        },
        system_id: None,
        volume_set_id: None,
        publisher_id: None,
        preparer_id: None,
        application_id: Some("LSW".to_owned()),
        strict_charset: false,
    };

    let tmp = path.with_extension("iso.tmp");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    let input = InputFiles {
        path_separator: PathSeparator::ForwardSlash,
        files: tree,
    };
    let file = IsoImageWriter::create(file, input, options)
        .map_err(|e| anyhow::anyhow!("building setup ISO: {e}"))?;
    file.sync_all()?;
    drop(file);
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
    use hadris_iso::file::EntryType;
    use hadris_iso::read::IsoImage;
    use std::io::BufReader;

    /// Read `path` back through the Joliet tree, as Windows does.
    fn read_back(iso: &Path, path: &str) -> Option<Vec<u8>> {
        let image = IsoImage::open(BufReader::new(std::fs::File::open(iso).unwrap())).unwrap();
        let root = image
            .root_dirs()
            .iter()
            .find(|r| matches!(r.entry_type(), EntryType::Joliet { .. }))
            .expect("Joliet tree present")
            .dir_ref();
        let mut dir = image.open_dir(root);
        let parts: Vec<&str> = path.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            let entry = dir.entries().filter_map(Result::ok).find(|e| {
                !e.is_special() && e.record.joliet_name().trim_end_matches(";1") == *part
            })?;
            if i + 1 == parts.len() {
                return Some(image.read_file(&entry).unwrap());
            }
            dir = image.open_dir(entry.as_dir_ref(&image).unwrap());
        }
        None
    }

    #[test]
    fn image_contains_files_with_long_lowercase_names() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("setup.iso");
        build_setup_media(
            &iso,
            "<unattend/>",
            &FirstBootConfig { test_signing: true },
            Some(b"MZ"),
        )
        .unwrap();
        assert_eq!(read_back(&iso, "autounattend.xml").unwrap(), b"<unattend/>");
        assert_eq!(
            read_back(&iso, "lsw/firstboot.ps1").unwrap(),
            FIRSTBOOT_PS1.as_bytes()
        );
        assert_eq!(read_back(&iso, "lsw/agent/lsw-agent.exe").unwrap(), b"MZ");
        let cfg: serde_json::Value =
            serde_json::from_slice(&read_back(&iso, "lsw/config.json").unwrap()).unwrap();
        assert_eq!(cfg["test_signing"], true);
        assert!(read_back(&iso, "missing.txt").is_none());
        assert!(!dir.path().join("setup.iso.tmp").exists());
    }

    #[test]
    fn rejects_bad_paths() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("x.iso");
        assert!(build_image(&iso, &[("../evil", b"x")]).is_err());
        assert!(build_image(&iso, &[("/abs", b"x")]).is_err());
        assert!(build_image(&iso, &[("a\\b", b"x")]).is_err());
        assert!(build_image(&iso, &[("a", b"x"), ("a", b"y")]).is_err());
    }

    #[test]
    fn firstboot_reports_completion_marker() {
        assert!(FIRSTBOOT_PS1.contains(crate::unattend::INSTALL_DONE_MARKER));
        assert!(FIRSTBOOT_PS1.is_ascii());
    }
}
