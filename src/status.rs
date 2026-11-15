//! `git status --porcelain=v2 --branch --show-stash -z --untracked-files=normal`, parsed (pure).
//!
//! Headers: `# branch.oid <commit>|(initial)`, `# branch.head <name>|(detached)`,
//! `# branch.upstream <name>`, `# branch.ab +<ahead> -<behind>` (an upstream without it is gone),
//! `# stash <n>`. Records: `1 XY ...` changed, `2 XY ... <path>\0<orig>` renamed or copied,
//! `u XY ...` unmerged, `? <path>` untracked, `! <path>` ignored. Staged when X is not `.`,
//! unstaged when Y is not `.`; an untracked folder counts once. Anything git would not write is
//! refused ("status output not understood"), never guessed.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    Detached,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub head: Head,
    /// The commit checked out; None: no commits yet (`(initial)`).
    pub oid: Option<String>,
    pub upstream: Option<String>,
    /// Ahead and behind the upstream as of the last fetch; None with an upstream: it is gone.
    pub ab: Option<(u32, u32)>,
    pub stash: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicts: u32,
}

impl Status {
    pub fn upstream_gone(&self) -> bool {
        self.upstream.is_some() && self.ab.is_none()
    }
    pub fn unborn(&self) -> bool {
        self.oid.is_none()
    }
    pub fn dirty(&self) -> bool {
        self.staged + self.unstaged + self.untracked + self.conflicts > 0
    }
}

fn bad(what: &str) -> String {
    format!("status output not understood ({what})")
}

fn xy(field: &[u8]) -> Result<(u8, u8), String> {
    match field {
        [x, y] if valid_code(*x) && valid_code(*y) => Ok((*x, *y)),
        _ => Err(bad("a change code")),
    }
}

fn valid_code(c: u8) -> bool {
    matches!(c, b'.' | b'M' | b'T' | b'A' | b'D' | b'R' | b'C' | b'U' | b'?' | b'!')
}

/// The output of the status call, every record NUL-ended.
pub fn parse(out: &[u8]) -> Result<Status, String> {
    let mut s = Status::default();
    let mut oid_seen = false;
    let mut recs = out.split(|&b| b == 0);
    let mut last_empty = false;
    while let Some(r) = recs.next() {
        if r.is_empty() {
            // Only the split after the final NUL is empty.
            last_empty = true;
            continue;
        }
        if last_empty {
            return Err(bad("an empty record"));
        }
        match r[0] {
            b'#' => {
                // A ref name is bytes to git; one that is not UTF-8 is shown replaced, not refused.
                let line = String::from_utf8_lossy(r);
                let line = line.strip_prefix("# ").ok_or_else(|| bad("a header"))?;
                let (key, val) = line.split_once(' ').ok_or_else(|| bad("a header without a value"))?;
                match key {
                    "branch.oid" => {
                        oid_seen = true;
                        s.oid = if val == "(initial)" {
                            None
                        } else if val.len() >= 4 && val.bytes().all(|c| c.is_ascii_hexdigit()) {
                            Some(val.to_string())
                        } else {
                            return Err(bad("the commit id"));
                        };
                    }
                    "branch.head" => s.head = if val == "(detached)" { Head::Detached } else { Head::Branch(val.to_string()) },
                    "branch.upstream" => s.upstream = Some(val.to_string()),
                    "branch.ab" => {
                        let (a, b) = val.split_once(' ').ok_or_else(|| bad("ahead and behind"))?;
                        let a = a.strip_prefix('+').and_then(|n| n.parse::<u32>().ok()).ok_or_else(|| bad("ahead"))?;
                        let b = b.strip_prefix('-').and_then(|n| n.parse::<u32>().ok()).ok_or_else(|| bad("behind"))?;
                        s.ab = Some((a, b));
                    }
                    "stash" => s.stash = val.parse().map_err(|_| bad("the stash count"))?,
                    // A header a newer git adds is not an error.
                    _ => {}
                }
            }
            b'1' => {
                let f: Vec<&[u8]> = r.splitn(9, |&b| b == b' ').collect();
                if f.len() != 9 || f[8].is_empty() {
                    return Err(bad("a changed entry"));
                }
                count(&mut s, xy(f[1])?);
            }
            b'2' => {
                let f: Vec<&[u8]> = r.splitn(10, |&b| b == b' ').collect();
                if f.len() != 10 || f[9].is_empty() || !matches!(f[8].first(), Some(b'R' | b'C')) {
                    return Err(bad("a renamed entry"));
                }
                count(&mut s, xy(f[1])?);
                // The original path follows as a record of its own.
                match recs.next() {
                    Some(orig) if !orig.is_empty() => {}
                    _ => return Err(bad("a renamed entry without its original path")),
                }
            }
            b'u' => {
                let f: Vec<&[u8]> = r.splitn(11, |&b| b == b' ').collect();
                if f.len() != 11 || f[10].is_empty() {
                    return Err(bad("an unmerged entry"));
                }
                xy(f[1])?;
                s.conflicts += 1;
            }
            b'?' => {
                if r.len() < 3 || r[1] != b' ' {
                    return Err(bad("an untracked entry"));
                }
                s.untracked += 1;
            }
            b'!' => {
                if r.len() < 3 || r[1] != b' ' {
                    return Err(bad("an ignored entry"));
                }
            }
            _ => return Err(bad("a record of an unknown kind")),
        }
    }
    if !oid_seen || s.head == Head::Unknown {
        return Err(bad("no branch headers"));
    }
    Ok(s)
}

fn count(s: &mut Status, (x, y): (u8, u8)) {
    if x != b'.' {
        s.staged += 1;
    }
    if y != b'.' {
        s.unstaged += 1;
    }
}
