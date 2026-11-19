//! OVMF firmware discovery.
//!
//! LSW needs a secure-boot capable OVMF build (SMM enabled) and a matching
//! NVRAM template. Two templates matter:
//!
//! * **Microsoft keys enrolled** – Secure Boot on. Used for tiers B–D.
//! * **Blank (setup mode)** – Secure Boot capable but off. Required by the
//!   test-signed tier A GPU driver in the alpha, since Windows refuses
//!   `testsigning` while Secure Boot is enforced.
//!
//! Search order: LSW's bundled firmware first, then distribution paths for
//! Ubuntu/Debian and Arch/CachyOS.

use std::path::{Path, PathBuf};

/// A usable code + vars template pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwarePair {
    pub code: PathBuf,
    pub vars_template: PathBuf,
    pub ms_keys_enrolled: bool,
}

/// Candidate pairs, best first. Paths are relative to `root` so tests can
/// fake a filesystem.
const CANDIDATES: &[(&str, &str, bool)] = &[
    // Bundled with LSW packages.
    (
        "usr/lib/lsw/firmware/OVMF_CODE.secboot.fd",
        "usr/lib/lsw/firmware/OVMF_VARS.ms.fd",
        true,
    ),
    (
        "usr/lib/lsw/firmware/OVMF_CODE.secboot.fd",
        "usr/lib/lsw/firmware/OVMF_VARS.fd",
        false,
    ),
    // Ubuntu / Debian (package `ovmf`).
    (
        "usr/share/OVMF/OVMF_CODE_4M.secboot.fd",
        "usr/share/OVMF/OVMF_VARS_4M.ms.fd",
        true,
    ),
    (
        "usr/share/OVMF/OVMF_CODE_4M.secboot.fd",
        "usr/share/OVMF/OVMF_VARS_4M.fd",
        false,
    ),
    (
        "usr/share/OVMF/OVMF_CODE.secboot.fd",
        "usr/share/OVMF/OVMF_VARS.ms.fd",
        true,
    ),
    // Arch / CachyOS (package `edk2-ovmf`; no pre-enrolled template).
    (
        "usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd",
        "usr/share/edk2/x64/OVMF_VARS.4m.fd",
        false,
    ),
    // Fedora-style layout, also used by some Arch derivatives.
    (
        "usr/share/edk2/ovmf/OVMF_CODE.secboot.fd",
        "usr/share/edk2/ovmf/OVMF_VARS.secboot.fd",
        true,
    ),
    (
        "usr/share/edk2/ovmf/OVMF_CODE.secboot.fd",
        "usr/share/edk2/ovmf/OVMF_VARS.fd",
        false,
    ),
];

/// Find firmware. `want_secure_boot` selects an MS-enrolled template; when
/// false a blank template is preferred (Secure Boot capable but off).
pub fn find(root: &Path, want_secure_boot: bool) -> Option<FirmwarePair> {
    let exists = |rel: &str| root.join(rel).is_file();
    CANDIDATES
        .iter()
        .filter(|(code, vars, _)| exists(code) && exists(vars))
        .find(|(_, _, enrolled)| *enrolled == want_secure_boot)
        .map(|(code, vars, enrolled)| FirmwarePair {
            code: root.join(code),
            vars_template: root.join(vars),
            ms_keys_enrolled: *enrolled,
        })
}

/// Write the VM's NVRAM from a blank template with Microsoft's keys
/// enrolled and Secure Boot on (atomic). For distributions that ship no
/// enrolled template (Arch, CachyOS). `tool` is virt-firmware's
/// `virt-fw-vars`; its Microsoft OEM set is what Windows PCs ship: KEK CA
/// 2011 and 2023, Windows Production PCA 2011, Windows UEFI CA 2023 and the
/// third-party UEFI CAs.
pub fn enroll_microsoft(tool: &Path, blank: &FirmwarePair, dest: &Path) -> anyhow::Result<()> {
    let tmp = dest.with_extension("fd.tmp");
    let out = std::process::Command::new(tool)
        .arg("--input")
        .arg(&blank.vars_template)
        .arg("--output")
        .arg(&tmp)
        .args(["--enroll-microsoft", "--secure-boot"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run {}: {e}", tool.display()))?;
    if !out.status.success() || !tmp.is_file() {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!(
            "{} could not enroll Microsoft's Secure Boot keys: {}",
            tool.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::fs::rename(tmp, dest)?;
    Ok(())
}

/// Replace the VM's NVRAM with a fresh copy of the template (atomic). Used
/// while a VM is being created, so a retried creation never keeps NVRAM
/// from a template with a different Secure Boot state.
pub fn reset_vars(pair: &FirmwarePair, dest: &Path) -> std::io::Result<()> {
    let tmp = dest.with_extension("fd.tmp");
    std::fs::copy(&pair.vars_template, &tmp)?;
    std::fs::rename(tmp, dest)
}

/// Copy the NVRAM template into the VM directory (once; the copy is the
/// VM's persistent UEFI state).
pub fn instantiate_vars(pair: &FirmwarePair, dest: &Path) -> std::io::Result<()> {
    if !dest.exists() {
        std::fs::copy(&pair.vars_template, dest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"fd").unwrap();
    }

    #[test]
    fn ubuntu_layout() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "usr/share/OVMF/OVMF_CODE_4M.secboot.fd");
        touch(root.path(), "usr/share/OVMF/OVMF_VARS_4M.ms.fd");
        touch(root.path(), "usr/share/OVMF/OVMF_VARS_4M.fd");
        let sb = find(root.path(), true).unwrap();
        assert!(sb.ms_keys_enrolled);
        assert!(sb.vars_template.ends_with("OVMF_VARS_4M.ms.fd"));
        let blank = find(root.path(), false).unwrap();
        assert!(!blank.ms_keys_enrolled);
        assert!(blank.vars_template.ends_with("OVMF_VARS_4M.fd"));
    }

    #[test]
    fn arch_has_no_enrolled_template() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd");
        touch(root.path(), "usr/share/edk2/x64/OVMF_VARS.4m.fd");
        assert!(find(root.path(), true).is_none());
        assert!(find(root.path(), false).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn enrolls_microsoft_keys_with_virt_fw_vars() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let blank = tmp.path().join("OVMF_VARS.4m.fd");
        std::fs::write(&blank, b"blank").unwrap();
        // A stand-in for virt-fw-vars: records its arguments, writes --output.
        let tool = tmp.path().join("virt-fw-vars");
        std::fs::write(
            &tool,
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/args\"\nprintf enrolled > \"$4\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pair = FirmwarePair {
            code: tmp.path().join("code.fd"),
            vars_template: blank.clone(),
            ms_keys_enrolled: false,
        };
        let dest = tmp.path().join("vm/OVMF_VARS.fd");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"old").unwrap();
        enroll_microsoft(&tool, &pair, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"enrolled");
        let args = std::fs::read_to_string(tmp.path().join("args")).unwrap();
        assert!(args.contains("--enroll-microsoft --secure-boot"), "{args}");
        assert!(
            args.starts_with(&format!("--input {}", blank.display())),
            "{args}"
        );
        // A failing tool leaves the old NVRAM and no temp file behind.
        std::fs::write(&tool, "#!/bin/sh\necho broken >&2\nexit 1\n").unwrap();
        let err = enroll_microsoft(&tool, &pair, &dest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("broken"), "{err}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"enrolled");
        assert!(!dest.with_extension("fd.tmp").exists());
    }

    #[test]
    fn bundled_wins() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "usr/lib/lsw/firmware/OVMF_CODE.secboot.fd");
        touch(root.path(), "usr/lib/lsw/firmware/OVMF_VARS.ms.fd");
        touch(root.path(), "usr/share/OVMF/OVMF_CODE_4M.secboot.fd");
        touch(root.path(), "usr/share/OVMF/OVMF_VARS_4M.ms.fd");
        assert!(
            find(root.path(), true)
                .unwrap()
                .code
                .starts_with(root.path().join("usr/lib/lsw"))
        );
    }

    #[test]
    fn vars_copied_once() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "usr/share/OVMF/OVMF_CODE_4M.secboot.fd");
        touch(root.path(), "usr/share/OVMF/OVMF_VARS_4M.ms.fd");
        let pair = find(root.path(), true).unwrap();
        let dest = root.path().join("vars.fd");
        instantiate_vars(&pair, &dest).unwrap();
        std::fs::write(&dest, b"modified").unwrap();
        instantiate_vars(&pair, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"modified");
    }

    #[test]
    fn reset_replaces_nvram_from_another_template() {
        let tmp = tempfile::tempdir().unwrap();
        let (blank, enrolled) = (tmp.path().join("blank.fd"), tmp.path().join("ms.fd"));
        std::fs::write(&blank, b"blank").unwrap();
        std::fs::write(&enrolled, b"ms-keys").unwrap();
        let dest = tmp.path().join("OVMF_VARS.fd");
        let pair = |vars: &Path, ms| FirmwarePair {
            code: tmp.path().join("code.fd"),
            vars_template: vars.to_path_buf(),
            ms_keys_enrolled: ms,
        };
        // A first, failed creation copied the blank template …
        instantiate_vars(&pair(&blank, false), &dest).unwrap();
        // … instantiate keeps it, reset replaces it.
        instantiate_vars(&pair(&enrolled, true), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"blank");
        reset_vars(&pair(&enrolled, true), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"ms-keys");
    }
}
