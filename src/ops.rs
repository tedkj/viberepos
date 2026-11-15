//! What git leaves in a repo's git folder while an operation is half done, and when the repo was
//! last fetched, read straight from the files (no git call; SPEC section 3, read 6). A file
//! that cannot be looked at is unknown, never "not there".

use std::path::Path;

/// An operation left half done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Rebasing,
    Am,
    Merging,
    CherryPicking,
    Reverting,
    Bisecting,
}

impl Op {
    /// The chip before the branch.
    pub fn chip(self) -> &'static str {
        match self {
            Op::Rebasing => "REBASING",
            Op::Am => "AM",
            Op::Merging => "MERGING",
            Op::CherryPicking => "CHERRY-PICKING",
            Op::Reverting => "REVERTING",
            Op::Bisecting => "BISECTING",
        }
    }
    /// In a sentence and the CSV.
    pub fn word(self) -> &'static str {
        match self {
            Op::Rebasing => "rebasing",
            Op::Am => "applying patches (am)",
            Op::Merging => "merging",
            Op::CherryPicking => "cherry-picking",
            Op::Reverting => "reverting",
            Op::Bisecting => "bisecting",
        }
    }
}

/// Which of git's marker files are there (the per-worktree git folder), as read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Markers {
    pub merge_head: bool,
    pub rebase_merge: bool,
    pub rebase_apply: bool,
    pub applying: bool,
    pub cherry_pick_head: bool,
    pub revert_head: bool,
    pub bisect_log: bool,
}

/// The operation the markers say, the one git's own prompt would name first: a rebase or am
/// over a merge or pick (they run inside one), bisect last (it can sit under any of them).
pub fn op_of(m: Markers) -> Option<Op> {
    if m.rebase_merge || (m.rebase_apply && !m.applying) {
        Some(Op::Rebasing)
    } else if m.rebase_apply && m.applying {
        Some(Op::Am)
    } else if m.merge_head {
        Some(Op::Merging)
    } else if m.cherry_pick_head {
        Some(Op::CherryPicking)
    } else if m.revert_head {
        Some(Op::Reverting)
    } else if m.bisect_log {
        Some(Op::Bisecting)
    } else {
        None
    }
}

/// Whether `name` is in `dir`: Err with the words when it cannot be told.
fn present(dir: &Path, name: &str) -> Result<bool, String> {
    match crate::walk::attributes(&dir.join(name)) {
        Ok(a) => Ok(a.is_some()),
        Err(e) => Err(format!("{}: {}", name, crate::walk::error_text(e))),
    }
}

/// The operation in progress in `gitdir` (a worktree's own git folder), or why it is unknown.
pub fn in_progress(gitdir: &Path) -> Result<Option<Op>, String> {
    let m = Markers {
        merge_head: present(gitdir, "MERGE_HEAD")?,
        rebase_merge: present(gitdir, "rebase-merge")?,
        rebase_apply: present(gitdir, "rebase-apply")?,
        applying: present(&gitdir.join("rebase-apply"), "applying").unwrap_or(false),
        cherry_pick_head: present(gitdir, "CHERRY_PICK_HEAD")?,
        revert_head: present(gitdir, "REVERT_HEAD")?,
        bisect_log: present(gitdir, "BISECT_LOG")?,
    };
    Ok(op_of(m))
}

/// When the repo was last fetched: FETCH_HEAD's modified time in the common git folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetched {
    Never,
    /// Unix seconds.
    At(i64),
    Unknown(String),
}

pub fn fetched(common: &Path) -> Fetched {
    let p = common.join("FETCH_HEAD");
    match std::fs::metadata(crate::host::path_from_wide(&crate::walk::long(&crate::walk::units(&p)))) {
        Ok(m) => match m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
            Some(d) => Fetched::At(d.as_secs() as i64),
            None => Fetched::Unknown("FETCH_HEAD's time could not be read".into()),
        },
        Err(e) if matches!(e.raw_os_error(), Some(2 | 3)) => Fetched::Never,
        Err(e) => Fetched::Unknown(format!("FETCH_HEAD: {}", crate::walk::error_words(&e))),
    }
}
