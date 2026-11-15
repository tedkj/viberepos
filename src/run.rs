//! Every git process (SPEC section 3): started by its full path with `CREATE_NO_WINDOW`, stdin
//! the NUL device, in a job object of its own with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (git's own
//! children go with it), stdout and stderr read up to their caps (16 MiB, 64 KiB), killed after
//! its time. The child inherits exactly its three handles (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`):
//! with four readers starting git at once, no git keeps another's pipe open.
//!
//! The environment is the user's own minus everything that would point git somewhere else, plus
//! the switches that keep it read-only, quiet and in English (`git_env`), and the arguments put
//! the repo's own hooks and fsmonitor out of reach (`git_args`).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use windows_sys::Win32::Foundation::HANDLE;

pub const OUT_CAP: usize = 16 * 1024 * 1024;
pub const ERR_CAP: usize = 64 * 1024;
pub const READ_TIMEOUT: Duration = Duration::from_secs(20);
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// How a process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    Exited(u32),
    /// Killed after its time.
    TimedOut,
    /// Killed when stdout passed its cap.
    TooBig,
    /// It could not be started: why.
    NotStarted(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub ended: Ended,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.ended == Ended::Exited(0)
    }
}

/// What to start.
pub struct Spec<'a> {
    pub exe: &'a Path,
    pub args: &'a [OsString],
    pub env: &'a [(OsString, OsString)],
    /// The process's own current folder (git works in `-C <repo>`; this is a neutral place).
    pub cwd: &'a Path,
    pub timeout: Duration,
    pub out_cap: usize,
    pub err_cap: usize,
}

// ---------- the environment and the arguments (pure) ----------

/// Variables that would make git read or write another repo, index, object store or config
/// than the one asked for, or trace to a file (SPEC section 3).
const SCRUB: [&str; 12] = [
    "GIT_DIR",
    // `git config --list` would read only this file (VibeRepos lists the filters with it).
    "GIT_CONFIG",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];
const SCRUB_PREFIX: [&str; 3] = ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_", "GIT_TRACE"];

/// The environment git runs in: `base` (the user's own) without the redirecting variables
/// (names compared as Windows does, ignoring case), with `GIT_OPTIONAL_LOCKS=0` (status leaves
/// the index alone), no prompts (`GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`), English
/// messages (`LC_ALL=C`), `GIT_LFS_SKIP_SMUDGE=1` (git-lfs, should it ever run, downloads
/// nothing), and `GIT_CEILING_DIRECTORIES` at the repo's parent, so git never looks
/// above the folder it is asked about. A fetch also turns every askpass program off (empty
/// `GIT_ASKPASS` and `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE=never`): no sign-in window opens.
pub fn git_env(base: impl IntoIterator<Item = (OsString, OsString)>, ceiling: Option<&Path>, fetch: bool) -> Vec<(OsString, OsString)> {
    let mut set: Vec<(&str, OsString)> = vec![("GIT_OPTIONAL_LOCKS", "0".into()), ("GIT_TERMINAL_PROMPT", "0".into()), ("GCM_INTERACTIVE", "never".into()), ("LC_ALL", "C".into()), ("GIT_LFS_SKIP_SMUDGE", "1".into())];
    if let Some(c) = ceiling {
        set.push(("GIT_CEILING_DIRECTORIES", c.as_os_str().to_os_string()));
    }
    if fetch {
        set.extend([("GIT_ASKPASS", OsString::new()), ("SSH_ASKPASS", OsString::new()), ("SSH_ASKPASS_REQUIRE", "never".into())]);
    }
    let upper = |k: &OsStr| k.to_string_lossy().to_ascii_uppercase();
    let mut out: Vec<(OsString, OsString)> = base
        .into_iter()
        .filter(|(k, _)| {
            let u = upper(k);
            !SCRUB.contains(&u.as_str()) && !SCRUB_PREFIX.iter().any(|p| u.starts_with(p)) && !set.iter().any(|(s, _)| *s == u)
        })
        .collect();
    out.extend(set.into_iter().map(|(k, v)| (OsString::from(k), v)));
    out
}

/// Git LFS's filter switched off on every call, wherever it is configured (Git for Windows puts
/// it in the system config with `required = true`): its clean filter, run by status on a file
/// whose time changed, writes into `.git` (`lfs\objects`, `lfs\tmp`, an `[lfs]` section in
/// `config`).
pub const LFS_OFF: [&str; 8] = ["-c", "filter.lfs.clean=", "-c", "filter.lfs.smudge=", "-c", "filter.lfs.process=", "-c", "filter.lfs.required=false"];

/// `git -C <repo> -c core.fsmonitor=false -c core.hooksPath=<none> <LFS_OFF> <overrides>
/// <command>`.
pub fn git_args(repo: &Path, hooks: &Path, overrides: &[String], command: &[&str]) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec!["-C".into(), repo.as_os_str().to_os_string(), "-c".into(), "core.fsmonitor=false".into(), "-c".into()];
    let mut h = OsString::from("core.hooksPath=");
    h.push(hooks.as_os_str());
    a.push(h);
    a.extend(LFS_OFF.iter().map(OsString::from));
    a.extend(overrides.iter().map(OsString::from));
    a.extend(command.iter().map(OsString::from));
    a
}

/// One argument quoted the way the C runtime (and git for Windows) splits a command line.
pub fn quote_arg(arg: &OsStr, out: &mut Vec<u16>) {
    use std::os::windows::ffi::OsStrExt;
    let units: Vec<u16> = arg.encode_wide().collect();
    let plain = !units.is_empty() && !units.iter().any(|&c| matches!(c, 0x20 | 0x09 | 0x0A | 0x0B | 0x22));
    if plain {
        out.extend_from_slice(&units);
        return;
    }
    out.push(b'"' as u16);
    let mut slashes = 0usize;
    for &c in &units {
        if c == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        if c == b'"' as u16 {
            // Backslashes before a quote are doubled, and the quote escaped.
            out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n(b'\\' as u16, slashes));
        }
        slashes = 0;
        out.push(c);
    }
    // Backslashes before the closing quote are doubled.
    out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    out.push(b'"' as u16);
}

pub fn command_line(exe: &Path, args: &[OsString]) -> Vec<u16> {
    let mut out = Vec::new();
    quote_arg(exe.as_os_str(), &mut out);
    for a in args {
        out.push(b' ' as u16);
        quote_arg(a, &mut out);
    }
    out
}

/// The block CreateProcessW takes: `name=value` pairs sorted by name (as Windows keeps them),
/// each NUL-ended, the block ended by another NUL.
pub fn env_block(env: &[(OsString, OsString)]) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut pairs: Vec<&(OsString, OsString)> = env.iter().collect();
    pairs.sort_by_key(|(k, _)| k.to_string_lossy().to_ascii_uppercase());
    let mut out = Vec::new();
    for (k, v) in pairs {
        out.extend(k.encode_wide());
        out.push(b'=' as u16);
        out.extend(v.encode_wide());
        out.push(0);
    }
    if out.is_empty() {
        out.push(0);
    }
    out.push(0);
    out
}

// ---------- the process ----------

#[derive(Clone, Copy)]
struct Handle(HANDLE);
// SAFETY: a kernel handle is usable from any thread; each is closed once, by its owner.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

/// A handle closed when it goes out of scope.
struct Owned(HANDLE);
// SAFETY: a kernel handle owned by one value at a time, closed once by its Drop.
unsafe impl Send for Owned {}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            // SAFETY: a handle this struct owns, closed once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", crate::walk::error_words(&std::io::Error::last_os_error()))
}

/// A job that kills everything in it when its last handle closes, and keeps a crash of git's
/// from opening Windows' error dialog.
fn new_job() -> Result<Owned, String> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    // SAFETY: no name, default security; the handle is owned below.
    let job = Owned(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) });
    if job.0.is_null() {
        return Err(last_error("no job object"));
    }
    // SAFETY: zeroed is valid for the struct; its size passed.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    // SAFETY: the job just made; the struct and its size.
    let ok = unsafe { SetInformationJobObject(job.0, JobObjectExtendedLimitInformation, (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) };
    if ok == 0 {
        return Err(last_error("no job limits"));
    }
    Ok(job)
}

/// Read a pipe to its end, keeping at most `cap` bytes; past the cap `over` is set and the job
/// killed (stdout only: stderr past its cap is just cut).
fn drain(pipe: Owned, cap: usize, kill_over: Option<(Handle, Arc<AtomicBool>)>) -> Vec<u8> {
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut got = 0u32;
        // SAFETY: a buffer of its length; the pipe is open until `pipe` drops.
        let ok = unsafe { ReadFile(pipe.0, buf.as_mut_ptr(), buf.len() as u32, &mut got, std::ptr::null_mut()) } != 0;
        if !ok || got == 0 {
            break;
        }
        let room = cap.saturating_sub(out.len());
        out.extend_from_slice(&buf[..(got as usize).min(room)]);
        if got as usize > room
            && let Some((job, over)) = &kill_over
            && !over.swap(true, Ordering::SeqCst)
        {
            // SAFETY: the job stays open until both readers are done (run waits for them).
            unsafe { TerminateJobObject(job.0, 1) };
        }
    }
    out
}

/// Start, wait, collect. Never panics; a failure to start is `Ended::NotStarted`.
pub fn run(spec: &Spec) -> Output {
    match start(spec) {
        Ok(o) => o,
        Err(e) => Output { ended: Ended::NotStarted(e), stdout: Vec::new(), stderr: Vec::new() },
    }
}

fn start(spec: &Spec) -> Result<Output, String> {
    use windows_sys::Win32::Foundation::{GetLastError, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_TIMEOUT};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, TerminateJobObject};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
    };
    if !spec.exe.is_absolute() {
        return Err("git's path is not absolute".into());
    }
    let job = new_job()?;
    let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: std::ptr::null_mut(), bInheritHandle: 1 };
    let pipe = |what: &str| -> Result<(Owned, Owned), String> {
        let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: two out handles and inheritable attributes; owned right after.
        if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
            return Err(last_error(what));
        }
        let (r, w) = (Owned(r), Owned(w));
        // Our end stays ours: only the child's end is inherited.
        // SAFETY: the read end just made.
        unsafe { SetHandleInformation(r.0, HANDLE_FLAG_INHERIT, 0) };
        Ok((r, w))
    };
    let (out_r, out_w) = pipe("no stdout pipe")?;
    let (err_r, err_w) = pipe("no stderr pipe")?;
    let nul = crate::host::wide("NUL");
    // SAFETY: a NUL-terminated name; inheritable; owned below.
    let stdin = Owned(unsafe { CreateFileW(nul.as_ptr(), FILE_GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, &sa, OPEN_EXISTING, 0, std::ptr::null_mut()) });
    if stdin.0 == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(last_error("no NUL device"));
    }
    // The attribute list: exactly these three handles are inherited.
    let handles: [HANDLE; 3] = [stdin.0, out_w.0, err_w.0];
    let mut size = 0usize;
    // SAFETY: the documented size query (fails with ERROR_INSUFFICIENT_BUFFER, sets `size`).
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
    let mut list_buf = vec![0u64; size.div_ceil(8).max(1)];
    let list = list_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    // SAFETY: a buffer of at least `size` bytes, 8-aligned.
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
        return Err(last_error("no attribute list"));
    }
    struct ListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for ListGuard {
        fn drop(&mut self) {
            // SAFETY: initialised above, deleted once.
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _list_guard = ListGuard(list);
    // SAFETY: the list initialised above; `handles` outlives CreateProcessW below.
    if unsafe { UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, handles.as_ptr().cast(), size_of_val(&handles), std::ptr::null_mut(), std::ptr::null()) } == 0 {
        return Err(last_error("no handle list"));
    }
    // SAFETY: zeroed is valid for the struct.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdin.0;
    si.StartupInfo.hStdOutput = out_w.0;
    si.StartupInfo.hStdError = err_w.0;
    si.lpAttributeList = list;
    let app = crate::host::wide_path(spec.exe);
    let mut cmd = command_line(spec.exe, spec.args);
    cmd.push(0);
    let env = env_block(spec.env);
    let cwd = crate::host::wide_path(spec.cwd);
    // SAFETY: zeroed is valid for the struct.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every buffer is NUL-terminated and alive for the call; the command line is
    // writable as CreateProcessW wants; the handles in the list are open and inheritable.
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            env.as_ptr().cast(),
            cwd.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        )
    };
    if ok == 0 {
        // SAFETY: right after the failing call.
        let code = unsafe { GetLastError() };
        return Err(format!("could not start {}: {}", crate::text::shown_path(spec.exe), crate::walk::error_text(code)));
    }
    let process = Owned(pi.hProcess);
    let thread = Owned(pi.hThread);
    // The child holds its own copies now.
    drop((stdin, out_w, err_w));
    // SAFETY: the job and the suspended process, both open.
    if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
        let e = last_error("could not put git in a job object");
        // SAFETY: the process we just started, never resumed.
        unsafe { TerminateProcess(process.0, 1) };
        return Err(e);
    }
    // SAFETY: the suspended main thread.
    unsafe { ResumeThread(thread.0) };
    drop(thread);
    let over = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
    let (jh, ov, cap) = (Handle(job.0), over.clone(), spec.out_cap);
    let t1 = tx.clone();
    std::thread::spawn(move || {
        let r = out_r;
        let _ = t1.send((true, drain(r, cap, Some((jh, ov)))));
    });
    let ecap = spec.err_cap;
    std::thread::spawn(move || {
        let r = err_r;
        let _ = tx.send((false, drain(r, ecap, None)));
    });
    let ms = spec.timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
    // SAFETY: the process handle is open.
    let timed_out = unsafe { WaitForSingleObject(process.0, ms) } == WAIT_TIMEOUT;
    if timed_out {
        // SAFETY: the job is open; this ends git and everything it started.
        unsafe { TerminateJobObject(job.0, 1) };
        // SAFETY: as above.
        unsafe { WaitForSingleObject(process.0, 5000) };
    }
    // The pipes end when every process holding them has ended. One git left behind (a helper
    // still running) is ended after a moment, which ends the pipes too.
    let mut stdout = None;
    let mut stderr = None;
    let mut deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut killed = timed_out;
    while stdout.is_none() || stderr.is_none() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((true, b)) => stdout = Some(b),
            Ok((false, b)) => stderr = Some(b),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !killed => {
                // SAFETY: the job is open.
                unsafe { TerminateJobObject(job.0, 1) };
                killed = true;
                deadline = std::time::Instant::now() + Duration::from_secs(5);
            }
            Err(_) => break, // a reader that never ends is left; its pipe ends with the job
        }
    }
    let mut code = 0u32;
    // SAFETY: the process handle is open; one u32 out.
    unsafe { GetExitCodeProcess(process.0, &mut code) };
    let ended = if timed_out {
        Ended::TimedOut
    } else if over.load(Ordering::SeqCst) {
        Ended::TooBig
    } else {
        Ended::Exited(code)
    };
    drop(job); // KILL_ON_JOB_CLOSE: nothing of git's outlives this call
    Ok(Output { ended, stdout: stdout.unwrap_or_default(), stderr: stderr.unwrap_or_default() })
}

/// A neutral current folder for git (it works in `-C <repo>`): Windows' System32, never a repo
/// and not writable by the user.
pub fn neutral_cwd() -> PathBuf {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 512];
    // SAFETY: a buffer and its length.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return PathBuf::from("C:\\Windows\\System32");
    }
    crate::host::path_from_wide(&buf[..n])
}

/// Start a program the user asked for (Explorer, a terminal) without handing it any of this
/// process's handles (a git pipe inherited by a terminal would stay open as long as it runs),
/// and without waiting for it.
pub fn spawn_detached(exe: &Path, args: &[OsString], cwd: Option<&Path>) -> Result<(), String> {
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};
    let app = crate::host::wide_path(exe);
    let mut cmd = command_line(exe, args);
    cmd.push(0);
    let cwd_w = cwd.map(crate::host::wide_path);
    // SAFETY: zeroed is valid for both structs.
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated buffers alive for the call; no handles inherited.
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT,
            std::ptr::null(),
            cwd_w.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(crate::walk::error_words(&std::io::Error::last_os_error()));
    }
    drop((Owned(pi.hProcess), Owned(pi.hThread)));
    Ok(())
}
