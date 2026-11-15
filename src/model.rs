//! The rows (SPEC sections 4 and 6), pure: what each column says, the filters and their counts,
//! the sort (ranks made once, a total order), the status line and the detail panel's lines.
//! Every word the window, the copies and the CSV show is made here; text from outside is cleaned
//! on the way.

use crate::discover::Kind;
use crate::ops::Fetched;
use crate::read::{Failure, Info};
use crate::remote;
use crate::status::Head;
use crate::text::{clean, count};
use crate::walk::Found;
use std::cmp::Ordering;

/// A fetch's state for one row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FetchState {
    #[default]
    Idle,
    Queued,
    Fetching,
    Fetched,
    Failed(Failure),
    Skipped(String),
}

impl FetchState {
    pub fn word(&self) -> Option<String> {
        match self {
            FetchState::Idle => None,
            FetchState::Queued => Some("queued".into()),
            FetchState::Fetching => Some("fetching…".into()),
            FetchState::Fetched => Some("fetched".into()),
            FetchState::Failed(f) => Some(format!("fetch failed: {}", f.short)),
            FetchState::Skipped(w) => Some(format!("not fetched: {w}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub found: Found,
    /// None while it is being read.
    pub info: Option<Info>,
    pub fetch: FetchState,
}

/// What the formatting needs besides the row.
#[derive(Clone, Debug, Default)]
pub struct View {
    /// Now, Unix seconds.
    pub now: i64,
    /// The user's GitHub login (unused since the public hint went, copy-ux M9).
    pub login: Option<String>,
    /// git was not found: every row says so.
    pub git_missing: bool,
    /// What a row says then ("git not found", "git too old", "git not usable"; empty: the first).
    pub git_word: &'static str,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Dirty,
    Unpushed,
    Behind,
    NoRemote,
}

impl Filter {
    pub const ALL: [Filter; 5] = [Filter::All, Filter::Dirty, Filter::Unpushed, Filter::Behind, Filter::NoRemote];
    pub fn word(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Dirty => "Dirty",
            Filter::Unpushed => "Unpushed",
            Filter::Behind => "Behind",
            Filter::NoRemote => "No remote",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Dirty => "dirty",
            Filter::Unpushed => "unpushed",
            Filter::Behind => "behind",
            Filter::NoRemote => "no-remote",
        }
    }
    pub fn from_key(k: &str) -> Option<Filter> {
        Filter::ALL.into_iter().find(|f| f.key() == k)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Path,
    LastCommit,
    Fetched,
}

impl Sort {
    pub fn key(self) -> &'static str {
        match self {
            Sort::Path => "path",
            Sort::LastCommit => "last-commit",
            Sort::Fetched => "fetched",
        }
    }
    pub fn from_key(k: &str) -> Option<Sort> {
        [Sort::Path, Sort::LastCommit, Sort::Fetched].into_iter().find(|s| s.key() == k)
    }
}

// ---------- facts of a row ----------

impl Row {
    pub fn new(found: Found) -> Row {
        Row { found, info: None, fetch: FetchState::Idle }
    }

    pub fn name(&self) -> String {
        self.found.name()
    }

    fn status(&self) -> Option<&crate::status::Status> {
        self.info.as_ref()?.status.as_ref().ok()
    }

    fn remotes(&self) -> Option<&[crate::refs::Remote]> {
        self.info.as_ref()?.remotes.as_deref().ok()
    }

    /// The row's error, as its line says it: a broken `.git`, git refusing, a failed read.
    pub fn error(&self, v: &View) -> Option<String> {
        if v.git_missing && self.found.kind.readable() {
            return Some(if v.git_word.is_empty() { "git not found" } else { v.git_word }.into());
        }
        if let Kind::BrokenFile(w) = &self.found.kind {
            return Some(format!("broken .git file: {}", clean(w)));
        }
        if self.found.kind == Kind::BrokenDir {
            return Some("broken .git (no HEAD)".into());
        }
        if let Kind::Unchecked(w) = &self.found.kind {
            return Some(format!("{}: {}", crate::discover::UNCHECKED, clean(w)));
        }
        let info = self.info.as_ref()?;
        if info.gone {
            return Some("gone since the scan".into());
        }
        info.first_error().map(|f| f.short.clone())
    }

    pub fn dirty(&self) -> bool {
        self.status().is_some_and(|s| s.dirty())
    }

    pub fn ahead(&self) -> u32 {
        self.status().and_then(|s| s.ab).map_or(0, |a| a.0)
    }

    pub fn behind(&self) -> u32 {
        self.status().and_then(|s| s.ab).map_or(0, |a| a.1)
    }

    /// The current branch's name, when on one.
    fn current(&self) -> Option<&str> {
        match &self.status()?.head {
            Head::Branch(b) => Some(b),
            _ => None,
        }
    }

    pub fn has_remote(&self) -> bool {
        self.remotes().is_some_and(|r| !r.is_empty())
    }

    pub fn no_remote(&self) -> bool {
        self.remotes().is_some_and(|r| r.is_empty())
    }

    /// Other local branches with work that is not on a remote: ahead of their upstream, or with
    /// no upstream while a remote exists.
    pub fn other_unpushed(&self) -> usize {
        let Some(info) = &self.info else { return 0 };
        let Ok(branches) = &info.branches else { return 0 };
        let has_remote = self.has_remote();
        let current = self.current();
        branches.iter().filter(|b| Some(b.name.as_str()) != current && (b.ahead > 0 || (b.upstream.is_none() && has_remote))).count()
    }

    /// The current branch has commits and no upstream while a remote exists.
    fn current_unpublished(&self) -> bool {
        let Some(s) = self.status() else { return false };
        matches!(s.head, Head::Branch(_)) && !s.unborn() && s.upstream.is_none() && self.has_remote()
    }

    pub fn unpushed(&self) -> bool {
        self.ahead() > 0 || self.other_unpushed() > 0 || self.current_unpublished()
    }

    pub fn matches(&self, f: Filter) -> bool {
        match f {
            Filter::All => true,
            Filter::Dirty => self.dirty(),
            Filter::Unpushed => self.unpushed(),
            Filter::Behind => self.behind() > 0,
            Filter::NoRemote => self.no_remote(),
        }
    }

    /// The remote the Remote column names: the upstream's, else `origin`, else the first.
    pub fn primary_remote(&self) -> Option<&crate::refs::Remote> {
        let rs = self.remotes()?;
        let up = self.status().and_then(|s| s.upstream.as_deref());
        if let Some(up) = up
            && let Some(r) = rs.iter().filter(|r| up.starts_with(&format!("{}/", r.name))).max_by_key(|r| r.name.len())
        {
            return Some(r);
        }
        rs.iter().find(|r| r.name == "origin").or_else(|| rs.first())
    }

    pub fn last_commit_time(&self) -> Option<i64> {
        self.info.as_ref()?.last.as_ref().ok()?.as_ref().map(|c| c.time)
    }

    pub fn fetched_time(&self) -> Option<i64> {
        match self.info.as_ref()?.fetched {
            Fetched::At(t) => Some(t),
            _ => None,
        }
    }

    /// Something that makes the row a problem: an error, a conflict, an operation half done.
    pub fn trouble(&self, v: &View) -> bool {
        self.error(v).is_some()
    }
}

// ---------- what the columns say ----------

/// "3 days ago", "just now"; a time in the future (a clock off) says so.
pub fn age(t: i64, now: i64) -> String {
    let d = now - t;
    if d < -120 {
        return "in the future".into();
    }
    let d = d.max(0);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86_400 => format!("{} h ago", d / 3600),
        86_400..172_800 => "1 day ago".into(),
        172_800..1_209_600 => format!("{} days ago", d / 86_400),
        1_209_600..4_838_400 => format!("{} weeks ago", d / 604_800),
        4_838_400..63_072_000 => format!("{} months ago", d / 2_629_800),
        _ => format!("{} years ago", d / 31_557_600),
    }
}

/// The cells a row shows (and the copies and the CSV use).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cells {
    pub repo: String,
    pub tag: String,
    /// "REBASING" etc., or empty.
    pub op: String,
    pub branch: String,
    pub changes: String,
    pub conflicts: bool,
    pub sync: String,
    pub stash: String,
    pub remote: String,
    pub hint: String,
    pub last: String,
    pub last_tip: String,
    pub fetched: String,
    pub error: String,
    /// Still being read.
    pub reading: bool,
}

pub fn changes_text(s: &crate::status::Status) -> String {
    let mut parts = Vec::new();
    if s.conflicts > 0 {
        parts.push(count(s.conflicts as usize, "conflict", "conflicts"));
    }
    if s.staged > 0 {
        parts.push(format!("{} staged", s.staged));
    }
    if s.unstaged > 0 {
        parts.push(format!("{} changed", s.unstaged));
    }
    if s.untracked > 0 {
        parts.push(format!("{} untracked", s.untracked));
    }
    if parts.is_empty() { "clean".into() } else { parts.join(" · ") }
}

pub fn cells(r: &Row, v: &View) -> Cells {
    let mut c = Cells { repo: r.name(), tag: crate::discover::tag(&r.found.kind, r.found.nested_in.as_deref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy()).as_deref()), ..Cells::default() };
    c.error = r.error(v).unwrap_or_default();
    let Some(info) = &r.info else {
        c.reading = r.found.kind.readable() && !v.git_missing;
        if c.reading {
            c.changes = "reading…".into();
        }
        return c;
    };
    if let Ok(Some(op)) = &info.op {
        c.op = op.chip().into();
    }
    match &info.status {
        Ok(s) => {
            c.branch = match &s.head {
                Head::Detached => format!("detached at {}", s.oid.as_deref().map(|o| &o[..o.len().min(7)]).unwrap_or("?")),
                Head::Branch(_) if s.unborn() => "no commits yet".into(),
                Head::Branch(b) => clean(b),
                Head::Unknown => "branch unknown".into(),
            };
            c.changes = changes_text(s);
            c.conflicts = s.conflicts > 0;
            c.sync = match (&s.upstream, s.ab) {
                (Some(_), Some((0, 0))) => "up to date".into(),
                (Some(_), Some((a, b))) => [(a > 0).then(|| format!("↑{a}")), (b > 0).then(|| format!("↓{b}"))].into_iter().flatten().collect::<Vec<_>>().join(" "),
                (Some(_), None) => "upstream gone".into(),
                (None, _) if matches!(s.head, Head::Detached) => String::new(),
                (None, _) => "no upstream".into(),
            };
            c.stash = if s.stash > 0 { s.stash.to_string() } else { String::new() };
        }
        Err(_) if r.found.kind.bare() => {
            c.branch = "bare".into();
            c.changes = String::new();
        }
        Err(_) => {
            c.branch = "branch unknown".into();
            c.changes = "unknown".into();
        }
    }
    let other = r.other_unpushed();
    if other > 0 {
        let more = count(other, "branch", "branches");
        c.sync = if c.sync.is_empty() { format!("+{more}") } else { format!("{} · +{more}", c.sync) };
    }
    c.remote = match &info.remotes {
        Ok(rs) if rs.is_empty() => "no remote".into(),
        Ok(_) => r.primary_remote().map(|p| remote::parse(&p.url).column()).unwrap_or_default(),
        Err(_) => "unknown".into(),
    };
    match &info.last {
        Ok(Some(cm)) => {
            c.last = age(cm.time, v.now);
            c.last_tip = format!("{} · {}", crate::host::local(cm.time).0.minutes(), clean(&cm.subject));
        }
        Ok(None) => c.last = "none".into(),
        Err(_) => c.last = "unknown".into(),
    }
    c.fetched = match &info.fetched {
        Fetched::Never => "never".into(),
        Fetched::At(t) => age(*t, v.now),
        Fetched::Unknown(_) => "unknown".into(),
    };
    c
}

/// What a screen reader says about a row (its one accessibility node).
pub fn a11y(c: &Cells) -> String {
    let parts = [&c.tag, &c.op, &c.branch, &c.changes, &c.sync, &c.remote, &c.hint]
        .into_iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .chain((!c.stash.is_empty()).then(|| format!("{} stashed", c.stash)))
        .chain((!c.last.is_empty()).then(|| format!("last commit {}", c.last)))
        .chain((!c.fetched.is_empty()).then(|| format!("fetched {}", c.fetched)))
        .chain((!c.error.is_empty()).then(|| c.error.clone()))
        .collect::<Vec<_>>();
    parts.join(" · ")
}

/// The Sync column's tooltip: ahead and behind are as of the last fetch.
pub fn sync_tip(r: &Row, v: &View) -> String {
    let Some(info) = &r.info else { return String::new() };
    let Ok(s) = &info.status else { return String::new() };
    let when = match &info.fetched {
        Fetched::Never => "it was never fetched".to_string(),
        Fetched::At(t) => format!("the fetch {}", age(*t, v.now)),
        Fetched::Unknown(_) => "a fetch of unknown age".into(),
    };
    match (&s.upstream, s.ab) {
        (Some(u), Some((a, b))) => format!("Against {} as of {when}: ahead {a}, behind {b}", clean(u)),
        (Some(u), None) => format!("{} was deleted on the remote (as of {when})", clean(u)),
        _ => String::new(),
    }
}

// ---------- filters, counts, sort ----------

/// The rows a chip and the name field show (indexes into `rows`, in `order`). Errors pass every
/// chip (never hidden by one); the name field filters them like any row.
pub fn visible(rows: &[Row], order: &[usize], f: Filter, name: &str, v: &View) -> Vec<usize> {
    let needle = name.trim().to_lowercase();
    order
        .iter()
        .copied()
        .filter(|&i| {
            let r = &rows[i];
            (r.matches(f) || r.trouble(v)) && (needle.is_empty() || r.name().to_lowercase().contains(&needle) || r.found.rel.to_string_lossy().to_lowercase().contains(&needle))
        })
        .collect()
}

/// Each chip's count, over every row (the name field aside).
pub fn chip_counts(rows: &[Row]) -> [usize; 5] {
    let mut c = [0usize; 5];
    for r in rows {
        for (i, f) in Filter::ALL.iter().enumerate() {
            if r.matches(*f) {
                c[i] += 1;
            }
        }
    }
    c
}

/// Two UTF-16 names in `CompareStringOrdinal`'s order, ignoring case; equal ones by their exact
/// units (a total order: no locale, no "logical" digits).
pub fn ordinal_cmp(a: &[u16], b: &[u16]) -> Ordering {
    use windows_sys::Win32::Globalization::CompareStringOrdinal;
    // SAFETY: two slices with their lengths.
    let r = unsafe { CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1) };
    match r {
        1 => Ordering::Less,
        3 => Ordering::Greater,
        _ => a.cmp(b),
    }
}

/// The display order: ranks made once (the key of each row), then a sort by (key, index): a total
/// order whatever the names hold.
pub fn order(rows: &[Row], s: Sort) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    match s {
        Sort::Path => {
            let keys: Vec<(usize, Vec<u16>)> = rows.iter().map(|r| (r.found.root, crate::walk::units(&r.found.rel))).collect();
            // Rank the paths once (ordinal, ignoring case), then sort integers.
            let mut by: Vec<usize> = (0..rows.len()).collect();
            by.sort_by(|&a, &b| keys[a].0.cmp(&keys[b].0).then_with(|| ordinal_cmp(&keys[a].1, &keys[b].1)).then(a.cmp(&b)));
            let mut rank = vec![0usize; rows.len()];
            for (n, &i) in by.iter().enumerate() {
                rank[i] = n;
            }
            idx.sort_by_key(|&i| rank[i]);
        }
        Sort::LastCommit => {
            let k: Vec<i64> = rows.iter().map(|r| r.last_commit_time().map_or(i64::MAX, |t| -t)).collect();
            idx.sort_by_key(|&i| (k[i], i));
        }
        Sort::Fetched => {
            let k: Vec<i64> = rows.iter().map(|r| r.fetched_time().map_or(i64::MAX, |t| -t)).collect();
            idx.sort_by_key(|&i| (k[i], i));
        }
    }
    idx
}

/// "41 repos · 6 dirty · 3 unpushed · 2 behind · 28 no remote".
pub fn summary(rows: &[Row]) -> String {
    let c = chip_counts(rows);
    format!("{} · {} dirty · {} unpushed · {} behind · {} no remote", count(rows.len(), "repo", "repos"), c[1], c[2], c[3], c[4])
}

// ---------- the detail panel ----------

/// One line of the detail panel: a section title, or a key and its value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub key: String,
    pub value: String,
    pub mono: bool,
    pub section: bool,
    pub warn: bool,
}

fn line(key: &str, value: impl Into<String>, mono: bool) -> Line {
    Line { key: key.into(), value: value.into(), mono, ..Line::default() }
}

fn section(t: &str) -> Line {
    Line { key: t.into(), section: true, ..Line::default() }
}

pub fn detail(r: &Row, v: &View) -> Vec<Line> {
    let mut out = Vec::new();
    let sp = crate::text::shown_path;
    out.push(line("Path", sp(&r.found.path), true));
    if let Some(g) = &r.found.gitdir {
        out.push(line("Git folder", sp(g), true));
    }
    if let Some(c) = &r.found.common
        && Some(c) != r.found.gitdir.as_ref()
    {
        out.push(line("Common", sp(c), true));
    }
    if let Some(e) = r.error(v) {
        out.push(Line { key: "Error".into(), value: e, warn: true, ..Line::default() });
    }
    if let Some(w) = r.fetch.word() {
        let warn = matches!(r.fetch, FetchState::Failed(_) | FetchState::Skipped(_));
        out.push(Line { key: "Fetch".into(), value: w, warn, ..Line::default() });
    }
    let Some(info) = &r.info else { return out };
    if let Some(w) = info.config.warning() {
        out.push(Line { key: "Config".into(), value: w, warn: true, ..Line::default() });
    }
    if info.lfs {
        out.push(Line { key: "LFS".into(), value: crate::read::LFS_NOTE.into(), warn: true, ..Line::default() });
    }
    if info.filtered {
        out.push(Line { key: "Filters".into(), value: crate::read::FILTER_NOTE.into(), warn: true, ..Line::default() });
    }
    if !info.config.fetch_commands.is_empty() {
        out.push(Line { key: "Fetch".into(), value: format!("never fetched by VibeRepos: its config sets {}", info.config.fetch_commands.join(", ")), warn: true, ..Line::default() });
    }
    if let Some(d) = &info.dubious {
        out.push(Line { key: "Owner".into(), value: clean(&d.owner), warn: true, ..Line::default() });
        out.push(line("Command", d.command.clone(), true));
    }
    if let Ok(Some(op)) = &info.op {
        out.push(Line { key: "In progress".into(), value: op.word().into(), warn: true, ..Line::default() });
    }
    if let Ok(s) = &info.status {
        out.push(section("BRANCH"));
        let c = cells(r, v);
        out.push(line("Branch", c.branch.clone(), true));
        if let Some(o) = &s.oid {
            out.push(line("Commit", o.clone(), true));
        }
        out.push(line("Upstream", s.upstream.as_deref().map(clean).unwrap_or_else(|| "none".into()), true));
        let tip = sync_tip(r, v);
        if !tip.is_empty() {
            out.push(line("Sync", tip, false));
        }
        out.push(line("Changes", c.changes, false));
        if s.stash > 0 {
            out.push(line("Stashes", s.stash.to_string(), true));
        }
    }
    if let Ok(bs) = &info.branches
        && !bs.is_empty()
    {
        out.push(section("LOCAL BRANCHES"));
        for b in bs.iter().take(50) {
            let state = match (&b.upstream, b.gone) {
                (Some(_), true) => "upstream gone".to_string(),
                (Some(u), false) if b.ahead + b.behind == 0 => format!("up to date with {}", clean(u)),
                (Some(u), false) => format!("↑{} ↓{} against {}", b.ahead, b.behind, clean(u)),
                (None, _) => "no upstream".into(),
            };
            out.push(line(&clean(&b.name), state, true));
        }
        if bs.len() > 50 {
            out.push(line("", format!("and {} more", bs.len() - 50), false));
        }
    }
    // Remotes as read (a failed read is the error above, not said twice).
    if let Ok(rs) = &info.remotes {
        out.push(section("REMOTES"));
        if rs.is_empty() {
            out.push(line("", "no remote", false));
        }
        for rm in rs {
            out.push(line(&clean(&rm.name), remote::parse(&rm.url).shown, true));
        }
    }
    out.push(line(
        "Last fetch",
        match &info.fetched {
            Fetched::Never => "never".into(),
            Fetched::At(t) => format!("{} ({})", crate::host::local(*t).0.minutes(), age(*t, v.now)),
            Fetched::Unknown(e) => format!("unknown: {}", clean(e)),
        },
        false,
    ));
    if let Ok(Some(cm)) = &info.last {
        out.push(section("LAST COMMIT"));
        out.push(line("Hash", cm.hash.clone(), true));
        out.push(line("Date", format!("{} ({})", crate::host::local(cm.time).0.minutes(), age(cm.time, v.now)), false));
        out.push(line("Subject", clean(&cm.subject), false));
    }
    if let Ok(wts) = &info.worktrees
        && wts.len() > 1
    {
        out.push(section("WORKTREES"));
        for w in wts.iter().take(50) {
            let what = if w.bare {
                "bare".to_string()
            } else if let Some(p) = &w.prunable {
                format!("folder gone (prunable{})", if p.is_empty() { String::new() } else { format!(": {}", clean(p)) })
            } else if w.detached {
                "detached".into()
            } else {
                w.branch.as_deref().map(clean).unwrap_or_default()
            };
            out.push(Line { key: what, value: clean(&w.path), mono: true, warn: w.prunable.is_some(), ..Line::default() });
        }
    }
    if !info.submodules.is_empty() {
        out.push(section("SUBMODULES"));
        for s in info.submodules.iter().take(50) {
            out.push(Line { key: clean(&s.path), value: if s.initialised { "checked out".into() } else { "not initialised".into() }, warn: !s.initialised, ..Line::default() });
        }
    }
    if let Some(f) = info.first_error()
        && f.full != f.short
    {
        out.push(section("GIT SAID"));
        out.push(Line { key: String::new(), value: f.full.clone(), mono: true, warn: true, ..Line::default() });
    }
    if let FetchState::Failed(f) = &r.fetch
        && f.full != f.short
    {
        out.push(section("FETCH SAID"));
        out.push(Line { key: String::new(), value: f.full.clone(), mono: true, warn: true, ..Line::default() });
    }
    out
}
