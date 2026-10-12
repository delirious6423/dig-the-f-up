//! Free-space check before a model download.
//!
//! Uses `statvfs(3)` (unix, via the `libc` crate, which is already in the lock file through the Tauri stack), so no
//! new download-time dependency and no parsing of `df` output. On other platforms the check is skipped: the free
//! space is reported as unknown and the download proceeds (Windows support is not verified yet).

use crate::{ModelFile, DISK_MARGIN};
use anyhow::{bail, Result};
use std::path::Path;

const GIB: f64 = (1u64 << 30) as f64;

/// Bytes still to download for `files` into `dir`: a finished file counts as 0, a `.part` counts as already done.
pub fn remaining_bytes(dir: &Path, files: &[ModelFile]) -> u64 {
    files
        .iter()
        .map(|file| {
            let final_len = std::fs::metadata(dir.join(file.name)).map(|m| m.len()).ok();
            if final_len == Some(file.bytes) {
                return 0;
            }
            let part_len = std::fs::metadata(dir.join(format!("{}.part", file.name)))
                .map(|m| m.len())
                .unwrap_or(0);
            file.bytes.saturating_sub(part_len.min(file.bytes))
        })
        .sum()
}

/// Pure check: fails unless `available >= needed + DISK_MARGIN`. The message gives both numbers.
pub fn check_free_space(available: u64, needed: u64) -> Result<()> {
    let required = needed.saturating_add(DISK_MARGIN);
    if available < required {
        bail!(
            "not enough free disk space for the model download: {} needed ({} download + {} safety margin), {} free. \
             Free up about {} and try again; partial files are kept.",
            gib(required),
            gib(needed),
            gib(DISK_MARGIN),
            gib(available),
            gib(required - available),
        );
    }
    Ok(())
}

/// Checks that the volume holding `dir` has room for what `files` still need. `dir` need not exist yet: the
/// nearest existing ancestor is checked. Returns Ok without checking when the platform cannot report free space.
pub fn ensure_disk_space(dir: &Path, files: &[ModelFile]) -> Result<()> {
    let needed = remaining_bytes(dir, files);
    if needed == 0 {
        return Ok(());
    }
    let probe = dir
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("/"));
    if let Some(available) = available_bytes(probe)? {
        check_free_space(available, needed)?;
    }
    Ok(())
}

/// Bytes available to unprivileged users on the volume holding `path`; `None` where this platform cannot tell.
#[cfg(unix)]
pub fn available_bytes(path: &Path) -> Result<Option<u64>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `statvfs` only writes into `stat`, which is a valid, zeroed, properly sized struct; `c_path` is a
    // NUL-terminated string that lives for the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    #[allow(clippy::unnecessary_cast)]
    let bytes = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    Ok(Some(bytes))
}

/// Free-space probing is not implemented off unix; the check is skipped there.
#[cfg(not(unix))]
pub fn available_bytes(_path: &Path) -> Result<Option<u64>> {
    Ok(None)
}

/// Formats a byte count as GiB with two decimals, e.g. "0.81 GiB".
fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / GIB)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelFile, DISK_MARGIN};

    const FILE: ModelFile = ModelFile {
        name: "a.gguf",
        bytes: 1000,
        sha256: "",
    };

    #[test]
    fn enough_space_passes_exactly_at_margin() {
        assert!(check_free_space(DISK_MARGIN + 500, 500).is_ok());
        assert!(check_free_space(u64::MAX, u64::MAX / 2).is_ok());
    }

    #[test]
    fn one_byte_short_fails_with_numbers_in_message() {
        let err = check_free_space(DISK_MARGIN + 499, 500)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not enough free disk space"), "{err}");
        assert!(err.contains("GiB"), "{err}");
        assert!(err.contains("safety margin"), "{err}");
    }

    #[test]
    fn remaining_counts_part_and_finished_files() {
        let d = tempfile::tempdir().unwrap();
        let files = [
            FILE,
            ModelFile {
                name: "b.gguf",
                bytes: 400,
                sha256: "",
            },
        ];
        assert_eq!(remaining_bytes(d.path(), &files), 1400);
        std::fs::write(d.path().join("a.gguf.part"), vec![0u8; 300]).unwrap();
        assert_eq!(remaining_bytes(d.path(), &files), 1100);
        std::fs::write(d.path().join("a.gguf"), vec![0u8; 1000]).unwrap();
        assert_eq!(remaining_bytes(d.path(), &files), 400);
    }

    #[test]
    fn oversized_part_counts_as_done() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.gguf.part"), vec![0u8; 5000]).unwrap();
        assert_eq!(remaining_bytes(d.path(), &[FILE]), 0);
    }

    #[test]
    fn real_volume_reports_some_free_space() {
        let d = tempfile::tempdir().unwrap();
        let free = available_bytes(d.path()).unwrap();
        if cfg!(unix) {
            assert!(free.unwrap() > 0);
        } else {
            assert!(free.is_none());
        }
    }

    #[test]
    fn ensure_checks_nonexistent_dir_through_ancestor() {
        let d = tempfile::tempdir().unwrap();
        let nested = d.path().join("not/yet/created");
        // Asking for an absurd amount must fail on any real volume; the nested dir is probed via its ancestor.
        let huge = ModelFile {
            name: "huge.gguf",
            bytes: u64::MAX / 4,
            sha256: "",
        };
        if available_bytes(d.path()).unwrap().is_some() {
            assert!(ensure_disk_space(&nested, &[huge]).is_err());
        }
        assert!(ensure_disk_space(&nested, &[]).is_ok());
    }
}
