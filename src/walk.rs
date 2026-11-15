//! Finding the repos under the roots (SPEC section 2): VibeSpace's bulk listing
//! (`GetFileInformationByHandleEx` with `FileIdExtdDirectoryInfo`, `FileIdBothDirectoryInfo`
//! where a file system lacks it), walked from each root's verbatim `\\?\` form on a worker.
//!
//! A root that is itself a junction or symbolic link is followed (it is what the user picked);
//! inside, name-surrogate reparse points (junctions, symbolic links, mount points) are counted
//! and not followed, so no loop is possible; other reparse points (OneDrive's folders) are walked,
//! except an online-only one, whose listing OneDrive would fetch: counted, not opened (X15WFS-9).
//! The walk never enters a `.git` folder or a bare repo, goes on into working trees (nested repos
//! and submodules are rows of their own), skips the build and package folders the spec names,
//! goes at most 8 folders below a root and lists at most 200 000 folders per scan. Every path it
//! hands out is built from the raw UTF-16 names it listed, never from git's output.

use crate::discover::{self, Kind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub const MAX_DEPTH: usize = 8;
pub const MAX_FOLDERS: u64 = 200_000;
const BUF: usize = 64 * 1024;

/// Folders never walked into: package stores and build output, by name (any case).
pub const SKIP: [&str; 6] = ["node_modules", ".venv", "__pycache__", ".next", ".svelte-kit", ".astro"];

/// Windows' words for an I/O error, without the "(os error 5)".
pub fn error_words(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => error_text(code as u32),
        None => e.to_string(),
    }
}

/// Windows' words for an error code, without the "(os error 5)" and the full stop.
pub fn error_text(code: u32) -> String {
    let s = std::io::Error::from_raw_os_error(code as i32).to_string();
    let s = match s.find(" (os error") {
        Some(p) => s[..p].to_string(),
        None => s,
    };
    s.trim_end_matches('.').to_string()
}

// ---------- paths ----------

/// UTF-16 path to its `\\?\` form (long paths, trailing dots and spaces, `aux.c` reach the file).
pub fn long(units: &[u16]) -> Vec<u16> {
    let bs = b'\\' as u16;
    let s: Vec<u16> = units.iter().map(|&c| if c == b'/' as u16 { bs } else { c }).collect();
    let pre = |p: &str| p.encode_utf16().collect::<Vec<u16>>();
    if s.starts_with(&pre("\\\\?\\")) {
        s
    } else if s.starts_with(&pre("\\\\")) {
        let mut v = pre("\\\\?\\UNC\\");
        v.extend_from_slice(&s[2..]);
        v
    } else {
        let mut v = pre("\\\\?\\");
        v.extend_from_slice(&s);
        v
    }
}

pub fn units(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().collect()
}

fn with_nul(v: &[u16]) -> Vec<u16> {
    let mut w = v.to_vec();
    w.push(0);
    w
}

/// A root as the walk keeps it: absolute, separators backslashes, no trailing one (a drive keeps
/// its own: `C:\`), the `\\?\` prefix dropped. Built from the exact units (a name with a lone
/// surrogate stays itself).
pub fn normalize_root(p: &Path) -> PathBuf {
    let mut u: Vec<u16> = units(p).iter().map(|&c| if c == b'/' as u16 { b'\\' as u16 } else { c }).collect();
    let pre = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    if u.starts_with(&pre("\\\\?\\UNC\\")) {
        u = pre("\\\\").into_iter().chain(u[8..].iter().copied()).collect();
    } else if u.starts_with(&pre("\\\\?\\")) {
        u = u[4..].to_vec();
    }
    while u.len() > 3 && u.last() == Some(&(b'\\' as u16)) {
        u.pop();
    }
    if u.len() == 2 && u[1] == b':' as u16 {
        u.push(b'\\' as u16);
    }
    crate::host::path_from_wide(&u)
}

/// What a path names right now, read through a handle (a link at the end followed, as git and
/// Explorer follow it): its volume serial and 128-bit file id (FILE_ID_INFO). None when either is
/// zero or it cannot be read: unknown, never "the same as" anything.
pub fn identity(path: &Path) -> Option<(u64, u128)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandleEx, OPEN_EXISTING};
    let p = with_nul(&long(&units(path)));
    // SAFETY: a NUL-terminated path; attributes only; closed below.
    let h = unsafe { CreateFileW(p.as_ptr(), FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: a zeroed out struct of the size given; the handle is open.
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandleEx(h, 18 /* FileIdInfo */, (&mut info as *mut FILE_ID_INFO).cast(), size_of::<FILE_ID_INFO>() as u32) } != 0;
    // SAFETY: opened above, closed once.
    unsafe { CloseHandle(h) };
    let id = u128::from_le_bytes(info.FileId.Identifier);
    (ok && info.VolumeSerialNumber != 0 && id != 0).then_some((info.VolumeSerialNumber, id))
}

/// A path's attributes: Ok(None) when it is not there (not found, its folder answering or not:
/// the caller decides), Err with the code for anything else.
pub fn attributes(path: &Path) -> Result<Option<u32>, u32> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    let p = with_nul(&long(&units(path)));
    // SAFETY: a NUL-terminated path.
    let a = unsafe { GetFileAttributesW(p.as_ptr()) };
    if a != INVALID_FILE_ATTRIBUTES {
        return Ok(Some(a));
    }
    // SAFETY: right after the failing call.
    match unsafe { GetLastError() } {
        2 | 3 => Ok(None),
        e => Err(e),
    }
}

pub const ATTR_DIRECTORY: u32 = 0x10;
pub const ATTR_REPARSE: u32 = 0x400;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x4_0000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// A reparse tag that names another place (junction, symbolic link, mount point, ...): the walk
/// does not follow these.
pub fn name_surrogate(tag: u32) -> bool {
    tag & 0x2000_0000 != 0
}

// ---------- the listing ----------

/// One folder entry.
#[derive(Clone, Debug)]
pub struct Item {
    pub name: Vec<u16>,
    pub attrs: u32,
    pub tag: u32,
}

impl Item {
    pub fn dir(&self) -> bool {
        self.attrs & ATTR_DIRECTORY != 0
    }
    pub fn link(&self) -> bool {
        self.attrs & ATTR_REPARSE != 0 && name_surrogate(self.tag)
    }
    pub fn is(&self, name: &str) -> bool {
        String::from_utf16(&self.name).is_ok_and(|n| n.eq_ignore_ascii_case(name))
    }
    /// A cloud folder whose listing is not on this disk (OneDrive's online-only folders):
    /// listing it makes the provider fetch it over the network (X15WFS-9).
    pub fn online(&self) -> bool {
        self.attrs & (ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0
    }
}

/// What the walk does with a subfolder it listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sub {
    /// `.git`, a package or build folder (`SKIP`, a Cargo `target`).
    Skip,
    /// A name-surrogate reparse point: counted, not followed.
    Link,
    /// Online-only (`Item::online`): counted, not opened, as VibeSpace and VibeRescue do.
    Online,
    Walk,
}

/// `it`, a subfolder of a folder that holds a Cargo.toml when `cargo`.
fn sub_step(it: &Item, cargo: bool) -> Sub {
    if it.is(".git") || SKIP.iter().any(|s| it.is(s)) || (cargo && it.is("target")) {
        Sub::Skip
    } else if it.link() {
        Sub::Link
    } else if it.online() {
        Sub::Online
    } else {
        Sub::Walk
    }
}

/// One folder's entries, or the Windows error that stopped the listing (with what came before).
#[derive(Default, Debug)]
pub struct Listing {
    pub items: Vec<Item>,
    pub error: Option<u32>,
}

const CLASS_EXTD: i32 = 19; // FileIdExtdDirectoryInfo
const CLASS_BOTH: i32 = 10; // FileIdBothDirectoryInfo

/// Which class this scan lists with: 0 not decided, 1 Extd works, 2 the older one (FAT, some
/// shares). One per scan, never for the process.
pub type Mode = AtomicU32;

/// List a folder (its `\\?\` path, no NUL) in bulk.
pub fn list(path: &[u16], mode: &Mode) -> Listing {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandleEx, OPEN_EXISTING};
    let mut out = Listing::default();
    let p = with_nul(path);
    // SAFETY: a NUL-terminated path; list rights only; closed below.
    let h = unsafe { CreateFileW(p.as_ptr(), FILE_LIST_DIRECTORY, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: right after the failing call.
        out.error = Some(unsafe { GetLastError() });
        return out;
    }
    let mut buf = vec![0u64; BUF / 8];
    let mut class = if mode.load(Ordering::Relaxed) == 2 { CLASS_BOTH } else { CLASS_EXTD };
    let mut first = true;
    loop {
        // SAFETY: a buffer of BUF bytes, 8-aligned; the handle is open.
        let ok = unsafe { GetFileInformationByHandleEx(h, class, buf.as_mut_ptr().cast(), BUF as u32) } != 0;
        if !ok {
            // SAFETY: right after the failing call.
            let e = unsafe { GetLastError() };
            if e == 18 {
                break; // ERROR_NO_MORE_FILES
            }
            if first && class == CLASS_EXTD && matches!(e, 87 | 50 | 124 | 1) {
                class = CLASS_BOTH;
                continue;
            }
            out.error = Some(e);
            break;
        }
        if first {
            mode.store(if class == CLASS_EXTD { 1 } else { 2 }, Ordering::Relaxed);
        }
        first = false;
        if !parse(&buf, class, &mut out) {
            break;
        }
        if out.items.len() > 1_000_000 {
            out.error = Some(8); // ERROR_NOT_ENOUGH_MEMORY: a folder of a million entries is told, not held
            break;
        }
    }
    // SAFETY: opened above, closed once.
    unsafe { CloseHandle(h) };
    out
}

/// One buffer of a listing into `out`. Every entry is checked against the buffer before it is
/// read (VibeSpace SPE-5): one that does not fit sets ERROR_INVALID_DATA and stops.
fn parse(buf: &[u64], class: i32, out: &mut Listing) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{FILE_ID_BOTH_DIR_INFO, FILE_ID_EXTD_DIR_INFO};
    let size = buf.len() * 8;
    let (whole, header) = if class == CLASS_EXTD {
        (size_of::<FILE_ID_EXTD_DIR_INFO>(), std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName))
    } else {
        (size_of::<FILE_ID_BOTH_DIR_INFO>(), std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName))
    };
    let base = buf.as_ptr() as *const u8;
    let mut off = 0usize;
    loop {
        if !off.is_multiple_of(8) || off + whole > size {
            out.error = Some(13); // ERROR_INVALID_DATA
            return false;
        }
        // SAFETY: the struct fits whole inside the buffer at an 8-aligned offset (checked above).
        let (next, attrs, tag, name_bytes) = unsafe {
            if class == CLASS_EXTD {
                let d = &*(base.add(off) as *const FILE_ID_EXTD_DIR_INFO);
                (d.NextEntryOffset, d.FileAttributes, d.ReparsePointTag, d.FileNameLength as usize)
            } else {
                let d = &*(base.add(off) as *const FILE_ID_BOTH_DIR_INFO);
                // For a reparse point this field holds its tag (as FindFirstFile reads it).
                (d.NextEntryOffset, d.FileAttributes, if d.FileAttributes & ATTR_REPARSE != 0 { d.EaSize } else { 0 }, d.FileNameLength as usize)
            }
        };
        if off + header + name_bytes > size || (next != 0 && (next as usize) < header) {
            out.error = Some(13);
            return false;
        }
        // SAFETY: the name lies inside the buffer (checked above); u16 alignment holds (8-aligned
        // entry, even header offset).
        let name = unsafe { std::slice::from_raw_parts(base.add(off + header) as *const u16, name_bytes / 2) };
        let dot = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
        if !dot && !name.is_empty() {
            out.items.push(Item { name: name.to_vec(), attrs, tag });
        }
        if next == 0 {
            return true;
        }
        off += next as usize;
    }
}

// ---------- the walk ----------

/// A repo (or a broken `.git`) the walk found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Which root it was found under (the first, when two reach it).
    pub root: usize,
    /// Its working folder (a bare repo's own folder), from the walk's raw names.
    pub path: PathBuf,
    /// The path under its root ("" for the root itself).
    pub rel: PathBuf,
    pub kind: Kind,
    /// Its git folder (`<path>\.git`, a `.git` file's target, a bare repo itself).
    pub gitdir: Option<PathBuf>,
    /// The git folder its refs and FETCH_HEAD live in (a linked worktree shares its main repo's).
    pub common: Option<PathBuf>,
    /// The working folder of another repo this one sits inside (not as a submodule).
    pub nested_in: Option<PathBuf>,
    /// FILE_ID_INFO of `path` (None: unknown).
    pub id: Option<(u64, u128)>,
}

impl Found {
    /// The folder's name, as shown.
    pub fn name(&self) -> String {
        crate::text::clean(&self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.path.to_string_lossy().into_owned()))
    }
}

/// What a walk saw.
#[derive(Clone, Debug, Default)]
pub struct Walked {
    pub found: Vec<Found>,
    pub folders: u64,
    /// Name-surrogate reparse points not followed.
    pub links: u64,
    /// Online-only cloud folders not opened (X15WFS-9).
    pub online: u64,
    /// Folders that could not be read (below the roots).
    pub unreadable: u64,
    /// The first of them, in words.
    pub first_unreadable: Option<String>,
    /// Roots that could not be read at all: (root index, words).
    pub root_errors: Vec<(usize, String)>,
    /// The 200 000 folder cap was reached.
    pub capped: bool,
    pub cancelled: bool,
}

impl Walked {
    /// Partial: what it left out, in words ("12 folders could not be read"), or None.
    pub fn partial(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.capped {
            parts.push(format!("stopped at {} folders", crate::text::thousands(MAX_FOLDERS)));
        }
        if self.unreadable > 0 {
            parts.push(format!("{} could not be read", crate::text::count(self.unreadable as usize, "folder", "folders")));
        }
        if !self.root_errors.is_empty() {
            parts.push(format!("{} could not be read", crate::text::count(self.root_errors.len(), "root", "roots")));
        }
        if self.online > 0 {
            parts.push(format!("{} not opened", crate::text::count(self.online as usize, "online-only folder", "online-only folders")));
        }
        if self.cancelled {
            parts.push("stopped".into());
        }
        (!parts.is_empty()).then(|| format!("partial: {}", parts.join(", ")))
    }
}

/// Progress, at most every 100 ms.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub root: String,
    pub folders: u64,
    pub repos: usize,
}

struct Job {
    path: PathBuf,
    rel: PathBuf,
    depth: usize,
    /// The repo whose working tree this folder is in.
    inside: Option<PathBuf>,
}

/// Walk `roots` in order. `cancel` stops it between folders.
pub fn walk(roots: &[PathBuf], cancel: &AtomicBool, progress: &dyn Fn(&Progress)) -> Walked {
    walk_with(roots, MAX_DEPTH, false, cancel, progress)
}

/// Whether `root` holds a repo within `depth` levels (the first start's look at the usual
/// folders): the walk stops at the first one.
pub fn holds_a_repo(root: &Path, depth: usize) -> bool {
    !walk_with(&[root.to_path_buf()], depth, true, &AtomicBool::new(false), &|_| {}).found.is_empty()
}

fn walk_with(roots: &[PathBuf], max_depth: usize, first_only: bool, cancel: &AtomicBool, progress: &dyn Fn(&Progress)) -> Walked {
    let mut w = Walked::default();
    let mut seen_ids: std::collections::HashSet<(u64, u128)> = Default::default();
    let mut seen_paths: std::collections::HashSet<String> = Default::default();
    let mut last = Instant::now();
    for (ri, root) in roots.iter().enumerate() {
        if w.capped || cancel.load(Ordering::Relaxed) {
            break;
        }
        let root = normalize_root(root);
        let shown_root = crate::text::shown_path(&root);
        let mode = Mode::new(0);
        let mut stack = vec![Job { path: root.clone(), rel: PathBuf::new(), depth: 0, inside: None }];
        while let Some(job) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                w.cancelled = true;
                break;
            }
            if w.folders >= MAX_FOLDERS {
                w.capped = true;
                break;
            }
            let l = list(&long(&units(&job.path)), &mode);
            w.folders += 1;
            if let Some(e) = l.error
                && l.items.is_empty()
            {
                if job.depth == 0 {
                    w.root_errors.push((ri, format!("{}: {}", shown_root, error_text(e))));
                } else if !matches!(e, 2 | 3) {
                    // Not found: it went between the listing of its folder and now (gone, not
                    // unreadable).
                    w.unreadable += 1;
                    w.first_unreadable.get_or_insert_with(|| format!("{}: {}", crate::text::shown_path(&job.path), error_text(e)));
                }
                continue;
            } else if l.error.is_some_and(|e| e != 18) {
                w.unreadable += 1;
                w.first_unreadable.get_or_insert_with(|| format!("{}: {} (listed in part)", crate::text::shown_path(&job.path), error_text(l.error.unwrap_or(0))));
            }
            let found = examine(&job.path, &l.items);
            let mut descend = true;
            let mut inside = job.inside.clone();
            if let Some((kind, gitdir, common)) = found {
                // A bare repo has no working tree to walk; every other kind is a folder of files.
                descend = !kind.bare();
                let id = identity(&job.path);
                let key = job.path.to_string_lossy().to_lowercase();
                let dup = id.is_some_and(|i| seen_ids.contains(&i)) || seen_paths.contains(&key);
                if dup {
                    // The same repo through another root (or a link at a root): listed once, and
                    // its tree was walked there already.
                    continue;
                }
                if let Some(i) = id {
                    seen_ids.insert(i);
                }
                seen_paths.insert(key);
                // Worktrees and submodules carry their own tag; any other repo inside another
                // repo's working tree is "nested in" it.
                let nested_in = if matches!(kind, Kind::Worktree { .. } | Kind::Submodule { .. }) { None } else { job.inside.clone() };
                if kind.readable() && !kind.bare() {
                    inside = Some(job.path.clone());
                }
                w.found.push(Found { root: ri, path: job.path.clone(), rel: job.rel.clone(), kind, gitdir, common, nested_in, id });
                if first_only {
                    return w;
                }
            } else if is_git_folder(&l.items) {
                // A git folder that is no bare repo (a separate git folder's target, a copy of
                // `.git\modules`): not a repo of its own, and nothing to walk.
                descend = false;
            }
            if descend && job.depth < max_depth {
                let cargo = l.items.iter().any(|i| !i.dir() && i.is("Cargo.toml"));
                let mut subs: Vec<&Item> = l.items.iter().filter(|i| i.dir()).collect();
                // Popped from the end: push in reverse order, so the walk goes A to Z.
                subs.sort_by(|a, b| crate::model::ordinal_cmp(&b.name, &a.name));
                for it in subs {
                    match sub_step(it, cargo) {
                        Sub::Skip => continue,
                        Sub::Link => {
                            w.links += 1;
                            continue;
                        }
                        Sub::Online => {
                            w.online += 1;
                            continue;
                        }
                        Sub::Walk => {}
                    }
                    let name = crate::host::path_from_wide(&it.name);
                    stack.push(Job { path: job.path.join(&name), rel: job.rel.join(&name), depth: job.depth + 1, inside: inside.clone() });
                }
            }
            if last.elapsed() >= Duration::from_millis(100) {
                last = Instant::now();
                progress(&Progress { root: shown_root.clone(), folders: w.folders, repos: w.found.len() });
            }
        }
        if w.cancelled {
            break;
        }
    }
    if !w.cancelled {
        reprobe(&mut w.found);
    }
    w
}

/// HEAD, objects and refs side by side: a git folder.
fn is_git_folder(items: &[Item]) -> bool {
    items.iter().any(|i| !i.dir() && i.is("HEAD")) && items.iter().any(|i| i.dir() && i.is("objects")) && items.iter().any(|i| i.dir() && i.is("refs"))
}

/// What a folder is, from its listing: a repo (by its `.git`), a bare repo, or nothing. Reads a
/// `.git` file (at most 4 KiB), `.git\HEAD`'s presence, a linked worktree's `commondir` and a
/// bare candidate's `config`.
pub fn examine(folder: &Path, items: &[Item]) -> Option<(Kind, Option<PathBuf>, Option<PathBuf>)> {
    if let Some(dot) = items.iter().find(|i| i.is(".git")) {
        let gitdir = folder.join(".git");
        if dot.dir() {
            return Some(match attributes(&gitdir.join("HEAD")) {
                Ok(None) => (Kind::BrokenDir, Some(gitdir.clone()), Some(gitdir)),
                Ok(Some(a)) if a & ATTR_DIRECTORY != 0 => (Kind::BrokenDir, Some(gitdir.clone()), Some(gitdir)),
                // Present, or unknown (refused): git will say what it is.
                _ => (Kind::Repo, Some(gitdir.clone()), Some(gitdir)),
            });
        }
        return Some(git_file(folder, &gitdir));
    }
    let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if (discover::bare_name(&name) && items.iter().any(|i| !i.dir() && i.is("HEAD"))) || is_git_folder(items) {
        let config = crate::gitconfig::load(&[folder.join("config")], None);
        if config.problem.is_none() && config.boolean("core", "bare") == Some(true) {
            return Some((Kind::Bare, Some(folder.to_path_buf()), Some(folder.to_path_buf())));
        }
    }
    None
}

/// How long the walk waits for a `.git` file's gitdir on another machine (and once more at the
/// end of the walk, for the servers still not answered).
pub const UNC_WAIT: Duration = Duration::from_secs(5);

/// What asks a gitdir on another machine: `attributes`, and UNC_WAIT.
type Probe = fn(&Path) -> Result<Option<u32>, u32>;

fn unc_probe() -> (Probe, Duration) {
    (attributes, UNC_WAIT)
}

/// The servers an earlier probe is still waiting on: not asked again until it ends.
static WAITING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Where a path the repo's own files name (a gitdir, an include, a common folder) leads, told
/// before anything opens it, from what the path resolves to rather than how it is spelled
/// (IN-VibeRepos-2, RIN4-1). Reads no file: the full path by name (GetFullPathNameW), the drive's
/// DOS device (QueryDosDeviceW) and type (GetDriveTypeW), and the attributes of each part on the
/// way, from the drive down, never through a link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// This machine: a local drive, no link on the way.
    Here,
    /// Maybe another machine's (a share, a mapped, substituted or unknown drive, a link on the
    /// way): asked through the bounded probe first. The key is what one question at a time is
    /// kept per (a server, a drive, a link).
    Ask(String),
    /// A device path (`\\.\...`, `\\?\GLOBALROOT\...`, a volume name, a device name such as
    /// `NUL`): never opened.
    Device,
}

pub fn place(p: &Path) -> Place {
    place_in(p, 0)
}

fn place_in(p: &Path, depth: usize) -> Place {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};
    // `\??\` (or `/??/`) is the NT namespace: CreateFileW and File::open hand it on as it is
    // (`\??\UNC\host\...` goes to the redirector), while GetFullPathNameW reads it as a folder on
    // the current drive. So it is a device path, whatever follows (review RF4-2).
    let head: Vec<u16> = p.as_os_str().encode_wide().take(4).collect();
    let slash = |c: u16| c == u16::from(b'\\') || c == u16::from(b'/');
    if head.len() == 4 && slash(head[0]) && head[1] == u16::from(b'?') && head[2] == u16::from(b'?') && slash(head[3]) {
        return Place::Device;
    }
    // By name only (`C:\x\nul` comes back as `\\.\nul`, a device); a verbatim path stays as it is.
    let full = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let Some(Component::Prefix(px)) = full.components().next() else { return Place::Ask("?".into()) };
    let letter = match px.kind() {
        Prefix::UNC(server, _) | Prefix::VerbatimUNC(server, _) => return Place::Ask(server.to_string_lossy().to_ascii_lowercase()),
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d.to_ascii_uppercase(),
        // `\\.\UNC\`, `\\?\GLOBALROOT\Device\Mup\`, `\\.\pipe\`, `\\?\Volume{...}\`, `\\.\C:\`
        Prefix::DeviceNS(_) | Prefix::Verbatim(_) => return Place::Device,
    };
    match drive_place(letter, depth) {
        Place::Here => {}
        other => return other,
    }
    match link_on_the_way(&full) {
        Some(link) => Place::Ask(format!("link {}", link.to_string_lossy().to_lowercase())),
        None => Place::Here,
    }
}

/// What a drive letter's DOS device names (QueryDosDeviceW's first string).
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// `subst`: the folder it stands for (`\??\C:\x`; `\??\UNC\s\x` as `\\s\x`).
    Subst(PathBuf),
    /// A volume of this machine (its drive type then says which kind).
    Volume,
    /// A network redirector (a mapped drive), or a device not known to be local: asked first.
    Other,
}

fn dos_target(t: &str) -> Target {
    if let Some(rest) = t.strip_prefix(r"\??\") {
        // `subst` makes only `X:\...` or `UNC\...`; anything else after `\??\` (a DefineDosDevice
        // target such as `GLOBALROOT\...` or a volume name) is not known to be local (RF4-3).
        if rest.get(..4).is_some_and(|u| u.eq_ignore_ascii_case(r"UNC\")) {
            return Target::Subst(PathBuf::from(format!(r"\\{}", &rest[4..])));
        }
        let b = rest.as_bytes();
        if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
            return Target::Subst(PathBuf::from(rest));
        }
        return Target::Other;
    }
    let l = t.to_ascii_lowercase();
    if [r"\device\harddiskvolume", r"\device\cdrom", r"\device\floppy"].iter().any(|v| l.starts_with(v)) { Target::Volume } else { Target::Other }
}

/// A drive letter's DOS device (QueryDosDeviceW's first string). None: no such drive, or it
/// cannot be told.
fn query_dos_device(letter: u8) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
    let name = crate::host::wide(&format!("{}:", letter as char));
    let mut buf = vec![0u16; 4096];
    // SAFETY: a NUL-terminated device name, a buffer and its length.
    let n = (unsafe { QueryDosDeviceW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize).min(buf.len());
    if n == 0 {
        return None;
    }
    let end = buf[..n].iter().position(|&c| c == 0).unwrap_or(n);
    Some(String::from_utf16_lossy(&buf[..end]))
}

fn dos_device(letter: u8) -> Option<String> {
    query_dos_device(letter)
}

fn drive_place(letter: u8, depth: usize) -> Place {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    let ask = || Place::Ask(format!("{}:", letter.to_ascii_lowercase() as char));
    // No such drive (asking it answers at once), or it cannot be told.
    let Some(device) = dos_device(letter) else { return ask() };
    match dos_target(&device) {
        Target::Subst(p) if depth < 4 => place_in(&p, depth + 1),
        Target::Volume => {
            let root = crate::host::wide(&format!("{}:\\", letter as char));
            // SAFETY: a NUL-terminated root path. 2 removable, 3 fixed, 5 CD, 6 RAM disk.
            match unsafe { GetDriveTypeW(root.as_ptr()) } {
                2 | 3 | 5 | 6 => Place::Here,
                _ => ask(),
            }
        }
        _ => ask(),
    }
}

/// The first part of `full` (an absolute path on a local drive) that is a reparse point (a
/// junction, symbolic link or mount point: it may lead to another machine), each part read
/// without following it, from the drive down. A part that cannot be read is in doubt too; one
/// that is not there ends the walk (nothing past it can be opened).
fn link_on_the_way(full: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut at = PathBuf::new();
    for c in full.components() {
        at.push(c.as_os_str());
        if matches!(c, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        match attributes(&at) {
            Ok(None) => return None,
            Ok(Some(a)) if a & ATTR_REPARSE == 0 => {}
            _ => return Some(at),
        }
    }
    None
}

/// Why a path the repo names is not opened now.
#[derive(Debug, PartialEq, Eq)]
pub enum NotNow {
    /// A device path: never.
    Device(String),
    /// Maybe another machine's, and it did not answer in time: Refresh asks again.
    Slow(String),
}

impl std::fmt::Display for NotNow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotNow::Device(w) | NotNow::Slow(w) => f.write_str(w),
        }
    }
}

fn device_words(path: &Path) -> String {
    format!("{} is a device path, which VibeRepos does not open", crate::text::shown_path(path))
}

fn slow_words(path: &Path) -> String {
    format!("{} is on a server that did not answer in {} s; Refresh asks it again", crate::text::shown_path(path), UNC_WAIT.as_secs())
}

/// `probe(path)` on a thread of its own, waited for at most `wait`. None: no answer in time, or
/// an earlier probe of the same server is still waiting (that thread ends with the network's
/// own timeout; at most one per server).
fn bounded(server: &str, path: &Path, wait: Duration, probe: Probe) -> Option<Result<Option<u32>, u32>> {
    let waiting = || WAITING.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut w = waiting();
        if w.iter().any(|s| s == server) {
            return None;
        }
        w.push(server.to_string());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let (s, p) = (server.to_string(), path.to_path_buf());
    let started = std::thread::Builder::new().name("unc-probe".into()).spawn(move || {
        let r = probe(&p);
        WAITING.lock().unwrap_or_else(|e| e.into_inner()).retain(|x| *x != s);
        let _ = tx.send(r);
    });
    if started.is_err() {
        waiting().retain(|x| x != server);
        return None;
    }
    rx.recv_timeout(wait).ok()
}

/// A path the repo's own files name (an include, a linked worktree's common folder) is asked
/// the way a gitdir is before anything opens it (IN-VibeRepos-2, RIN4-1): on this machine (by
/// `place`) not at all; maybe another machine's UNC_WAIT, one question per key at a time; a
/// device path is never opened.
pub fn unc_answers(path: &Path) -> Result<(), NotNow> {
    match place(path) {
        Place::Here => Ok(()),
        Place::Device => Err(NotNow::Device(device_words(path))),
        Place::Ask(key) => {
            let (probe, wait) = unc_probe();
            match bounded(&key, path, wait, probe) {
                Some(_) => Ok(()),
                None => Err(NotNow::Slow(slow_words(path))),
            }
        }
    }
}

/// A `.git` file: where it points, and what that makes the folder.
fn git_file(folder: &Path, file: &Path) -> (Kind, Option<PathBuf>, Option<PathBuf>) {
    let bytes = match crate::gitconfig::read_capped(file, discover::GIT_FILE_MAX) {
        Ok(Some(b)) => b,
        Ok(None) => return (Kind::BrokenFile("it went while it was read".into()), None, None),
        Err(e) => return (Kind::BrokenFile(e), None, None),
    };
    let gitdir = match discover::parse_git_file(&bytes, folder) {
        Ok(g) => g,
        Err(e) => return (Kind::BrokenFile(e), None, None),
    };
    // A gitdir on another machine: a host that does not answer would hold the walk for the
    // network's own timeout (about 21 s), so it gets UNC_WAIT. No answer yet is not "broken": a
    // NAS waking its disks is slow, not gone (`reprobe` asks again at the end of the walk).
    // Told by where the path leads (`place`), so a mapped drive or a link on the way is asked
    // too, and a device path (`\\.\UNC\...`) is never opened (RIN4-1).
    let attrs = match place(&gitdir) {
        Place::Here => attributes(&gitdir),
        Place::Device => return (Kind::BrokenFile(format!("it points to {}", device_words(&gitdir))), None, None),
        Place::Ask(key) => {
            let (probe, wait) = unc_probe();
            match bounded(&key, &gitdir, wait, probe) {
                Some(r) => r,
                None => return (Kind::Unchecked(format!("it points to {}, whose server did not answer in {} s; Refresh asks it again", crate::text::shown_path(&gitdir), UNC_WAIT.as_secs())), Some(gitdir), None),
            }
        }
    };
    match attrs {
        Ok(Some(a)) if a & ATTR_DIRECTORY != 0 => {}
        Ok(_) => return (Kind::BrokenFile(format!("it points to {}, which is not there", crate::text::shown_path(&gitdir))), Some(gitdir), None),
        Err(e) => return (Kind::BrokenFile(format!("{}: {}", crate::text::shown_path(&gitdir), error_text(e))), Some(gitdir), None),
    }
    if let Ok(Some(b)) = crate::gitconfig::read_capped(&gitdir.join("commondir"), 4096)
        && let Some(common) = discover::parse_commondir(&b, &gitdir)
    {
        match unc_answers(&common) {
            Ok(()) => {}
            Err(e @ NotNow::Slow(_)) => return (Kind::Unchecked(format!("its common git folder {e}")), Some(gitdir), None),
            Err(e @ NotNow::Device(_)) => return (Kind::BrokenFile(format!("its common git folder {e}")), Some(gitdir), None),
        }
        let main = discover::owner_of(&common);
        return (Kind::Worktree { main }, Some(gitdir), Some(common));
    }
    if let Some(parent) = discover::submodule_parent(&gitdir) {
        return (Kind::Submodule { parent }, Some(gitdir.clone()), Some(gitdir));
    }
    (Kind::Separate, Some(gitdir.clone()), Some(gitdir))
}

/// The `.git` files left unchecked because their server was slow, asked once more now that the
/// local work is done: each such server's first probe gets one more wait to end, then each
/// folder is examined again. One still not answered stays unchecked.
fn reprobe(found: &mut [Found]) {
    let pending: Vec<usize> = (0..found.len()).filter(|&i| matches!(found[i].kind, Kind::Unchecked(_))).collect();
    if pending.is_empty() {
        return;
    }
    let servers: Vec<String> = pending
        .iter()
        .filter_map(|&i| match found[i].gitdir.as_deref().map(place) {
            Some(Place::Ask(key)) => Some(key),
            _ => None,
        })
        .collect();
    let (_, wait) = unc_probe();
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline && WAITING.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|s| servers.contains(s)) {
        std::thread::sleep(Duration::from_millis(50));
    }
    for i in pending {
        let f = &mut found[i];
        let (kind, gitdir, common) = git_file(&f.path, &f.path.join(".git"));
        (f.kind, f.gitdir, f.common) = (kind, gitdir, common);
    }
}
