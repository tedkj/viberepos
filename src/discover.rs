//! What a folder holding `.git` is (SPEC section 2), pure: a `.git` file's `gitdir:` line, where
//! it points (a linked worktree's `commondir`, a submodule's `.git\modules\...`), a bare repo,
//! and the tags a row carries. The walker (`walk.rs`) reads the bytes; this decides.

use std::path::{Component, Path, PathBuf};

/// How much of a `.git` file is read: its first line is all git reads.
pub const GIT_FILE_MAX: u64 = 4096;

/// A `.git` file's first line, `gitdir: <path>`, as a path: relative to the file's folder,
/// forward slashes taken as git writes them on Windows, `.` and `..` resolved by name. Err: the
/// file is not one git would follow ("broken .git file"), in words.
pub fn parse_git_file(bytes: &[u8], folder: &Path) -> Result<PathBuf, String> {
    let b = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let line = b.split(|&c| c == b'\n').next().unwrap_or(b"");
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(rest) = line.strip_prefix(b"gitdir: ") else {
        return Err("its first line is not \"gitdir: <path>\"".into());
    };
    let Ok(p) = std::str::from_utf8(rest) else { return Err("its path is not text".into()) };
    let p = p.trim_end();
    if p.is_empty() {
        return Err("its gitdir line names no folder".into());
    }
    if p.contains(['\0', '<', '>', '|', '?', '*', '"']) {
        return Err("its path holds characters no Windows path can".into());
    }
    let p = p.replace('/', "\\");
    // `/c/Users/...` is how some MSYS tools write a drive; git for Windows writes `C:/...`.
    let p = msys_drive(&p).unwrap_or(p);
    let path = PathBuf::from(&p);
    let full = if path.is_absolute() || p.starts_with("\\\\") { path } else { folder.join(path) };
    Ok(lexical(&full))
}

/// `\c\x` (an MSYS path) as `C:\x`.
fn msys_drive(p: &str) -> Option<String> {
    let b = p.as_bytes();
    (b.len() >= 3 && b[0] == b'\\' && b[1].is_ascii_alphabetic() && b[2] == b'\\').then(|| format!("{}:{}", (b[1] as char).to_ascii_uppercase(), &p[2..]))
}

/// `.` and `..` resolved by name (no disk read), the way git resolves a gitdir line.
pub fn lexical(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !matches!(out.last(), Some(Component::RootDir | Component::Prefix(_))) {
                    out.push(c);
                }
            }
            c => out.push(c),
        }
    }
    out.iter().collect()
}

/// A linked worktree's `commondir` file (inside its gitdir): the shared git folder, relative to
/// the gitdir. None when the bytes name nothing.
pub fn parse_commondir(bytes: &[u8], gitdir: &Path) -> Option<PathBuf> {
    let line = bytes.split(|&c| c == b'\n').next()?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let s = std::str::from_utf8(line).ok()?.trim();
    if s.is_empty() || s.contains('\0') {
        return None;
    }
    let s = s.replace('/', "\\");
    let p = PathBuf::from(&s);
    Some(lexical(&if p.is_absolute() || s.starts_with("\\\\") { p } else { gitdir.join(p) }))
}

/// The repo a git folder belongs to: the working folder around a `.git` folder, or the git
/// folder itself (a bare repo, a separate git folder).
pub fn owner_of(common: &Path) -> PathBuf {
    match common.file_name() {
        Some(n) if n.eq_ignore_ascii_case(".git") => common.parent().map(Path::to_path_buf).unwrap_or_else(|| common.to_path_buf()),
        _ => common.to_path_buf(),
    }
}

/// A gitdir under `<x>\.git\modules\...`: the folder `<x>` whose submodule this is.
pub fn submodule_parent(gitdir: &Path) -> Option<PathBuf> {
    let parts: Vec<Component> = gitdir.components().collect();
    let i = parts.windows(2).position(|w| matches!((w[0], w[1]), (Component::Normal(a), Component::Normal(b)) if a.eq_ignore_ascii_case(".git") && b.eq_ignore_ascii_case("modules")))?;
    if i + 2 >= parts.len() {
        return None; // `.git\modules` itself is no submodule's folder
    }
    Some(parts[..i].iter().collect())
}

/// What a `.git` entry made of a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A working tree with its own `.git` folder.
    Repo,
    /// A linked worktree (`git worktree add`) of the repo at `main` (its working folder, or the
    /// bare repo's folder).
    Worktree { main: PathBuf },
    /// A submodule of the repo at `parent`.
    Submodule { parent: PathBuf },
    /// A `.git` file pointing at a git folder elsewhere (`git init --separate-git-dir`).
    Separate,
    /// A bare repo (no working tree).
    Bare,
    /// A `.git` folder without `HEAD`.
    BrokenDir,
    /// A `.git` file git would not follow, or one pointing nowhere: why.
    BrokenFile(String),
    /// A `.git` file whose gitdir is on a server that did not answer in time, even when asked
    /// again at the end of the walk: not checked yet (why), and never called broken.
    Unchecked(String),
}

impl Kind {
    /// Rows git can be asked about.
    pub fn readable(&self) -> bool {
        !matches!(self, Kind::BrokenDir | Kind::BrokenFile(_) | Kind::Unchecked(_))
    }
    pub fn bare(&self) -> bool {
        matches!(self, Kind::Bare)
    }
    /// The kind as the CSV and the copies name it.
    pub fn word(&self) -> &'static str {
        match self {
            Kind::Repo => "repo",
            Kind::Worktree { .. } => "worktree",
            Kind::Submodule { .. } => "submodule",
            Kind::Separate => "separate git folder",
            Kind::Bare => "bare",
            Kind::BrokenDir | Kind::BrokenFile(_) => "broken",
            Kind::Unchecked(_) => "not checked yet",
        }
    }
}

/// The tag (and the start of the row's line) of a `.git` file on a server that was too slow.
pub const UNCHECKED: &str = "not checked yet (server slow)";

/// A folder name that marks a bare repo by its name (`project.git`).
pub fn bare_name(name: &str) -> bool {
    name.len() > 4 && name.get(name.len() - 4..).is_some_and(|e| e.eq_ignore_ascii_case(".git"))
}

/// The tag a row carries after the repo's name: "worktree of X", "submodule of X", "bare",
/// "separate git folder", "nested in X" (another repo's working tree holds it), or nothing.
pub fn tag(kind: &Kind, nested_in: Option<&str>) -> String {
    let name = |p: &Path| crate::text::clean(&p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned()));
    match kind {
        Kind::Worktree { main } => format!("worktree of {}", name(main)),
        Kind::Submodule { parent } => format!("submodule of {}", name(parent)),
        Kind::Bare => "bare".into(),
        Kind::Separate => "separate git folder".into(),
        Kind::BrokenDir => "broken .git (no HEAD)".into(),
        Kind::BrokenFile(_) => "broken .git file".into(),
        Kind::Unchecked(_) => UNCHECKED.into(),
        Kind::Repo => nested_in.map(|n| format!("nested in {}", crate::text::clean(n))).unwrap_or_default(),
    }
}
