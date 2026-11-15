//! The other reads' outputs, parsed (pure): `for-each-ref` (every local branch with its
//! upstream and how far apart they are), `log -1` (the last commit), `remote -v` (the fetch
//! URLs) and `worktree list --porcelain -z`. Malformed output is an error, never a guess.

/// A local branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Its upstream was deleted (`[gone]`).
    pub gone: bool,
}

/// The format the for-each-ref call asks for: name, upstream and track, NUL-separated (refs
/// cannot hold NUL or a line break, so one line is one branch). for-each-ref has no `-z`
/// (DECISIONS.md).
pub const BRANCH_FORMAT: &str = "--format=%(refname:short)%00%(upstream:short)%00%(upstream:track,nobracket)";

/// `ahead 2, behind 1`, `ahead 2`, `behind 1`, `gone` or empty.
pub fn parse_track(t: &str) -> Result<(u32, u32, bool), String> {
    let t = t.trim();
    if t.is_empty() {
        return Ok((0, 0, false));
    }
    if t == "gone" {
        return Ok((0, 0, true));
    }
    let (mut a, mut b) = (None, None);
    for part in t.split(", ") {
        let (word, n) = part.split_once(' ').ok_or_else(|| format!("branch tracking not understood: {}", crate::text::clean(t)))?;
        let n: u32 = n.parse().map_err(|_| format!("branch tracking not understood: {}", crate::text::clean(t)))?;
        match word {
            "ahead" if a.is_none() => a = Some(n),
            "behind" if b.is_none() => b = Some(n),
            _ => return Err(format!("branch tracking not understood: {}", crate::text::clean(t))),
        }
    }
    Ok((a.unwrap_or(0), b.unwrap_or(0), false))
}

pub fn parse_branches(out: &[u8]) -> Result<Vec<Branch>, String> {
    let s = String::from_utf8_lossy(out);
    let mut v = Vec::new();
    for line in s.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\0').collect();
        if f.len() != 3 || f[0].is_empty() {
            return Err("branch list not understood".into());
        }
        let (ahead, behind, gone) = parse_track(f[2])?;
        v.push(Branch { name: f[0].to_string(), upstream: (!f[1].is_empty()).then(|| f[1].to_string()), ahead, behind, gone });
    }
    Ok(v)
}

/// The last commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    /// Committer time, Unix seconds.
    pub time: i64,
    pub subject: String,
}

/// `log -1 -z --format=%H%x00%ct%x00%s`. Ok(None): no commits.
pub fn parse_log(out: &[u8]) -> Result<Option<Commit>, String> {
    let body = out.strip_suffix(b"\n").unwrap_or(out);
    let body = body.strip_suffix(b"\0").unwrap_or(body);
    if body.is_empty() {
        return Ok(None);
    }
    let f: Vec<&[u8]> = body.splitn(3, |&b| b == 0).collect();
    if f.len() != 3 {
        return Err("last commit not understood".into());
    }
    let hash = std::str::from_utf8(f[0]).ok().filter(|h| h.len() >= 7 && h.bytes().all(|c| c.is_ascii_hexdigit())).ok_or("last commit not understood")?;
    let time = std::str::from_utf8(f[1]).ok().and_then(|t| t.parse::<i64>().ok()).ok_or("last commit's time not understood")?;
    Ok(Some(Commit { hash: hash.to_string(), time, subject: String::from_utf8_lossy(f[2]).into_owned() }))
}

/// A remote and the URL it fetches from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

/// A line's URL before its marker (` (fetch)` or ` (push)`), with one ` [<filter>]` allowed
/// after the marker: git 2.37+ writes a partial clone's fetch line as
/// `origin\t<url> (fetch) [blob:none]` (FED5). None when the line has anything else after it.
fn url_before(rest: &str, marker: &str) -> Option<String> {
    if let Some(url) = rest.strip_suffix(marker) {
        return Some(url.to_string());
    }
    let (head, filter) = rest.rsplit_once(" [")?;
    let filter = filter.strip_suffix(']')?;
    if filter.is_empty() || filter.contains(['[', ']']) {
        return None;
    }
    head.strip_suffix(marker).map(str::to_string)
}

/// `remote -v`: `<name>\t<url> (fetch)` lines (the push lines are skipped).
pub fn parse_remotes(out: &[u8]) -> Result<Vec<Remote>, String> {
    let s = String::from_utf8_lossy(out);
    let mut v: Vec<Remote> = Vec::new();
    for line in s.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let (name, rest) = line.split_once('\t').ok_or("remote list not understood")?;
        if let Some(url) = url_before(rest, " (fetch)") {
            if name.is_empty() {
                return Err("remote list not understood".into());
            }
            if !v.iter().any(|r| r.name == name) {
                v.push(Remote { name: name.to_string(), url });
            }
        } else if url_before(rest, " (push)").is_none() {
            return Err("remote list not understood".into());
        }
    }
    Ok(v)
}

/// One worktree of a repo.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Worktree {
    /// As git names it (shown only; actions use the walk's paths).
    pub path: String,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: Option<String>,
    /// Its folder is gone (`prunable`): why, as git says.
    pub prunable: Option<String>,
}

/// `worktree list --porcelain -z`: attribute lines NUL-ended, a worktree ended by an empty one.
pub fn parse_worktrees(out: &[u8]) -> Result<Vec<Worktree>, String> {
    let mut v: Vec<Worktree> = Vec::new();
    let mut cur: Option<Worktree> = None;
    for rec in out.split(|&b| b == 0) {
        let line = String::from_utf8_lossy(rec);
        if line.is_empty() {
            if let Some(w) = cur.take() {
                v.push(w);
            }
            continue;
        }
        let (key, val) = match line.split_once(' ') {
            Some((k, v)) => (k, Some(v)),
            None => (line.as_ref(), None),
        };
        if key == "worktree" {
            if let Some(w) = cur.take() {
                v.push(w);
            }
            cur = Some(Worktree { path: val.ok_or("worktree list not understood")?.to_string(), ..Worktree::default() });
            continue;
        }
        let w = cur.as_mut().ok_or("worktree list not understood")?;
        match key {
            "HEAD" => w.head = val.map(str::to_string),
            "branch" => w.branch = val.map(|b| b.strip_prefix("refs/heads/").unwrap_or(b).to_string()),
            "detached" => w.detached = true,
            "bare" => w.bare = true,
            "locked" => w.locked = Some(val.unwrap_or("").to_string()),
            "prunable" => w.prunable = Some(val.unwrap_or("").to_string()),
            _ => {} // an attribute a newer git adds
        }
    }
    if let Some(w) = cur.take() {
        v.push(w);
    }
    Ok(v)
}
