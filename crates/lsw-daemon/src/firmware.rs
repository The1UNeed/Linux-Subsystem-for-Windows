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
}
