//! Reading one repo (SPEC section 3), strictly read-only: its own config checked first (filters
//! switched off, or nothing read when the config cannot be checked), then git's reads, each on
//! its own (one failing leaves the others standing), then the in-progress markers and
//! FETCH_HEAD straight from the files. Nothing is cached across scans, nothing is written to a
//! repo: `GIT_OPTIONAL_LOCKS=0` keeps status from refreshing the index, `core.fsmonitor=false`
//! and a hooks folder that does not exist keep the repo's programs out.

use crate::discover::Kind;
use crate::gitconfig;
use crate::gitexe::Git;
use crate::ops::{Fetched, Op};
use crate::refs::{Branch, Commit, Remote, Worktree};
use crate::run::{self, Ended};
use crate::status::Status;
use crate::walk::Found;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// A read that failed: its short words (the row) and all of it (the detail panel, at most 2 KiB).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub short: String,
    pub full: String,
}

impl Failure {
    pub fn new(short: impl Into<String>) -> Failure {
        let s = short.into();
        Failure { full: s.clone(), short: s }
    }
}

pub type Res<T> = Result<T, Failure>;

/// A bare repo's status: not read (there is no working tree), not a failure.
pub const BARE: &str = "bare (no working tree)";

/// Git refused the repo: who owns its folder, and the command that would let it read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dubious {
    pub owner: String,
    /// Empty when no command can be offered (`safe_directory_command`).
    pub command: String,
}

/// A submodule `.gitmodules` names, and whether its folder holds a checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submodule {
    pub path: String,
    pub initialised: bool,
}

/// What the repo's own config says (before any git call).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigCheck {
    /// Filter drivers switched off for the reads.
    pub filters: Vec<String>,
    /// Settings that run a program on fetch: the repo is never fetched.
    pub fetch_commands: Vec<String>,
    /// The config could not be checked: why (then git is not run at all).
    pub problem: Option<String>,
}

impl ConfigCheck {
    /// The row's line about it, if any.
    pub fn warning(&self) -> Option<String> {
        if let Some(p) = &self.problem {
            return Some(format!("Repo config could not be checked ({}): not read", crate::text::clean(p)));
        }
        (!self.filters.is_empty()).then(|| {
            let names: Vec<String> = self.filters.iter().map(|f| crate::text::clean(f)).collect();
            format!("Repo config runs programs (filter {}): read with them switched off; counts can differ from your own git status", names.join(", "))
        })
    }
}

/// Everything read about one repo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub config: ConfigCheck,
    pub status: Res<Status>,
    pub branches: Res<Vec<Branch>>,
    pub last: Res<Option<Commit>>,
    pub remotes: Res<Vec<Remote>>,
    pub worktrees: Res<Vec<Worktree>>,
    pub op: Res<Option<Op>>,
    pub fetched: Fetched,
    pub dubious: Option<Dubious>,
    pub submodules: Vec<Submodule>,
    /// Its folder went between the scan and this read.
    pub gone: bool,
    /// It uses Git LFS: its LFS files are left out of status (`LFS_NOTE`).
    pub lfs: bool,
    /// Some of its tracked files have a git filter other than LFS: they are left out of status
    /// (`FILTER_NOTE`).
    pub filtered: bool,
}

/// The detail panel's words for a repo that uses Git LFS.
pub const LFS_NOTE: &str = "Uses Git LFS: LFS files are not checked (checking them would let git-lfs write)";

/// The detail panel's words for a repo with files that have a git filter (LFS has its own line).
pub const FILTER_NOTE: &str = "Files with a git filter are not checked (git would run the filter's program)";

/// Status's own arguments; its pathspec (`KEEP_UNFILTERED`) follows `--`.
pub const STATUS: [&str; 7] = ["status", "--porcelain=v2", "--branch", "--show-stash", "-z", "--untracked-files=normal", "--ignore-submodules=dirty"];

/// Status keeps only the paths whose `filter` attribute names no driver (unspecified, set without
/// a value, or unset): a path with `filter=<driver>` is never hashed, so its clean filter never
/// runs, whatever the driver and wherever it is configured (AUDIT-1 R-REPOS-2). It names what to
/// keep because git's `attr:filter` magic matches only a filter set without a value, never
/// `filter=nb`. With git-lfs off (`run::LFS_OFF`) a checked-out LFS file would never equal its
/// pointer anyway.
pub const KEEP_UNFILTERED: [&str; 3] = [":(attr:!filter)", ":(attr:filter)", ":(attr:-filter)"];

/// `ls-files`' pathspec for the tracked files `KEEP_UNFILTERED` leaves out, LFS's own excepted.
pub const FILTERED: [&str; 5] = [".", ":(exclude,attr:!filter)", ":(exclude,attr:filter)", ":(exclude,attr:-filter)", ":(exclude,attr:filter=lfs)"];

/// On every read: no credential helper (a read never needs one). A fetch keeps the user's own.
pub const NO_CREDENTIALS: [&str; 2] = ["-c", "credential.helper="];

impl Info {
    /// Nothing could be read: every read is this failure.
    pub fn failed(f: Failure) -> Info {
        Info {
            config: ConfigCheck::default(),
            status: Err(f.clone()),
            branches: Err(f.clone()),
            last: Err(f.clone()),
            remotes: Err(f.clone()),
            worktrees: Err(f.clone()),
            op: Err(f),
            fetched: Fetched::Never,
            dubious: None,
            submodules: Vec::new(),
            gone: false,
            lfs: false,
            filtered: false,
        }
    }

    /// The first failure among the reads, for the row's error line (a bare repo's status, which
    /// is not read, is no failure).
    pub fn first_error(&self) -> Option<&Failure> {
        [self.status.as_ref().err(), self.branches.as_ref().err(), self.last.as_ref().err(), self.remotes.as_ref().err(), self.worktrees.as_ref().err(), self.op.as_ref().err()].into_iter().flatten().find(|f| f.short != BARE)
    }
}

/// What every git call of a scan shares.
#[derive(Clone, Debug)]
pub struct Ctx {
    pub git: Git,
    /// A folder that does not exist (`core.hooksPath`).
    pub hooks: PathBuf,
    /// git's own current folder (it works in `-C <repo>`).
    pub cwd: PathBuf,
    /// For `~/` in a repo config's include.
    pub home: Option<PathBuf>,
    /// Added to the environment, each replacing any variable of its name (tests only:
    /// GIT_TEST_ASSUME_DIFFERENT_OWNER, the fixture's config).
    pub extra_env: Vec<(OsString, OsString)>,
    pub timeout: Duration,
}

impl Ctx {
    pub fn new(git: Git, local_data: &Path) -> Ctx {
        let mut r = [0u8; 8];
        crate::host::random_bytes(&mut r);
        let hooks = local_data.join(format!("no-hooks-{}", r.iter().map(|b| format!("{b:02x}")).collect::<String>()));
        Ctx { git, hooks, cwd: run::neutral_cwd(), home: std::env::var_os("USERPROFILE").map(PathBuf::from), extra_env: Vec::new(), timeout: run::READ_TIMEOUT }
    }

    fn env(&self, repo: &Path, fetch: bool) -> Vec<(OsString, OsString)> {
        let mut e = run::git_env(std::env::vars_os(), repo.parent().or(Some(repo)), fetch);
        for (k, v) in &self.extra_env {
            // Each name once (Windows looks names up ignoring case): the last one given wins.
            e.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
            e.push((k.clone(), v.clone()));
        }
        e
    }

    /// A git call's arguments: `run::git_args`, with `NO_CREDENTIALS` first on every read (AUDIT-1
    /// R-REPOS-5). A fetch keeps the user's own credential helper: it is the user's own network
    /// action.
    pub fn args(&self, repo: &Path, overrides: &[String], command: &[&str], fetch: bool) -> Vec<OsString> {
        let mut o: Vec<String> = if fetch { Vec::new() } else { NO_CREDENTIALS.iter().map(|s| s.to_string()).collect() };
        o.extend(overrides.iter().cloned());
        run::git_args(repo, &self.hooks, &o, command)
    }

    /// One git call in `repo`, as it ended, its stdout kept up to `out_cap`.
    pub fn output(&self, repo: &Path, overrides: &[String], command: &[&str], timeout: Duration, fetch: bool, out_cap: usize) -> run::Output {
        let args = self.args(repo, overrides, command, fetch);
        let env = self.env(repo, fetch);
        run::run(&run::Spec { exe: &self.git.exe, args: &args, env: &env, cwd: &self.cwd, timeout, out_cap, err_cap: run::ERR_CAP })
    }

    /// One git call in `repo`: stdout, or the failure in words.
    pub fn call(&self, repo: &Path, overrides: &[String], command: &[&str], timeout: Duration, fetch: bool) -> Res<Vec<u8>> {
        let o = self.output(repo, overrides, command, timeout, fetch, run::OUT_CAP);
        match o.ended {
            Ended::Exited(0) => Ok(o.stdout),
            Ended::Exited(code) => Err(failure_of(&o.stderr, code)),
            Ended::TimedOut => Err(Failure { short: format!("timed out after {} s", timeout.as_secs()), full: format!("git did not finish in {} s and was stopped. {}", timeout.as_secs(), crate::text::clean_bytes(&o.stderr, 2048)).trim().to_string() }),
            Ended::TooBig => Err(Failure::new("too many changes to count (git's answer passed 16 MiB)")),
            Ended::NotStarted(e) => Err(Failure::new(e)),
        }
    }
}

/// git's stderr in words: the first line (cleaned) for the row, all of it (2 KiB) for the panel.
pub fn failure_of(stderr: &[u8], code: u32) -> Failure {
    let text = String::from_utf8_lossy(stderr);
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let short = if code >= 0xC000_0000 {
        format!("git crashed (exit code {code:#X})")
    } else if first.contains("not a git repository") {
        "not a repository after all (git does not read its .git)".into()
    } else if first.is_empty() {
        format!("git failed (exit code {code})")
    } else {
        crate::text::clean_max(first, 200)
    };
    let full = crate::text::clean_max(&text, 2048);
    Failure { full: if full.is_empty() { short.clone() } else { full }, short }
}

pub fn is_dubious(stderr_or_full: &str) -> bool {
    stderr_or_full.contains("detected dubious ownership")
}

/// The command that lets git read a repo owned by another account (VibeRepos never runs it),
/// pasted as it is into PowerShell (the terminal the app opens) or cmd: the path must stay
/// literal text in both (IN-VibeRepos-3). Letters, digits and `/ : . _ -` go bare; anything else
/// is double-quoted, inside which both shells keep the text as it is except PowerShell's `$` and
/// backtick, cmd's `%` and `!` and the quotes themselves (PowerShell also takes U+201C..U+201E
/// for `"`). A path holding one of those, an invisible character (a line separator would end the
/// command; the rest do not show in the tooltip) or a unit that is not text (U+FFFD: git would
/// get another path) gets no command: empty, and the window then offers none.
pub fn safe_directory_command(path: &Path) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    if p.contains(['$', '`', '%', '!', '"', '\u{201C}', '\u{201D}', '\u{201E}', '\u{FFFD}']) || p.chars().any(crate::text::invisible) {
        return String::new();
    }
    let bare = p.chars().all(|c| c.is_alphanumeric() || matches!(c, '/' | ':' | '.' | '_' | '-'));
    let q = if bare { p } else { format!("\"{p}\"") };
    format!("git config --global --add safe.directory {q}")
}

/// The config files of a repo, in the order git reads them: the common folder's `config`, then
/// the worktree's own `config.worktree`.
pub fn config_files(found: &Found) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(c) = &found.common {
        v.push(c.join("config"));
    }
    if let Some(g) = &found.gitdir {
        v.push(g.join("config.worktree"));
    }
    v
}

pub fn check_config(found: &Found, home: Option<&Path>) -> ConfigCheck {
    let l = gitconfig::load(&config_files(found), home);
    ConfigCheck { filters: l.filters(), fetch_commands: l.fetch_commands(), problem: l.problem }
}

/// `.gitmodules` in a working tree: each submodule's path and whether it is checked out.
pub fn submodules(work: &Path) -> Vec<Submodule> {
    let Ok(Some(bytes)) = gitconfig::read_capped(&work.join(".gitmodules"), gitconfig::MAX_FILE) else { return Vec::new() };
    let Ok(entries) = gitconfig::parse(&bytes) else { return Vec::new() };
    let mut out: Vec<Submodule> = Vec::new();
    for e in entries {
        if e.section == "submodule"
            && e.key == "path"
            && let Some(p) = e.value
        {
            let rel = p.replace('/', "\\");
            // Only a path inside the working tree is followed: no `..`, no root (`\x`) and no
            // drive (`Q:x` would replace the working tree by that drive's current folder; review
            // RF4-1).
            let parts = Path::new(&rel).components();
            let inside = parts.clone().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) && parts.into_iter().any(|c| matches!(c, Component::Normal(_)));
            if !inside || out.iter().any(|s| s.path == p) {
                continue;
            }
            // `.gitmodules` is cloned content: its path is asked the way a gitdir, include or
            // common folder is before it is looked at, so a link in the working tree that may
            // lead to another machine gets the bounded probe (RF4-1). Not answered: not shown
            // as initialised.
            let dot = work.join(&rel).join(".git");
            let initialised = crate::walk::unc_answers(&dot).is_ok() && matches!(crate::walk::attributes(&dot), Ok(Some(_)));
            out.push(Submodule { path: p, initialised });
            if out.len() >= 200 {
                break;
            }
        }
    }
    out
}

/// A working tree that uses Git LFS: its top `.gitattributes` or the git folder's
/// `info\attributes` names `filter=lfs`, or git-lfs has made its folder in the common git folder
/// (it does on any clone or checkout with LFS files).
pub fn uses_lfs(found: &Found) -> bool {
    let names = |p: &Path| matches!(gitconfig::read_capped(p, gitconfig::MAX_FILE), Ok(Some(b)) if b.windows(10).any(|w| w == b"filter=lfs"));
    names(&found.path.join(".gitattributes"))
        || found.common.as_deref().is_some_and(|c| names(&c.join("info").join("attributes")) || matches!(crate::walk::attributes(&c.join("lfs")), Ok(Some(a)) if a & crate::walk::ATTR_DIRECTORY != 0))
}

/// The filter drivers in `git config --list --name-only -z`'s answer: every
/// `filter.<driver>.clean`, `.smudge` or `.process` (section and key in any case, the driver as
/// written), each once. None when such a name is not text (it could not be switched off).
pub fn drivers(listed: &[u8]) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for name in listed.split(|&b| b == 0) {
        if !name.get(..7).is_some_and(|s| s.eq_ignore_ascii_case(b"filter.")) {
            continue;
        }
        let name = std::str::from_utf8(name).ok()?;
        let Some((driver, key)) = name[7..].rsplit_once('.') else { continue };
        if !driver.is_empty() && ["clean", "smudge", "process"].iter().any(|k| key.eq_ignore_ascii_case(k)) && !out.iter().any(|d| d == driver) {
            out.push(driver.to_string());
        }
    }
    Some(out)
}

/// The `-c` switches that turn off every filter driver git would see in this repo (AUDIT-1
/// R-REPOS-2): the ones its own config names (read by VibeRepos first, every include followed)
/// and the ones any config level names as git reads them for it (`git config --list` in the
/// repo: system, global, the repo's own, includes with their real conditions). Err: they could
/// not be listed, or one cannot be switched off; the repo is then not read.
pub fn filter_switches(ctx: &Ctx, repo: &Path, config: &ConfigCheck) -> Res<Vec<String>> {
    let listed = ctx.call(repo, &[], &["config", "--list", "--name-only", "-z"], ctx.timeout, false).map_err(|f| Failure {
        short: "git's config could not be listed: not read".into(),
        full: format!("VibeRepos lists the filters every git config names before it reads a repo, to switch them off. Listing them failed: {}", f.full),
    })?;
    let mut names = config.filters.clone();
    for d in drivers(&listed).ok_or_else(|| Failure::new("a git config names a filter whose name is not text: not read"))? {
        if !names.contains(&d) {
            names.push(d);
        }
    }
    gitconfig::filter_overrides(&names).ok_or_else(|| Failure::new("a git config names a filter that cannot be switched off (its name holds =): not read"))
}

/// Read one repo, as found by the walk.
pub fn read(ctx: &Ctx, found: &Found) -> Info {
    if !found.kind.readable() {
        let why = match &found.kind {
            Kind::BrokenFile(w) => format!("broken .git file: {}", crate::text::clean(w)),
            Kind::Unchecked(w) => format!("{}: {}", crate::discover::UNCHECKED, crate::text::clean(w)),
            _ => "broken .git: it has no HEAD".into(),
        };
        return Info::failed(Failure::new(why));
    }
    // Gone since the scan: not found, its folder still answering. Anything else is git's to say.
    if let Ok(None) = crate::walk::attributes(&found.path)
        && found.path.parent().is_some_and(|p| matches!(crate::walk::attributes(p), Ok(Some(_))))
    {
        let mut i = Info::failed(Failure::new("gone since the scan"));
        i.gone = true;
        return i;
    }
    let config = check_config(found, ctx.home.as_deref());
    let bare = found.kind.bare();
    let mut info = Info::failed(Failure::new("not read"));
    info.config = config.clone();
    info.op = if bare {
        Ok(None)
    } else {
        found.gitdir.as_deref().map(crate::ops::in_progress).unwrap_or(Ok(None)).map_err(|e| Failure::new(format!("unknown: {}", crate::text::clean(&e))))
    };
    info.fetched = found.common.as_deref().map(crate::ops::fetched).unwrap_or(Fetched::Never);
    if !bare {
        info.submodules = submodules(&found.path);
        info.lfs = uses_lfs(found);
    }
    if let Some(p) = &config.problem {
        let f = Failure { short: "repo config could not be checked: not read".into(), full: format!("VibeRepos reads a repo's own config before it runs git, to switch off programs it names. This one could not be read: {}. Git was not run.", crate::text::clean(p)) };
        let op = info.op.clone();
        info = Info { config, op, fetched: info.fetched, submodules: info.submodules, ..Info::failed(f) };
        return info;
    }
    let overrides = match filter_switches(ctx, &found.path, &config) {
        Ok(o) => o,
        Err(f) => {
            let op = info.op.clone();
            return Info { config, op, fetched: info.fetched, submodules: info.submodules, ..Info::failed(f) };
        }
    };
    let t = ctx.timeout;
    let path = &found.path;
    let mut unborn = false;
    if !bare {
        // Submodules' own work trees are their own rows: `dirty` keeps git from running status
        // inside them (their config is not this repo's), while a new commit there still shows.
        let command: Vec<&str> = STATUS.iter().copied().chain(["--"]).chain(KEEP_UNFILTERED).collect();
        let st = ctx.call(path, &overrides, &command, t, false);
        if let Err(f) = &st
            && is_dubious(&f.full)
        {
            let owner = crate::host::owner_of(path).unwrap_or_else(|| "another account".into());
            let d = Dubious { owner: owner.clone(), command: safe_directory_command(path) };
            let f = Failure { short: format!("Git refuses to read this repo: the folder is owned by {} (safe.directory)", crate::text::clean(&owner)), full: f.full.clone() };
            let op = info.op.clone();
            return Info { config, op, fetched: info.fetched, submodules: info.submodules, dubious: Some(d), ..Info::failed(f) };
        }
        info.status = st.and_then(|o| crate::status::parse(&o).map_err(Failure::new));
        unborn = info.status.as_ref().is_ok_and(|s| s.unborn());
        // Whether status left any tracked file out for its filter (one is enough to say so).
        let command: Vec<&str> = ["ls-files", "-z", "--"].into_iter().chain(FILTERED).collect();
        let o = ctx.output(path, &overrides, &command, t, false, 4096);
        info.filtered = o.ended == Ended::TooBig || (o.ok() && !o.stdout.is_empty());
    } else {
        info.status = Err(Failure::new(BARE));
    }
    info.branches = ctx.call(path, &overrides, &["for-each-ref", crate::refs::BRANCH_FORMAT, "refs/heads"], t, false).and_then(|o| crate::refs::parse_branches(&o).map_err(Failure::new));
    if let Err(f) = &info.branches
        && is_dubious(&f.full)
    {
        let owner = crate::host::owner_of(path).unwrap_or_else(|| "another account".into());
        let d = Dubious { owner: owner.clone(), command: safe_directory_command(path) };
        let f = Failure { short: format!("Git refuses to read this repo: the folder is owned by {} (safe.directory)", crate::text::clean(&owner)), full: f.full.clone() };
        return Info { config, dubious: Some(d), fetched: info.fetched, ..Info::failed(f) };
    }
    info.last = if unborn {
        Ok(None)
    } else {
        match ctx.call(path, &overrides, &["log", "-1", "-z", "--no-show-signature", "--format=%H%x00%ct%x00%s"], t, false) {
            Ok(o) => crate::refs::parse_log(&o).map_err(Failure::new),
            // A bare repo or a branch with no commits says so on stderr.
            Err(f) if f.full.contains("does not have any commits yet") => Ok(None),
            Err(f) => Err(f),
        }
    };
    info.remotes = ctx.call(path, &overrides, &["remote", "-v"], t, false).and_then(|o| crate::refs::parse_remotes(&o).map_err(Failure::new));
    info.worktrees = if bare { Ok(Vec::new()) } else { ctx.call(path, &overrides, &["worktree", "list", "--porcelain", "-z"], t, false).and_then(|o| crate::refs::parse_worktrees(&o).map_err(Failure::new)) };
    info
}
