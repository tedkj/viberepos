//! What the controller asks of the outside world: find git, find the roots on a first start,
//! walk, read a repo, fetch one, and the user's GitHub login. `System` is the real thing;
//! `fake::World` scripts it for the window tests (they run without git).

use crate::fetch::Outcome;
use crate::gitexe::{self, Git};
use crate::read::{Ctx, Info};
use crate::walk::{Found, Progress, Walked};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub trait Backend: Send + Sync {
    /// git.exe: a chosen one first (still a file named git.exe), then PATH and the usual folders.
    fn find_git(&self, chosen: Option<&Path>) -> gitexe::Found;
    /// A first start's roots: the usual folders that exist and hold a repo within three levels.
    fn first_roots(&self) -> Vec<PathBuf>;
    fn walk(&self, roots: &[PathBuf], cancel: &AtomicBool, progress: &dyn Fn(&Progress)) -> Walked;
    fn read(&self, git: &Git, found: &Found) -> Info;
    fn fetch(&self, git: &Git, found: &Found) -> Outcome;
    /// The GitHub login the user's git config names (unused since the public hint went, copy-ux M9).
    fn login(&self, git: &Git) -> Option<String>;
}

/// The usual places, in order (SPEC section 2).
pub fn candidates(profile: Option<&Path>) -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("C:\\Projects")];
    if let Some(p) = profile {
        for rel in ["source\\repos", "Documents\\GitHub", "Projects", "repos", "src", "dev", "code"] {
            v.push(p.join(rel));
        }
    }
    v.extend(["C:\\src", "C:\\dev", "C:\\repos", "C:\\code"].map(PathBuf::from));
    v
}

pub struct System {
    /// %LOCALAPPDATA%\com.tedkj.viberepos (or the portable data folder): where the hooks folder
    /// that does not exist is named.
    pub local_data: PathBuf,
    /// Added to git's environment (tests only).
    pub extra_env: Vec<(OsString, OsString)>,
}

impl System {
    pub fn ctx(&self, git: &Git) -> Ctx {
        let mut c = Ctx::new(git.clone(), &self.local_data);
        c.extra_env = self.extra_env.clone();
        c
    }
}

impl Backend for System {
    fn find_git(&self, chosen: Option<&Path>) -> gitexe::Found {
        gitexe::find(chosen).0
    }

    fn first_roots(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for c in candidates(crate::host::profile().as_deref()) {
            if out.iter().any(|o| o.as_os_str().eq_ignore_ascii_case(c.as_os_str())) {
                continue;
            }
            if matches!(crate::walk::attributes(&c), Ok(Some(a)) if a & crate::walk::ATTR_DIRECTORY != 0) && crate::walk::holds_a_repo(&c, 3) {
                out.push(c);
            }
        }
        out
    }

    fn walk(&self, roots: &[PathBuf], cancel: &AtomicBool, progress: &dyn Fn(&Progress)) -> Walked {
        crate::walk::walk(roots, cancel, progress)
    }

    fn read(&self, git: &Git, found: &Found) -> Info {
        crate::read::read(&self.ctx(git), found)
    }

    fn fetch(&self, git: &Git, found: &Found) -> Outcome {
        crate::fetch::fetch(&self.ctx(git), found)
    }

    fn login(&self, git: &Git) -> Option<String> {
        let get = |key: &str| -> Option<String> {
            let env = crate::run::git_env(std::env::vars_os().chain(self.extra_env.iter().cloned()), None, false);
            let cwd = crate::run::neutral_cwd();
            let args: Vec<OsString> = ["config", "--get", key].iter().map(OsString::from).collect();
            let o = crate::run::run(&crate::run::Spec { exe: &git.exe, args: &args, env: &env, cwd: &cwd, timeout: crate::run::VERSION_TIMEOUT, out_cap: 4096, err_cap: 4096 });
            o.ok().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty())
        };
        crate::remote::login(get("github.user").as_deref(), get("user.email").as_deref())
    }
}
