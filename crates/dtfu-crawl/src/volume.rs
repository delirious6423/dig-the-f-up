//! Volume identification: which volume a folder lives on, a stable id across
//! re-plugs, and whether it is network/removable.

use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeKind {
    Local,
    Removable,
    Network,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    pub mount_point: String,
    pub id: String,
    pub kind: VolumeKind,
    pub fs_type: String,
}

/// Pure: classify a filesystem type name as a network filesystem.
pub fn is_network_fs(fs_type: &str) -> bool {
    let t = fs_type.to_ascii_lowercase();
    t.starts_with("nfs")
        || t.starts_with("smb")
        || matches!(
            t.as_str(),
            "cifs"
                | "afpfs"
                | "afp"
                | "webdav"
                | "davfs"
                | "fuse.davfs"
                | "fuse.sshfs"
                | "sshfs"
                | "ceph"
                | "glusterfs"
                | "fuse.glusterfs"
                | "ncpfs"
        )
}

/// None when the path cannot be stat'ed. May block on a dead network mount:
/// use `volume_info_timeout` from UI/schedulers.
pub fn volume_info(path: &Path) -> Option<VolumeInfo> {
    imp::volume_info(path)
}

/// Runs `volume_info` on a helper thread; None on timeout (the thread is left
/// to finish on its own) or failure.
pub fn volume_info_timeout(path: &Path, timeout: Duration) -> Option<VolumeInfo> {
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.to_path_buf();
    std::thread::Builder::new()
        .name("dtfu-volume-info".into())
        .spawn(move || {
            let _ = tx.send(volume_info(&p));
        })
        .ok()?;
    rx.recv_timeout(timeout).ok().flatten()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;

    fn cstr(a: &[libc::c_char]) -> String {
        // SAFETY: statfs name fields are NUL-terminated.
        unsafe { CStr::from_ptr(a.as_ptr()) }.to_string_lossy().into_owned()
    }

    fn plist_value(xml: &str, key: &str) -> Option<String> {
        let k = format!("<key>{key}</key>");
        let rest = &xml[xml.find(&k)? + k.len()..];
        let s = rest.find("<string>")? + "<string>".len();
        let e = rest[s..].find("</string>")?;
        // The value must belong to this key (no other key in between).
        if rest[..s].contains("<key>") {
            return None;
        }
        Some(rest[s..s + e].trim().to_string())
    }

    /// `diskutil` takes ~250 ms, and sync asks twice per root. A mount keeps its `f_fsid` until it is unmounted, and a
    /// drive mounted again (or a different one) gets a new one, so (fsid, device) is a safe cache key.
    fn cached_disk_uuid(st: &libc::statfs, mount: &str, from: &str) -> Option<String> {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        type UuidCache = Mutex<HashMap<(i32, i32, String), Option<String>>>;
        static CACHE: OnceLock<UuidCache> = OnceLock::new();
        // SAFETY: fsid_t is two i32s (its field is private in the libc crate).
        let fsid: [i32; 2] = unsafe { std::mem::transmute(st.f_fsid) };
        let key = (fsid[0], fsid[1], from.to_string());
        let cache = CACHE.get_or_init(Default::default);
        if let Some(v) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return v.clone();
        }
        let v = disk_uuid(mount);
        // A failed lookup is not cached: a one-off diskutil failure would switch this drive's id to the `dev:` form
        // for as long as it stays mounted, and the folder would look like a different drive.
        if v.is_some() {
            cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key, v.clone());
        }
        v
    }

    fn disk_uuid(mount: &str) -> Option<String> {
        let out = std::process::Command::new("diskutil")
            .args(["info", "-plist", mount])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let xml = String::from_utf8_lossy(&out.stdout);
        plist_value(&xml, "VolumeUUID").filter(|s| !s.is_empty())
    }

    pub fn volume_info(path: &Path) -> Option<VolumeInfo> {
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: zeroed statfs is a valid out-parameter.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: c is NUL-terminated, st is valid.
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let mount_point = cstr(&st.f_mntonname);
        let fs_type = cstr(&st.f_fstypename);
        let from = cstr(&st.f_mntfromname);
        let local = st.f_flags & (libc::MNT_LOCAL as u32) != 0;
        let network = is_network_fs(&fs_type) || !local;
        let (kind, id) = if network {
            (VolumeKind::Network, format!("net:{from}"))
        } else {
            let kind = if mount_point.starts_with("/Volumes/") {
                VolumeKind::Removable
            } else {
                VolumeKind::Local
            };
            let id = match cached_disk_uuid(&st, &mount_point, &from) {
                Some(u) => format!("uuid:{u}"),
                None => format!(
                    "dev:{from}:{fs_type}:{}",
                    (st.f_blocks).wrapping_mul(st.f_bsize as u64)
                ),
            };
            (kind, id)
        };
        Some(VolumeInfo { mount_point, id, kind, fs_type })
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::path::PathBuf;

    struct Mount {
        point: String,
        fs: String,
        source: String,
    }

    fn unescape(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
                out.push(v);
                i += 4;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn parse_mountinfo(text: &str) -> Vec<Mount> {
        text.lines()
            .filter_map(|l| {
                let (pre, post) = l.split_once(" - ")?;
                let point = unescape(pre.split(' ').nth(4)?);
                let mut it = post.split(' ');
                let fs = it.next()?.to_string();
                let source = unescape(it.next()?);
                Some(Mount { point, fs, source })
            })
            .collect()
    }

    fn by_uuid(source: &str) -> Option<String> {
        let dev = std::fs::canonicalize(source).ok()?;
        for e in std::fs::read_dir("/dev/disk/by-uuid").ok()?.flatten() {
            if std::fs::canonicalize(e.path()).ok().as_ref() == Some(&dev) {
                return Some(e.file_name().to_string_lossy().into_owned());
            }
        }
        None
    }

    fn is_removable(source: &str) -> bool {
        let Ok(dev) = std::fs::canonicalize(source) else { return false };
        let Some(name) = dev.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return false;
        };
        // Partition -> parent disk via sysfs: /sys/class/block/<part> resolves under the disk.
        let sys = PathBuf::from("/sys/class/block").join(&name);
        let Ok(real) = std::fs::canonicalize(&sys) else { return false };
        let mut cur = Some(real.as_path());
        while let Some(p) = cur {
            if let Ok(s) = std::fs::read_to_string(p.join("removable")) {
                return s.trim() == "1";
            }
            cur = p.parent().filter(|q| q.starts_with("/sys/devices"));
        }
        false
    }

    pub fn volume_info(path: &Path) -> Option<VolumeInfo> {
        let canon = std::fs::canonicalize(path).ok()?;
        let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        let mounts = parse_mountinfo(&text);
        // Longest mount point prefix; later entries win ties (overmounts).
        let m = mounts
            .iter()
            .filter(|m| canon.starts_with(&m.point))
            .max_by_key(|m| m.point.len())?;
        let network = is_network_fs(&m.fs);
        let (kind, id) = if network {
            (VolumeKind::Network, format!("net:{}", m.source))
        } else {
            let removable = m.point.starts_with("/media/")
                || m.point.starts_with("/run/media/")
                || (m.source.starts_with("/dev/") && is_removable(&m.source));
            let kind = if removable { VolumeKind::Removable } else { VolumeKind::Local };
            let id = match by_uuid(&m.source) {
                Some(u) => format!("uuid:{u}"),
                None => format!("dev:{}", m.source),
            };
            (kind, id)
        };
        Some(VolumeInfo { mount_point: m.point.clone(), id, kind, fs_type: m.fs.clone() })
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW};
    use windows_sys::Win32::System::WindowsProgramming::{DRIVE_REMOTE, DRIVE_REMOVABLE};

    pub fn volume_info(path: &Path) -> Option<VolumeInfo> {
        let abs = std::fs::canonicalize(path).ok()?;
        let wide: Vec<u16> = abs.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut root = [0u16; 261];
        // SAFETY: wide is NUL-terminated; root has the stated capacity.
        if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return None;
        }
        let len = root.iter().position(|&c| c == 0).unwrap_or(root.len());
        let mount_point = String::from_utf16_lossy(&root[..len]);
        // SAFETY: root is NUL-terminated by the API.
        let dt = unsafe { GetDriveTypeW(root.as_ptr()) };
        let mut serial = 0u32;
        let mut fsname = [0u16; 64];
        // SAFETY: all out pointers are valid for the sizes given.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fsname.as_mut_ptr(),
                fsname.len() as u32,
            )
        };
        let fl = fsname.iter().position(|&c| c == 0).unwrap_or(fsname.len());
        let fs_type = if ok != 0 { String::from_utf16_lossy(&fsname[..fl]) } else { String::new() };
        let kind = match dt {
            DRIVE_REMOTE => VolumeKind::Network,
            DRIVE_REMOVABLE => VolumeKind::Removable,
            _ => VolumeKind::Local,
        };
        let id = if kind == VolumeKind::Network {
            format!("net:{}", mount_point.trim_end_matches('\\').to_ascii_lowercase())
        } else if ok != 0 {
            format!("serial:{serial:08X}")
        } else {
            format!("drive:{}", mount_point.trim_end_matches('\\'))
        };
        Some(VolumeInfo { mount_point, id, kind, fs_type })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod imp {
    use super::*;
    pub fn volume_info(path: &Path) -> Option<VolumeInfo> {
        std::fs::metadata(path).ok()?;
        Some(VolumeInfo {
            mount_point: "/".into(),
            id: "dev:unknown".into(),
            kind: VolumeKind::Local,
            fs_type: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_paths() {
        for p in [std::env::temp_dir(), Path::new("/").to_path_buf()] {
            #[cfg(windows)]
            let p = if p == Path::new("/") { std::env::temp_dir() } else { p };
            let a = volume_info(&p).expect("some");
            assert_eq!(a.kind, VolumeKind::Local, "{a:?}");
            assert!(!a.id.is_empty());
            assert_eq!(volume_info(&p), Some(a));
        }
    }

    #[test]
    fn network_table() {
        for t in ["smbfs", "nfs", "afpfs", "webdav", "cifs", "smb3", "nfs4", "fuse.sshfs"] {
            assert!(is_network_fs(t), "{t}");
        }
        for t in ["apfs", "hfs", "ext4", "ntfs", "exfat", "msdos", "tmpfs", "fuse", ""] {
            assert!(!is_network_fs(t), "{t}");
        }
    }

    #[test]
    fn missing_is_none() {
        assert!(volume_info(Path::new("/definitely/not/here/dtfu-xyz")).is_none());
    }

    #[test]
    fn timeout_matches() {
        let p = std::env::temp_dir();
        assert_eq!(volume_info_timeout(&p, Duration::from_secs(20)), volume_info(&p));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn disk_image_removable() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("t.dmg");
        let ok = Command::new("hdiutil")
            .args(["create", "-size", "4m", "-fs", "APFS", "-volname", "dtfuvoltest"])
            .arg(&img)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let attach = || {
            let s = Command::new("hdiutil").arg("attach").arg(&img).status().unwrap();
            assert!(s.success());
            volume_info(Path::new("/Volumes/dtfuvoltest")).expect("info")
        };
        let detach = || {
            Command::new("hdiutil").args(["detach", "/Volumes/dtfuvoltest"]).status().unwrap();
        };
        let a = attach();
        detach();
        let b = attach();
        detach();
        assert_eq!(a.kind, VolumeKind::Removable, "{a:?}");
        assert!(a.id.starts_with("uuid:"), "{a:?}");
        assert_eq!(a.id, b.id);
    }
}
