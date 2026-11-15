//! Portable mode, for a USB stick (SPEC section 7; VibePrint's, copied). Decided at start, before
//! any prefs are read: VibeRepos is portable when a file named `portable` (exactly, any case, no
//! extension; a folder does not count) sits next to the exe, or the exe's volume is removable
//! (`GetDriveTypeW` DRIVE_REMOVABLE, or its disk reports a USB, SD or MMC bus through
//! `IOCTL_STORAGE_QUERY_PROPERTY` on the volume opened with no access rights). Then its settings
//! live in `<exe folder>\VibeRepos-data\config\`, created on the first write, and nothing goes to
//! the PC's profile or registry. A folder it cannot write to keeps the settings in memory.
//!
//! The exe's path is its final path (a junction or a subst drive resolved), so the marker and the
//! data folder are where the exe really is.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Installed (or run from a fixed disk): the profile's folders.
    Installed,
    /// Portable, settings beside the exe.
    Portable { dir: PathBuf },
    /// Portable, but the exe's folder cannot be written: settings in memory only.
    ReadOnly { dir: PathBuf },
}

impl Mode {
    pub fn portable(&self) -> bool {
        !matches!(self, Mode::Installed)
    }

    /// The exe's folder of a portable copy.
    pub fn dir(&self) -> Option<&Path> {
        match self {
            Mode::Installed => None,
            Mode::Portable { dir } | Mode::ReadOnly { dir } => Some(dir),
        }
    }
}

/// The marker's name: `portable`, any case, nothing more.
pub fn is_marker(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|n| n.eq_ignore_ascii_case("portable"))
}

/// A file (not a folder) named `portable` in `dir`.
pub fn has_marker(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    rd.flatten().take(10_000).any(|e| is_marker(&e.file_name()) && e.file_type().is_ok_and(|t| t.is_file()))
}

/// The bus types that make a disk a stick or a card.
pub fn removable_bus(bus: i32) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{BusTypeMmc, BusTypeSd, BusTypeUsb};
    bus == BusTypeUsb || bus == BusTypeSd || bus == BusTypeMmc
}

/// The exe's volume is removable: by drive type, or by its disk's bus.
pub fn on_removable(exe: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::*;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY, StorageDeviceProperty};
    let mut p: Vec<u16> = exe.as_os_str().encode_wide().collect();
    p.push(0);
    let mut root = vec![0u16; 1024];
    // SAFETY: NUL-terminated in, a buffer and its length out.
    if unsafe { GetVolumePathNameW(p.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return false;
    }
    // SAFETY: the root just read.
    if unsafe { GetDriveTypeW(root.as_ptr()) } == 2 {
        return true; // DRIVE_REMOVABLE
    }
    let mut name = vec![0u16; 64];
    // SAFETY: the root (NUL-terminated), a buffer of 50+ units.
    if unsafe { GetVolumeNameForVolumeMountPointW(root.as_ptr(), name.as_mut_ptr(), name.len() as u32) } == 0 {
        return false;
    }
    // `\\?\Volume{…}\` without its last backslash opens the volume itself.
    let n = name.iter().position(|&c| c == 0).unwrap_or(0);
    let mut dev = name[..n.saturating_sub(1)].to_vec();
    dev.push(0);
    // SAFETY: no access rights asked: a query of the device only; closed below.
    let h = unsafe { CreateFileW(dev.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return false;
    }
    let q = STORAGE_PROPERTY_QUERY { PropertyId: StorageDeviceProperty, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
    let mut out = [0u64; 128];
    let mut got = 0u32;
    // SAFETY: the query in, a 1 KiB aligned buffer out; the handle closed below.
    let ok = unsafe { DeviceIoControl(h, IOCTL_STORAGE_QUERY_PROPERTY, (&q as *const STORAGE_PROPERTY_QUERY).cast(), size_of::<STORAGE_PROPERTY_QUERY>() as u32, out.as_mut_ptr().cast(), 1024, &mut got, std::ptr::null_mut()) } != 0;
    // SAFETY: the handle opened above, closed once.
    unsafe { CloseHandle(h) };
    if !ok || (got as usize) < size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return false;
    }
    // SAFETY: the buffer holds a descriptor of at least its own size, 8-aligned.
    let d = unsafe { &*(out.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    removable_bus(d.BusType)
}

/// The folder can be written: a probe file of this process's own, made create-new and removed.
pub fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!("VibeRepos-write-test.{}.tmp", std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The exe's final path: a junction, symbolic link or subst drive on the way resolved (the
/// `\\?\` prefix dropped again). The path as given when it cannot be resolved.
pub fn final_path(exe: &Path) -> PathBuf {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFinalPathNameByHandleW, OPEN_EXISTING, VOLUME_NAME_DOS};
    let w = crate::host::wide_path(exe);
    // SAFETY: a NUL-terminated path; no access rights asked; closed below.
    let h = unsafe { CreateFileW(w.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return exe.to_path_buf();
    }
    let mut buf = vec![0u16; 32 * 1024];
    // SAFETY: a buffer of the length given; the handle is open.
    let n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS) } as usize;
    // SAFETY: opened above, closed once.
    unsafe { CloseHandle(h) };
    if n == 0 || n >= buf.len() {
        return exe.to_path_buf();
    }
    strip_verbatim(&buf[..n])
}

/// `\\?\C:\x` as `C:\x`, `\\?\UNC\s\x` as `\\s\x`; other forms as they are.
pub fn strip_verbatim(units: &[u16]) -> PathBuf {
    let pre = |p: &str| p.encode_utf16().collect::<Vec<u16>>();
    if units.starts_with(&pre("\\\\?\\UNC\\")) {
        let mut v = pre("\\\\");
        v.extend_from_slice(&units[8..]);
        crate::host::path_from_wide(&v)
    } else if units.starts_with(&pre("\\\\?\\")) && units.get(5) == Some(&(b':' as u16)) {
        crate::host::path_from_wide(&units[4..])
    } else {
        crate::host::path_from_wide(units)
    }
}

/// The mode for an exe at `exe` (its final path).
pub fn detect(exe: &Path) -> Mode {
    let Some(dir) = exe.parent().map(Path::to_path_buf) else { return Mode::Installed };
    if !(has_marker(&dir) || on_removable(exe)) {
        return Mode::Installed;
    }
    if writable(&dir) { Mode::Portable { dir } } else { Mode::ReadOnly { dir } }
}

/// The settings folders of a portable copy.
pub fn paths(dir: &Path) -> crate::host::Paths {
    let data = dir.join("VibeRepos-data");
    crate::host::Paths { config: data.join("config"), local_data: data.join("local") }
}

/// The drive letter a path is on (`C` for `C:\x`), None for a share or anything else.
pub fn drive_of(p: &Path) -> Option<char> {
    let s = p.as_os_str().to_str()?;
    let b = s.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\').then(|| (b[0] as char).to_ascii_uppercase())
}

/// The prefix a root on the stick's own drive is kept with: `?` is never a drive letter, so a
/// kept `?:\Projects` cannot be mistaken for a real path.
const SAME_DRIVE: &str = "?:\\";

/// A root as it is kept in prefs. A portable copy keeps a root on its own drive relative to that
/// drive (`?:\Projects`), so it still works when the stick gets another letter; every other root
/// as typed.
pub fn keep_root(root: &str, exe_drive: Option<char>) -> String {
    match (exe_drive, drive_of(Path::new(root))) {
        (Some(e), Some(d)) if e == d => format!("{SAME_DRIVE}{}", &root[3..]),
        _ => root.to_string(),
    }
}

/// A kept root as a path: `?:\x` on the exe's drive now (None when the exe has no drive letter).
pub fn resolve_root(kept: &str, exe_drive: Option<char>) -> Option<PathBuf> {
    match kept.strip_prefix(SAME_DRIVE) {
        Some(rest) => exe_drive.map(|d| PathBuf::from(format!("{d}:\\{rest}"))),
        None => Some(PathBuf::from(kept)),
    }
}
