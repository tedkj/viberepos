//! A scripted world for the window tests (no git, no disk): git found or not, the repos a walk
//! finds and what reading each one says, what a fetch of each one does. Every call is logged so
//! a test can see what the window asked for.

use crate::backend::Backend;
use crate::discover::Kind;
use crate::fetch::Outcome;
use crate::gitexe::{self, Git};
use crate::ops::{Fetched, Op};
use crate::read::{ConfigCheck, Dubious, Failure, Info, Submodule};
use crate::refs::{Branch, Commit, Remote, Worktree};
use crate::status::{Head, Status};
use crate::walk::{Found, Progress, Walked};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

/// The tests' "now": 2026-10-23 12:00:00 UTC.
pub const NOW: i64 = 1_792_756_800;
pub const DAY: i64 = 86_400;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct World {
    pub git: Mutex<gitexe::Found>,
    pub first: Mutex<Vec<PathBuf>>,
    pub repos: Mutex<Vec<(Found, Info)>>,
    /// Extras a walk reports (links not followed, unreadable folders).
    pub walk_extra: Mutex<Walked>,
    /// What fetching a repo (by path, lower case) does; Fetched when not named.
    pub fetches: Mutex<HashMap<String, Outcome>>,
    /// What reading a repo says after it was fetched (by path, lower case).
    pub after_fetch: Mutex<HashMap<String, Info>>,
    pub fetched: Mutex<Vec<String>>,
    pub login: Mutex<Option<String>>,
    /// Every call, in order ("walk", "read <name>", "fetch <name>", "git", "first", "login").
    pub log: Mutex<Vec<String>>,
}

pub fn git_ok() -> gitexe::Found {
    gitexe::Found::Ok(Git { exe: PathBuf::from("C:\\Program Files\\Git\\cmd\\git.exe"), version: "2.53.0.windows.1".into(), parsed: (2, 53, 0) })
}

fn key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

impl World {
    pub fn new(repos: Vec<(Found, Info)>) -> World {
        World {
            git: Mutex::new(git_ok()),
            first: Mutex::new(vec![PathBuf::from("C:\\Projects")]),
            repos: Mutex::new(repos),
            walk_extra: Mutex::new(Walked::default()),
            fetches: Mutex::new(HashMap::new()),
            after_fetch: Mutex::new(HashMap::new()),
            fetched: Mutex::new(Vec::new()),
            login: Mutex::new(Some("sample-user".into())),
            log: Mutex::new(Vec::new()),
        }
    }

    /// A developer's folder: every state the spec names, once.
    pub fn office() -> World {
        World::new(office())
    }

    pub fn calls(&self) -> Vec<String> {
        lock(&self.log).clone()
    }

    pub fn set_fetch(&self, path: &str, o: Outcome) {
        lock(&self.fetches).insert(key(Path::new(path)), o);
    }

    pub fn set_after_fetch(&self, path: &str, i: Info) {
        lock(&self.after_fetch).insert(key(Path::new(path)), i);
    }

    pub fn set_info(&self, path: &str, f: impl FnOnce(&mut Info)) {
        let mut r = lock(&self.repos);
        let e = r.iter_mut().find(|(fo, _)| key(&fo.path) == key(Path::new(path))).unwrap_or_else(|| panic!("no repo {path}"));
        f(&mut e.1);
    }

    pub fn remove(&self, path: &str) {
        lock(&self.repos).retain(|(fo, _)| key(&fo.path) != key(Path::new(path)));
    }
}

impl Backend for World {
    fn find_git(&self, chosen: Option<&Path>) -> gitexe::Found {
        lock(&self.log).push(format!("git {}", chosen.map(|c| c.to_string_lossy().into_owned()).unwrap_or_default()).trim().to_string());
        match (chosen, &*lock(&self.git)) {
            (Some(c), gitexe::Found::Missing) if gitexe::named_git_exe(c) => gitexe::Found::Ok(Git { exe: c.to_path_buf(), version: "2.53.0.windows.1".into(), parsed: (2, 53, 0) }),
            (_, g) => g.clone(),
        }
    }

    fn first_roots(&self) -> Vec<PathBuf> {
        lock(&self.log).push("first".into());
        lock(&self.first).clone()
    }

    fn walk(&self, roots: &[PathBuf], _cancel: &AtomicBool, progress: &dyn Fn(&Progress)) -> Walked {
        lock(&self.log).push("walk".into());
        let mut w = lock(&self.walk_extra).clone();
        let repos = lock(&self.repos).clone();
        for (ri, root) in roots.iter().enumerate() {
            let rk = key(root);
            let mut any = false;
            for (f, _) in &repos {
                let fk = key(&f.path);
                if (fk == rk || fk.starts_with(&format!("{}\\", rk.trim_end_matches('\\')))) && !w.found.iter().any(|x| key(&x.path) == fk) {
                    let mut f = f.clone();
                    f.root = ri;
                    f.rel = f.path.strip_prefix(root).map(Path::to_path_buf).unwrap_or_default();
                    w.found.push(f);
                    any = true;
                }
            }
            if !any && !repos.iter().any(|(f, _)| key(&f.path).starts_with(&rk)) && rk.contains("gone") {
                w.root_errors.push((ri, format!("{}: The system cannot find the path specified", root.display())));
            }
        }
        w.folders += 1234;
        progress(&Progress { root: roots.first().map(|r| r.display().to_string()).unwrap_or_default(), folders: w.folders, repos: w.found.len() });
        w
    }

    fn read(&self, _git: &Git, found: &Found) -> Info {
        lock(&self.log).push(format!("read {}", found.name()));
        let k = key(&found.path);
        if lock(&self.fetched).contains(&k)
            && let Some(i) = lock(&self.after_fetch).get(&k)
        {
            return i.clone();
        }
        lock(&self.repos).iter().find(|(f, _)| key(&f.path) == k).map(|(_, i)| i.clone()).unwrap_or_else(|| {
            let mut i = Info::failed(Failure::new("gone since the scan"));
            i.gone = true;
            i
        })
    }

    fn fetch(&self, _git: &Git, found: &Found) -> Outcome {
        lock(&self.log).push(format!("fetch {}", found.name()));
        let k = key(&found.path);
        lock(&self.fetched).push(k.clone());
        lock(&self.fetches).get(&k).cloned().unwrap_or(Outcome::Fetched)
    }

    fn login(&self, _git: &Git) -> Option<String> {
        lock(&self.log).push("login".into());
        lock(&self.login).clone()
    }
}

// ---------- building the world ----------

pub fn found(path: &str) -> Found {
    let p = PathBuf::from(path);
    Found { root: 0, path: p.clone(), rel: PathBuf::new(), kind: Kind::Repo, gitdir: Some(p.join(".git")), common: Some(p.join(".git")), nested_in: None, id: None }
}

pub fn status(branch: &str) -> Status {
    Status { head: Head::Branch(branch.into()), oid: Some("4f2a9c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39".into()), ..Status::default() }
}

pub fn origin(url: &str) -> Vec<Remote> {
    vec![Remote { name: "origin".into(), url: url.into() }]
}

/// A clean repo on `branch`, up to date with origin, last commit `days` ago, fetched a day ago.
pub fn info(branch: &str, remote: Option<&str>, days: i64) -> Info {
    let mut s = status(branch);
    let remotes = remote.map(origin).unwrap_or_default();
    let mut branches = vec![Branch { name: branch.into(), ..Branch::default() }];
    if remote.is_some() {
        s.upstream = Some(format!("origin/{branch}"));
        s.ab = Some((0, 0));
        branches[0].upstream = s.upstream.clone();
    }
    Info {
        config: ConfigCheck::default(),
        status: Ok(s),
        branches: Ok(branches),
        last: Ok(Some(Commit { hash: "4f2a9c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39".into(), time: NOW - days * DAY, subject: "Tidy the docs".into() })),
        remotes: Ok(remotes),
        worktrees: Ok(vec![]),
        op: Ok(None),
        fetched: if remote.is_some() { Fetched::At(NOW - DAY) } else { Fetched::Never },
        dubious: None,
        submodules: vec![],
        gone: false,
        lfs: false,
        filtered: false,
    }
}

fn with(mut i: Info, f: impl FnOnce(&mut Info)) -> Info {
    f(&mut i);
    i
}

fn st(i: &mut Info) -> &mut Status {
    i.status.as_mut().expect("a status")
}

/// The office: each state the spec names.
pub fn office() -> Vec<(Found, Info)> {
    let gh = |n: &str| format!("https://github.com/sample-user/{n}.git");
    let mut v = Vec::new();
    v.push((found("C:\\Projects\\Greenhouse"), with(info("main", Some(&gh("Greenhouse")), 0), |i| {
        let s = st(i);
        (s.staged, s.unstaged, s.untracked) = (2, 5, 3);
        s.ab = Some((3, 1));
        if let Ok(b) = &mut i.branches {
            b[0].ahead = 3;
            b[0].behind = 1;
        }
    })));
    v.push((found("C:\\Projects\\Cookbook-pub"), info("main", Some(&gh("Cookbook-pub")), 12)));
    v.push((found("C:\\Projects\\VibeRepos"), with(info("main", None, 0), |i| st(i).untracked = 4)));
    v.push((found("C:\\Projects\\VibeFTP"), with(info("master", None, 6), |i| {
        st(i).stash = 2;
        if let Ok(b) = &mut i.branches {
            b.push(Branch { name: "slint-port".into(), ..Branch::default() });
        }
    })));
    v.push((found("C:\\Projects\\Banjo"), with(info("feature/presets", Some(&gh("Banjo")), 2), |i| {
        let s = st(i);
        s.conflicts = 2;
        s.unstaged = 1;
        s.ab = Some((1, 4));
        i.op = Ok(Some(Op::Rebasing));
    })));
    v.push((found("C:\\Projects\\Tetris"), with(info("main", Some("git@github.com:sample-user/tetris-old.git"), 40), |i| {
        st(i).ab = None; // upstream gone
        if let Ok(b) = &mut i.branches {
            b[0].gone = true;
        }
        i.op = Ok(Some(Op::Merging));
        st(i).conflicts = 1;
    })));
    v.push((found("C:\\Projects\\Juggler"), with(info("main", Some("https://gitlab.com/sample-user/juggler.git"), 1), |i| {
        let s = st(i);
        s.head = Head::Detached;
        s.upstream = None;
        s.ab = None;
        i.op = Ok(Some(Op::Bisecting));
    })));
    v.push((found("C:\\Projects\\NewThing"), with(info("main", None, 0), |i| {
        st(i).oid = None;
        st(i).untracked = 1;
        i.last = Ok(None);
        i.branches = Ok(vec![]);
    })));
    v.push((found("C:\\Projects\\Puzzles"), with(info("main", Some("ssh://git@nas.home.arpa:2222/srv/git/puzzles.git"), 3), |i| {
        st(i).ab = Some((0, 2));
        i.op = Ok(Some(Op::CherryPicking));
    })));
    v.push((found("C:\\Projects\\Sketches"), with(info("main", Some("https://ghp_SECRETTOKEN@github.com/sample-user/sketches.git"), 9), |i| {
        i.config.filters = vec!["lfs".into()];
        i.fetched = Fetched::At(NOW - 30 * DAY);
    })));
    v.push((found("C:\\Projects\\Dotfiles"), with(info("main", Some("git@github.com:sample-user/dotfiles.git"), 100), |i| {
        i.config.fetch_commands = vec!["core.sshCommand".into()];
    })));
    let mut wt = found("C:\\Projects\\VibeFTP-port");
    wt.kind = Kind::Worktree { main: PathBuf::from("C:\\Projects\\VibeFTP") };
    wt.gitdir = Some(PathBuf::from("C:\\Projects\\VibeFTP\\.git\\worktrees\\VibeFTP-port"));
    wt.common = Some(PathBuf::from("C:\\Projects\\VibeFTP\\.git"));
    v.push((wt, info("slint-port", None, 2)));
    let mut sub = found("C:\\Projects\\Greenhouse\\vendor\\lib");
    sub.kind = Kind::Submodule { parent: PathBuf::from("C:\\Projects\\Greenhouse") };
    sub.gitdir = Some(PathBuf::from("C:\\Projects\\Greenhouse\\.git\\modules\\vendor\\lib"));
    sub.common = sub.gitdir.clone();
    v.push((sub, with(info("main", Some("https://github.com/other/lib.git"), 200), |i| st(i).head = Head::Detached)));
    let mut bare = found("C:\\Projects\\backup.git");
    bare.kind = Kind::Bare;
    bare.gitdir = Some(PathBuf::from("C:\\Projects\\backup.git"));
    bare.common = bare.gitdir.clone();
    v.push((bare, with(info("main", None, 5), |i| i.status = Err(Failure::new(crate::read::BARE)))));
    let mut broken = found("C:\\Projects\\Old\\half");
    broken.kind = Kind::BrokenFile("it points to C:\\Projects\\Old\\.git\\worktrees\\half, which is not there".into());
    v.push((broken, Info::failed(Failure::new("broken"))));
    v.push((found("C:\\Projects\\Shared"), with(Info::failed(Failure { short: "Git refuses to read this repo: the folder is owned by BUILTIN\\Administrators (safe.directory)".into(), full: "fatal: detected dubious ownership in repository at 'C:/Projects/Shared'".into() }), |i| {
        i.dubious = Some(Dubious { owner: "BUILTIN\\Administrators".into(), command: "git config --global --add safe.directory C:/Projects/Shared".into() });
    })));
    v.push((found("C:\\Projects\\Cookbook-pub\\site"), with(info("gh-pages", Some(&gh("Cookbook-pub")), 30), |i| {
        i.submodules = vec![Submodule { path: "themes/x".into(), initialised: false }];
        i.worktrees = Ok(vec![Worktree { path: "C:/Projects/Cookbook-pub/site".into(), branch: Some("gh-pages".into()), ..Worktree::default() }, Worktree { path: "D:/old/site-wt".into(), prunable: Some("gitdir file points to non-existent location".into()), ..Worktree::default() }]);
    })));
    let last = v.len() - 1;
    v[last].0.nested_in = Some(PathBuf::from("C:\\Projects\\Cookbook-pub"));
    v
}

/// `n` repos with long names and branches (the 300-repo snapshot and the timing test).
pub fn many(n: usize) -> Vec<(Found, Info)> {
    (0..n)
        .map(|k| {
            let name = format!("C:\\Projects\\clients\\{:03}-a-rather-long-project-folder-name-for-a-customer-{}", k, ["æøå", "日本語", "x"][k % 3]);
            let b = if k % 7 == 0 { "feature/a-very-long-branch-name-that-keeps-going-and-going-\u{202E}evil" } else { "main" };
            let remote = (k % 4 != 0).then(|| format!("https://github.com/sample-user/project-{k}{}.git", if k % 5 == 0 { "-pub" } else { "" }));
            let mut i = info(b, remote.as_deref(), (k as i64 * 3) % 400);
            if k % 3 == 0 {
                st(&mut i).unstaged = (k % 11) as u32 + 1;
            }
            if k % 6 == 1 {
                st(&mut i).ab = Some((k as u32 % 5 + 1, 0));
            }
            (found(&name), i)
        })
        .collect()
}
