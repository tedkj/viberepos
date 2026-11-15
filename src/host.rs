//! The Windows side: the app's folders, the shell (web links, Explorer, a terminal), the
//! clipboard, the system's light or dark mode, the locale's CSV separators, local time, and who
//! owns a folder. The same small set every VibeWare app carries (copied, not shared).

use std::path::{Path, PathBuf};

/// The app's own folders: settings in %APPDATA%, bulkier local state in %LOCALAPPDATA%.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub local_data: PathBuf,
}

pub const APP_ID: &str = "com.tedkj.viberepos";

pub fn paths() -> Paths {
    use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_RoamingAppData};
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from).unwrap_or_default();
    Paths {
        config: known_folder(&FOLDERID_RoamingAppData).unwrap_or_else(|| env("APPDATA")).join(APP_ID),
        local_data: known_folder(&FOLDERID_LocalAppData).unwrap_or_else(|| env("LOCALAPPDATA")).join(APP_ID),
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

pub fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    // SAFETY: an out pointer the shell fills and we free once.
    unsafe {
        let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
        let ok = SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) == 0;
        let out = if ok && !p.is_null() {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            Some(path_from_wide(std::slice::from_raw_parts(p, len)))
        } else {
            None
        };
        CoTaskMemFree(p as *const _);
        out
    }
}

/// The user's profile folder (`%USERPROFILE%`).
pub fn profile() -> Option<PathBuf> {
    use windows_sys::Win32::UI::Shell::FOLDERID_Profile;
    known_folder(&FOLDERID_Profile).or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// A web page in the default browser (Get Git, About's link). https only: nothing else is ever
/// handed to the shell from here.
pub fn open_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err(format!("Not a web address: {url}"));
    }
    shell_open(url)
}

/// Windows' own folder (`GetSystemWindowsDirectoryW`) or System32 (`GetSystemDirectoryW`), as
/// Windows names them: never from the environment, never relative (the template's; the install
/// check's tests find a signed Windows file and PowerShell with it).
pub fn windows_dir(system: bool) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetSystemWindowsDirectoryW};
    let mut buf = [0u16; 1024];
    // SAFETY: a buffer and its length in units.
    let n = unsafe { if system { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } else { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]))).filter(|p| p.is_absolute())
}

/// Explorer by its full path in the Windows folder: a bare "explorer.exe" is looked for in this
/// exe's own (user-writable) folder first (VibeServe SERVE-6).
pub fn explorer() -> Option<PathBuf> {
    use windows_sys::Win32::UI::Shell::FOLDERID_Windows;
    known_folder(&FOLDERID_Windows).filter(|w| w.is_absolute()).map(|w| w.join("explorer.exe"))
}

/// A folder in Explorer, from its raw path, quoted (VibeServe's `reveal`). Started without any
/// of this process's handles.
pub fn reveal(path: &Path, select: bool) -> Result<(), String> {
    let exe = explorer().ok_or("Could not open Explorer: the Windows folder could not be read")?;
    // Explorer reads its own command line: the whole quoted argument is passed as it is.
    let mut cl = crate::run::command_line(&exe, &[]);
    cl.push(b' ' as u16);
    use std::os::windows::ffi::OsStrExt;
    cl.extend(reveal_arg(path, select).encode_wide());
    spawn_raw(&exe, cl, None).map_err(|e| format!("Could not open Explorer: {e}"))
}

/// Explorer parses its own command line: the path goes in quotes (after /select, when
/// selecting). Built from the path's exact units, never a lossy string (a name with a lone
/// surrogate must still be found: VibeRename RR-7); `"` cannot be in a Windows name.
pub fn reveal_arg(path: &Path, select: bool) -> std::ffi::OsString {
    let mut arg = std::ffi::OsString::from(if select { "/select,\"" } else { "\"" });
    arg.push(path.as_os_str());
    arg.push("\"");
    arg
}

/// A process from a full path and a command line as given, without handles, not waited for.
fn spawn_raw(exe: &Path, mut cl: Vec<u16>, cwd: Option<&Path>) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};
    cl.push(0);
    let app = wide_path(exe);
    let cwd_w = cwd.map(wide_path);
    // SAFETY: zeroed is valid for both structs.
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated buffers alive for the call; no handles inherited.
    let ok = unsafe {
        CreateProcessW(app.as_ptr(), cl.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT, std::ptr::null(), cwd_w.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()), &si, &mut pi)
    };
    if ok == 0 {
        return Err(crate::walk::error_words(&std::io::Error::last_os_error()));
    }
    // SAFETY: both handles came from CreateProcessW; closed once.
    unsafe {
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }
    Ok(())
}

/// The longest current folder a new process can be given (MAX_PATH less the NUL and the
/// trailing backslash Windows adds).
pub const MAX_CWD: usize = 258;

/// The terminal for "Open terminal": Windows Terminal's alias when it is there, else Windows
/// PowerShell, by full path.
pub fn terminal() -> Option<(PathBuf, Vec<std::ffi::OsString>)> {
    use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;
    let wt = known_folder(&FOLDERID_LocalAppData).map(|l| l.join("Microsoft").join("WindowsApps").join("wt.exe"));
    if let Some(wt) = wt.filter(|w| w.exists()) {
        return Some((wt, vec!["-d".into(), ".".into()]));
    }
    let ps = crate::run::neutral_cwd().join("WindowsPowerShell").join("v1.0").join("powershell.exe");
    ps.exists().then(|| (ps, vec!["-NoLogo".into()]))
}

/// A terminal in `dir`: the folder is the process's current folder, never part of a command
/// line. Err: in words (a path too long for a current folder included).
pub fn open_terminal(dir: &Path) -> Result<(), String> {
    if crate::walk::units(dir).len() > MAX_CWD {
        return Err("Too long for a terminal's current folder".into());
    }
    let (exe, args) = terminal().ok_or("No terminal found (Windows Terminal or Windows PowerShell)")?;
    crate::run::spawn_detached(&exe, &args, Some(dir)).map_err(|e| format!("Could not open a terminal: {e}"))
}

/// Random bytes from the system's preferred generator.
pub fn random_bytes(buf: &mut [u8]) {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
    // SAFETY: a buffer of exactly that many bytes; no algorithm handle with this flag.
    let r = unsafe { BCryptGenRandom(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if r != 0 {
        // Never fails with these arguments; a fallback still differs per run.
        let t = now_unix_ms() as u64 ^ (std::process::id() as u64) << 32;
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (t >> ((i % 8) * 8)) as u8;
        }
    }
}

/// Who owns a folder: `DOMAIN\user` (or the SID when it has no name here).
pub fn owner_of(path: &Path) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{LookupAccountSidW, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE};
    let p = wide_path(path);
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated name; out pointers; the descriptor is freed below.
    let r = unsafe { GetNamedSecurityInfoW(p.as_ptr(), SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION, &mut owner, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut sd) };
    if r != 0 || owner.is_null() {
        return None;
    }
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut nl, mut dl) = (name.len() as u32, domain.len() as u32);
    let mut use_: SID_NAME_USE = 0;
    // SAFETY: the SID lives in `sd`; buffers with their lengths.
    let looked = unsafe { LookupAccountSidW(std::ptr::null(), owner, name.as_mut_ptr(), &mut nl, domain.as_mut_ptr(), &mut dl, &mut use_) } != 0;
    let out = if looked {
        let n = String::from_utf16_lossy(&name[..nl as usize]);
        let d = String::from_utf16_lossy(&domain[..dl as usize]);
        Some(if d.is_empty() { n } else { format!("{d}\\{n}") })
    } else {
        let mut s: windows_sys::core::PWSTR = std::ptr::null_mut();
        // SAFETY: the SID in `sd`; the string is freed with LocalFree.
        if unsafe { ConvertSidToStringSidW(owner, &mut s) } != 0 && !s.is_null() {
            // SAFETY: a NUL-terminated string from the call above.
            let len = unsafe { (0..).take_while(|&i| *s.add(i) != 0).count() };
            let t = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, len) });
            unsafe { LocalFree(s as _) };
            Some(t)
        } else {
            None
        }
    };
    // SAFETY: allocated by GetNamedSecurityInfoW, freed once.
    unsafe { LocalFree(sd as _) };
    out
}

/// A path Windows handed over as UTF-16 units, kept exactly (a lone surrogate included).
pub fn path_from_wide(units: &[u16]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_wide(units))
}

/// ShellExecuteEx with the shell's own error dialogs suppressed: a failure comes back as an
/// error here, it never parks a modal box on the UI.
fn shell_open(target: &str) -> Result<(), String> {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let file = wide(target);
    // SAFETY: COM on this thread; the struct points at a live NUL-terminated buffer.
    unsafe {
        CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC;
        info.lpFile = file.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) == 0 {
            return Err(format!("Could not open {target}: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Text onto the clipboard (copies, paths, the safe.directory command).
pub fn copy_text(text: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    copy_units(&clipboard_text(text).encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>(), |w| {
        let fail = |what: &str| format!("Could not copy ({what}): {}", std::io::Error::last_os_error());
        // SAFETY: the clipboard is open (copy_units); memory handed over on success only.
        unsafe {
            if EmptyClipboard() == 0 {
                return Err(fail("empty"));
            }
            let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
            if h.is_null() {
                return Err(fail("memory"));
            }
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                let e = fail("memory");
                GlobalFree(h);
                return Err(e);
            }
            std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
            GlobalUnlock(h);
            if SetClipboardData(CF_UNICODETEXT as u32, h).is_null() {
                let e = fail("set");
                GlobalFree(h);
                return Err(e);
            }
            Ok(())
        }
    })
}

/// A path's exact units onto the clipboard (Copy path: the raw path, a lone surrogate kept).
pub fn copy_path(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let w: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let text: String = String::from_utf16_lossy(&w[..w.len() - 1]);
    if String::from_utf16(&w[..w.len() - 1]).is_ok() {
        return copy_text(&text);
    }
    // Not valid UTF-16: put the units on as they are.
    copy_units(&w, |w| {
        use windows_sys::Win32::Foundation::GlobalFree;
        use windows_sys::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
        use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
        use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
        // SAFETY: as in copy_text.
        unsafe {
            EmptyClipboard();
            let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
            if h.is_null() {
                return Err("Could not copy (memory)".into());
            }
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                GlobalFree(h);
                return Err("Could not copy (memory)".into());
            }
            std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
            GlobalUnlock(h);
            if SetClipboardData(CF_UNICODETEXT as u32, h).is_null() {
                GlobalFree(h);
                return Err("Could not copy (set)".into());
            }
            Ok(())
        }
    })
}

/// Open the clipboard (a few short tries: a clipboard manager may hold it), run `put`, close it.
fn copy_units(w: &[u16], put: impl FnOnce(&[u16]) -> Result<(), String>) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};
    // SAFETY: no window; closed below.
    let mut open = unsafe { OpenClipboard(std::ptr::null_mut()) } != 0;
    for _ in 0..4 {
        if open {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
        open = unsafe { OpenClipboard(std::ptr::null_mut()) } != 0;
    }
    if !open {
        return Err(format!("Could not copy (the clipboard is busy): {}", std::io::Error::last_os_error()));
    }
    let r = put(w);
    // SAFETY: opened above.
    unsafe { CloseClipboard() };
    r
}

/// Windows text on the clipboard ends its lines with CRLF (Notepad and others expect it).
pub fn clipboard_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Windows' app mode (Settings > Personalization > Colors), dark when it cannot be read.
pub fn system_dark() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = wide("AppsUseLightTheme");
    let mut data: u32 = 1;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: NUL-terminated names; a DWORD out with its size.
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut data as *mut u32).cast(), &mut size) };
    r != 0 || data == 0
}

/// The user's list separator and decimal symbol (Region > Additional settings), the ones Excel
/// opens a CSV with; checked by `CsvFormat::from_locale`.
pub fn csv_format() -> crate::export::CsvFormat {
    use windows_sys::Win32::Globalization::{GetLocaleInfoEx, LOCALE_SDECIMAL, LOCALE_SLIST};
    let get = |what: u32| -> String {
        let mut buf = [0u16; 16];
        // SAFETY: a buffer of the size given; a null locale name is the user's default.
        let n = unsafe { GetLocaleInfoEx(std::ptr::null(), what, buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1).min(buf.len())])
    };
    crate::export::CsvFormat::from_locale(&get(LOCALE_SLIST), &get(LOCALE_SDECIMAL))
}

/// Now, Unix milliseconds.
pub fn now_unix_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// A moment split into its calendar parts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Clock {
    /// 14:02:11
    pub fn time(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }
    /// 14:02
    pub fn hm(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
    /// 2026-10-23 14:02
    pub fn minutes(&self) -> String {
        format!("{:04}-{:02}-{:02} {:02}:{:02}", self.year, self.month, self.day, self.hour, self.minute)
    }
    /// 2026-10-23
    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Unix seconds as this computer's local time (its daylight saving rules for that date) and the
/// offset from UTC in minutes.
pub fn local(unix: i64) -> (Clock, i32) {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ft = (unix.max(0) as u64) * 10_000_000 + 116_444_736_000_000_000;
    let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    // SAFETY: plain structs in and out; a null time zone is the current one.
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut loc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let ok = unsafe { FileTimeToSystemTime(&f, &mut utc) } != 0;
    let lok = ok && unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) } != 0;
    let s = if lok { loc } else { utc };
    let c = Clock { year: s.wYear, month: s.wMonth as u8, day: s.wDay as u8, hour: s.wHour as u8, minute: s.wMinute as u8, second: s.wSecond as u8 };
    let offset = if lok { minutes_between(&utc, &loc) } else { 0 };
    (c, offset)
}

fn minutes_between(utc: &windows_sys::Win32::Foundation::SYSTEMTIME, loc: &windows_sys::Win32::Foundation::SYSTEMTIME) -> i32 {
    let days = |s: &windows_sys::Win32::Foundation::SYSTEMTIME| crate::export::days_from_civil(s.wYear as i64, s.wMonth as i64, s.wDay as i64);
    let m = |s: &windows_sys::Win32::Foundation::SYSTEMTIME| days(s) * 1440 + s.wHour as i64 * 60 + s.wMinute as i64;
    (m(loc) - m(utc)) as i32
}

/// The window in front (owner of the pickers); 0 when none.
pub fn active_window() -> isize {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    // SAFETY: no arguments.
    unsafe { GetActiveWindow() as isize }
}

/// A rectangle in physical pixels, edges as Win32's RECT has them (right and bottom exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Px {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Where a window's visible frame goes so it fits the monitor's work area (X15A11Y-3): a side
/// longer than the area's shrinks to it, then the frame moves inside the area (left and top edges
/// first). None when it already fits. The window's minimum size still wins: Windows keeps it
/// (WM_GETMINMAXINFO), so a window whose minimum is bigger than the area ends up at its minimum.
pub fn fit_frame(frame: Px, work: Px) -> Option<Px> {
    let w = (frame.right - frame.left).min(work.right - work.left);
    let h = (frame.bottom - frame.top).min(work.bottom - work.top);
    let left = frame.left.min(work.right - w).max(work.left);
    let top = frame.top.min(work.bottom - h).max(work.top);
    let fitted = Px { left, top, right: left + w, bottom: top + h };
    (fitted != frame).then_some(fitted)
}

/// A monitor's work area in workspace coordinates, the ones a window's restore rectangle
/// (WINDOWPLACEMENT) is in: (0, 0) is the corner of the primary monitor's work area, so a
/// taskbar at the top or left of the primary monitor shifts every monitor's area by its size.
pub fn workspace_area(work: Px, primary_monitor: Px, primary_work: Px) -> Px {
    let (dx, dy) = (primary_work.left - primary_monitor.left, primary_work.top - primary_monitor.top);
    Px { left: work.left - dx, top: work.top - dy, right: work.right - dx, bottom: work.bottom - dy }
}

/// Fit a window that has just opened into its monitor's work area (X15A11Y-3; `fit_frame`):
/// the window opens at its preferred size, sized for a big screen at 100 %, which on a 1080p
/// laptop at 150 % runs past the taskbar and puts the toasts and the status line out of sight.
/// All in physical pixels (the process is per-monitor DPI aware, so no scaling): the work area of
/// the monitor the window is on, and its visible frame (DWM's, without the invisible resize
/// borders, which go back on around it). A maximized start gets its restore size fitted instead,
/// and a minimized one waits until it is restored (X15RFTP-4). None while the window is not on
/// screen yet or minimized (try again later); else whether it changed.
pub fn fit_to_work_area(hwnd: isize) -> Option<bool> {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, MonitorFromWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowPlacement, GetWindowRect, IsIconic, IsWindowVisible, IsZoomed, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPlacement, SetWindowPos, WINDOWPLACEMENT};
    let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
    let px = |r: RECT| Px { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
    // SAFETY: plain Win32 calls on the app's own window handle, each with a struct of its own
    // size; a call that fails leaves the window as it is.
    unsafe {
        let monitor = |m: HMONITOR| -> Option<(Px, Px)> {
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = size_of::<MONITORINFO>() as u32;
            (GetMonitorInfoW(m, &mut mi) != 0).then(|| (px(mi.rcMonitor), px(mi.rcWork)))
        };
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return None;
        }
        let Some((_, work)) = monitor(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)) else { return Some(false) };
        if IsZoomed(hwnd) != 0 {
            let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
            wp.length = size_of::<WINDOWPLACEMENT>() as u32;
            let Some((pm, pw)) = monitor(MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)) else { return Some(false) };
            if GetWindowPlacement(hwnd, &mut wp) == 0 {
                return Some(false);
            }
            let Some(to) = fit_frame(px(wp.rcNormalPosition), workspace_area(work, pm, pw)) else { return Some(false) };
            wp.rcNormalPosition = RECT { left: to.left, top: to.top, right: to.right, bottom: to.bottom };
            return Some(SetWindowPlacement(hwnd, &wp) != 0);
        }
        let mut outer: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut outer) == 0 {
            return Some(false);
        }
        let mut seen = outer;
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS as u32, (&mut seen as *mut RECT).cast(), size_of::<RECT>() as u32) != 0 {
            seen = outer;
        }
        let Some(to) = fit_frame(px(seen), work) else { return Some(false) };
        let (l, t, r, b) = (seen.left - outer.left, seen.top - outer.top, outer.right - seen.right, outer.bottom - seen.bottom);
        Some(SetWindowPos(hwnd, std::ptr::null_mut(), to.left - l, to.top - t, to.right - to.left + l + r, to.bottom - to.top + t + b, SWP_NOZORDER | SWP_NOACTIVATE) != 0)
    }
}
