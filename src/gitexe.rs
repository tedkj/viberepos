//! Finding git.exe (SPEC section 1): found, never bundled. On a worker at start: the PATH of the
//! environment Windows builds for this user now (`CreateEnvironmentBlock`, so a git installed
//! after this window opened counts), searched for `git.exe` in order the way `where.exe` does,
//! but never in the current folder and never through a relative PATH entry (a `git.exe` planted
//! in a scanned folder must not run); then Git for Windows' usual folders and scoop's. The first
//! that is a file wins; its `--version` is read (5 s). Git 2.36 or newer is needed
//! (`--show-stash`, `worktree list -z`).

use std::path::{Path, PathBuf};

pub const MIN: (u32, u32) = (2, 36);

/// The git in use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Git {
    pub exe: PathBuf,
    /// "2.53.0" (git's own words after "git version", cleaned).
    pub version: String,
    pub parsed: (u32, u32, u32),
}

/// What looking for git found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    Ok(Git),
    /// Older than 2.36.
    TooOld(Git),
    /// Nothing anywhere.
    Missing,
    /// It is there but `--version` failed or said something else: why.
    Broken { exe: PathBuf, why: String },
}

impl Found {
    pub fn git(&self) -> Option<&Git> {
        match self {
            Found::Ok(g) => Some(g),
            _ => None,
        }
    }
}

/// Where to look, in order: each absolute PATH entry (quotes stripped, empty and relative ones
/// skipped), then the fallbacks. `env` answers a variable's value.
pub fn search_order(path_var: &str, env: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in path_var.split(';') {
        let e = entry.trim().trim_matches('"').trim();
        if !absolute(e) {
            continue;
        }
        out.push(Path::new(e).join("git.exe"));
    }
    let tail = [("ProgramFiles", "Git\\cmd\\git.exe"), ("ProgramFiles(x86)", "Git\\cmd\\git.exe"), ("LOCALAPPDATA", "Programs\\Git\\cmd\\git.exe"), ("USERPROFILE", "scoop\\apps\\git\\current\\cmd\\git.exe")];
    for (var, rest) in tail {
        if let Some(v) = env(var).filter(|v| absolute(v)) {
            out.push(Path::new(&v).join(rest));
        }
    }
    out
}

/// `C:\x` or `\\server\share\x`: an entry that does not depend on the current folder (`.`, `bin`,
/// `C:bin` and `\bin` all do).
pub fn absolute(e: &str) -> bool {
    let b = e.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || (b.len() > 2 && (e.starts_with("\\\\") || e.starts_with("//")) && !e.starts_with("\\\\?\\") && !e.starts_with("\\\\.\\"))
}

/// "git version 2.53.0.windows.1" as (2, 53, 0) and "2.53.0.windows.1".
pub fn parse_version(out: &[u8]) -> Option<((u32, u32, u32), String)> {
    let s = String::from_utf8_lossy(out);
    let line = s.lines().next()?.trim();
    let v = line.strip_prefix("git version ")?.trim();
    let mut n = v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse::<u32>().ok());
    let major = n.next()??;
    let minor = n.next()??;
    let patch = n.next().flatten().unwrap_or(0);
    Some(((major, minor, patch), crate::text::clean_max(v, 64)))
}

/// The name a chosen file must have.
pub fn named_git_exe(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("git.exe"))
}

/// A file (not a folder) is at `p`.
pub fn is_file(p: &Path) -> bool {
    matches!(crate::walk::attributes(p), Ok(Some(a)) if a & crate::walk::ATTR_DIRECTORY == 0)
}

/// Ask a git.exe its version (5 s).
pub fn probe(exe: &Path) -> Found {
    let env = crate::run::git_env(std::env::vars_os(), None, false);
    let cwd = crate::run::neutral_cwd();
    let args = ["--version".into()];
    let o = crate::run::run(&crate::run::Spec { exe, args: &args, env: &env, cwd: &cwd, timeout: crate::run::VERSION_TIMEOUT, out_cap: 4096, err_cap: 4096 });
    let why = match &o.ended {
        crate::run::Ended::Exited(0) => match parse_version(&o.stdout) {
            Some((parsed, version)) => {
                let g = Git { exe: exe.to_path_buf(), version, parsed };
                return if (parsed.0, parsed.1) >= MIN { Found::Ok(g) } else { Found::TooOld(g) };
            }
            None => format!("it answered \"{}\" to --version", crate::text::clean_bytes(&o.stdout, 80)),
        },
        crate::run::Ended::Exited(c) => format!("--version ended with exit code {c}"),
        crate::run::Ended::TimedOut => "--version did not answer in 5 s".into(),
        crate::run::Ended::TooBig => "--version said too much".into(),
        crate::run::Ended::NotStarted(e) => e.clone(),
    };
    Found::Broken { exe: exe.to_path_buf(), why }
}

/// The PATH of the environment Windows builds for this user right now (None when it cannot).
pub fn fresh_path() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY};
    use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: our own process; the token handle is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE, &mut token) } == 0 {
        return None;
    }
    let mut block: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: the token is open; FALSE: not this process's environment, only the user's.
    let ok = unsafe { CreateEnvironmentBlock(&mut block, token, 0) } != 0;
    // SAFETY: opened above, closed once.
    unsafe { CloseHandle(token) };
    if !ok || block.is_null() {
        return None;
    }
    let mut out = None;
    // SAFETY: the block is a list of NUL-ended UTF-16 strings ended by an empty one, ours until
    // DestroyEnvironmentBlock; reading stops at that empty string.
    unsafe {
        let mut p = block as *const u16;
        loop {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            if len == 0 {
                break;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            if let Some((k, v)) = s.split_once('=')
                && k.eq_ignore_ascii_case("Path")
            {
                out = Some(v.to_string());
            }
            p = p.add(len + 1);
        }
        DestroyEnvironmentBlock(block);
    }
    out
}

/// Look for git: a chosen git.exe first (when it is still a file named git.exe), then PATH and
/// the usual folders. Every candidate is a full path; the first that is a file is asked its
/// version.
pub fn find(chosen: Option<&Path>) -> (Found, Vec<PathBuf>) {
    let env = |k: &str| std::env::var(k).ok();
    let path = fresh_path().or_else(|| env("Path")).unwrap_or_default();
    let mut order = search_order(&path, &env);
    if let Some(c) = chosen.filter(|c| named_git_exe(c) && c.is_absolute()) {
        order.insert(0, c.to_path_buf());
    }
    for p in &order {
        if is_file(p) {
            return (probe(p), order);
        }
    }
    (Found::Missing, order)
}
