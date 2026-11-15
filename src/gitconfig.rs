//! A repo's own config, read by VibeRepos itself before any git call (SPEC section 3): a small,
//! strict reader of git's INI format, for untrusted files. Git runs programs a repo's config
//! names (filters on status, ssh and credential helpers on fetch), so VibeRepos looks first and
//! switches the filters off (`-c filter.<driver>.clean=` ...) or does not fetch at all.
//!
//! The grammar is git's (config.c): `[section]`, `[section "sub"]` (the subsection exact, `\"`
//! and `\\` escaped), the old `[section.sub]` (the subsection lower-cased), `key = value` or a
//! bare `key` (true), values with quotes, the escapes `\n \t \b \\ \"`, a backslash at the end of
//! a line going on, `#` and `;` comments outside quotes. Anything else is an error with its
//! line: git would refuse such a file too, and VibeRepos then reads nothing from that repo (it
//! cannot tell what the file would run). `include.path` and `includeIf.<anything>.path` are
//! followed, every condition taken as true, at most 10 deep, each file at most 1 MiB, and at most
//! 64 files, 4 MiB and 20 000 entries in one load; an include on another machine is asked first,
//! the way a gitdir is (5 s, one question per server at a time); a missing include is skipped (as
//! git does), one that cannot be read is an error.

use std::path::{Path, PathBuf};

/// One `key = value` with where it sits. Section and key are lower case; the subsection as
/// written (case matters there); `value` None for a bare key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub section: String,
    pub sub: Option<String>,
    pub key: String,
    pub value: Option<String>,
}

pub const MAX_FILE: u64 = 1024 * 1024;
pub const MAX_DEPTH: usize = 10;
/// What one load reads at most, all files together (the configs and every include, the same
/// file included again counted again): beyond any of these the files are not read
/// (a `problem`). Ted's 40 repos read 1 to 3 files of well under 100 entries.
pub const MAX_FILES: usize = 64;
pub const MAX_TOTAL: u64 = 4 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 20_000;

/// One file's text as entries, or the first line it cannot be read at ("line 7: ...").
pub fn parse(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let b = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if b.contains(&0) {
        return Err("a NUL byte".into());
    }
    let mut p = Parser { b, i: 0, line: 1 };
    let mut out = Vec::new();
    let mut section: Option<(String, Option<String>)> = None;
    loop {
        p.skip_ws();
        let Some(c) = p.peek() else { break };
        match c {
            b'\n' => {
                p.bump();
            }
            b'#' | b';' => p.skip_line(),
            b'[' => {
                p.bump();
                section = Some(p.header()?);
                // After the header, the line may hold only a comment or a key.
            }
            c if c.is_ascii_alphabetic() => {
                let key = p.name()?;
                let Some((sec, sub)) = section.clone() else { return Err(p.err("a key outside any section")) };
                p.skip_ws();
                let value = match p.peek() {
                    Some(b'=') => {
                        p.bump();
                        Some(p.value()?)
                    }
                    None | Some(b'\n') => None,
                    Some(b'#') | Some(b';') => {
                        p.skip_line();
                        None
                    }
                    Some(_) => return Err(p.err("a key followed by something other than =")),
                };
                if out.len() >= MAX_ENTRIES {
                    return Err(p.err(&format!("more than {MAX_ENTRIES} entries")));
                }
                out.push(Entry { section: sec, sub, key, value });
            }
            _ => return Err(p.err("a line that is neither a section, a key nor a comment")),
        }
    }
    Ok(out)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    line: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        if c == b'\n' {
            self.line += 1;
        }
        Some(c)
    }
    fn err(&self, what: &str) -> String {
        format!("line {}: {what}", self.line)
    }
    /// Spaces and tabs (and a CR, as git takes CRLF files), not line ends.
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\x0B' | b'\x0C')) {
            self.bump();
        }
    }
    fn skip_line(&mut self) {
        while let Some(c) = self.bump() {
            if c == b'\n' {
                break;
            }
        }
    }
    /// `section]`, `section "sub"]` or `section.sub]` (the `[` already read).
    fn header(&mut self) -> Result<(String, Option<String>), String> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'-' || c == b'.' {
                self.bump();
            } else {
                break;
            }
        }
        let name = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| self.err("a section name that is not text"))?.to_ascii_lowercase();
        if name.is_empty() {
            return Err(self.err("an empty section name"));
        }
        match self.peek() {
            Some(b']') => {
                self.bump();
                // The old form: `[branch.main]` is section "branch", subsection "main" (lower case).
                Ok(match name.split_once('.') {
                    Some((s, sub)) if !s.is_empty() && !sub.is_empty() => (s.to_string(), Some(sub.to_string())),
                    Some(_) => return Err(self.err("a section name with an empty part")),
                    None => (name, None),
                })
            }
            Some(b' ' | b'\t') => {
                if name.contains('.') {
                    return Err(self.err("a dotted section name with a subsection"));
                }
                self.skip_ws();
                if self.peek() != Some(b'"') {
                    return Err(self.err("a section header without its quoted subsection"));
                }
                self.bump();
                let mut sub = Vec::new();
                loop {
                    // Looked at before it is taken: a line end stays on its line for the error.
                    match self.peek() {
                        None | Some(b'\n') => return Err(self.err("a subsection without its closing quote")),
                        Some(b'"') => {
                            self.bump();
                            break;
                        }
                        Some(b'\\') => {
                            self.bump();
                            match self.peek() {
                                None | Some(b'\n') => return Err(self.err("a subsection without its closing quote")),
                                Some(c) => {
                                    self.bump();
                                    sub.push(c);
                                }
                            }
                        }
                        Some(c) => {
                            self.bump();
                            sub.push(c);
                        }
                    }
                }
                if self.peek() != Some(b']') {
                    return Err(self.err("a section header not closed right after its subsection"));
                }
                self.bump();
                Ok((name, Some(String::from_utf8_lossy(&sub).into_owned())))
            }
            _ => Err(self.err("a section header that is not closed")),
        }
    }
    /// A key: a letter, then letters, digits and `-`.
    fn name(&mut self) -> Result<String, String> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'-' {
                self.bump();
            } else {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&self.b[start..self.i]).to_ascii_lowercase())
    }
    /// A value up to its line's end (or a comment), as git reads it: quotes, escapes, a
    /// backslash-newline going on, whitespace outside quotes trimmed at the ends.
    fn value(&mut self) -> Result<String, String> {
        let mut out = Vec::new();
        let mut quoted = false;
        let mut spaces = 0usize;
        loop {
            let Some(c) = self.peek() else {
                if quoted {
                    return Err(self.err("a value without its closing quote"));
                }
                break;
            };
            if c == b'\n' {
                if quoted {
                    return Err(self.err("a value without its closing quote"));
                }
                self.bump();
                break;
            }
            self.bump();
            if !quoted && (c == b';' || c == b'#') {
                self.skip_line();
                break;
            }
            if !quoted && matches!(c, b' ' | b'\t' | b'\r' | b'\x0B' | b'\x0C') {
                if !out.is_empty() {
                    spaces += 1;
                }
                continue;
            }
            out.extend(std::iter::repeat_n(b' ', spaces));
            spaces = 0;
            match c {
                b'\\' => match self.bump() {
                    Some(b'\n') => {}
                    Some(b'\r') if self.peek() == Some(b'\n') => {
                        self.bump();
                    }
                    Some(b'n') => out.push(b'\n'),
                    Some(b't') => out.push(b'\t'),
                    Some(b'b') => out.push(0x08),
                    Some(b'\\') => out.push(b'\\'),
                    Some(b'"') => out.push(b'"'),
                    _ => return Err(self.err("a backslash before a character it does not escape")),
                },
                b'"' => quoted = !quoted,
                c => out.push(c),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

/// A repo's config files read with their includes: every entry in the order git would see them,
/// or why they could not all be read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    pub entries: Vec<Entry>,
    /// The first file that could not be read or understood, in words. Anything named in the
    /// files is then unknown: the repo is not read at all.
    pub problem: Option<String>,
    /// How many files were read (the configs and their includes).
    pub files: usize,
}

/// `files` (the repo's `config`, then `config.worktree`) and what they include. A missing
/// top-level file is skipped (a worktree without its own config). `home` resolves `~/`.
pub fn load(files: &[PathBuf], home: Option<&Path>) -> Loaded {
    let mut out = Loaded::default();
    let mut read = 0u64;
    for f in files {
        if out.problem.is_some() {
            break;
        }
        let mut chain = Vec::new();
        load_file(f, home, 0, &mut chain, &mut read, &mut out);
    }
    out
}

/// Up to `max` bytes of a file: Ok(None) when it is not there, Err for anything else (too big,
/// refused, a share's hiccup: unknown, never "not there").
pub fn read_capped(path: &Path, max: u64) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if matches!(e.raw_os_error(), Some(2 | 3)) => return Ok(None),
        Err(e) => return Err(format!("{}: {}", crate::text::shown_path(path), crate::walk::error_words(&e))),
    };
    let mut buf = Vec::new();
    f.take(max + 1).read_to_end(&mut buf).map_err(|e| format!("{}: {}", crate::text::shown_path(path), crate::walk::error_words(&e)))?;
    if buf.len() as u64 > max {
        return Err(format!("{} is larger than {} KiB", crate::text::shown_path(path), max / 1024));
    }
    Ok(Some(buf))
}

fn same_file_key(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").to_lowercase()
}

fn load_file(path: &Path, home: Option<&Path>, depth: usize, chain: &mut Vec<String>, read: &mut u64, out: &mut Loaded) {
    if depth > MAX_DEPTH {
        out.problem = Some(format!("includes nested more than {MAX_DEPTH} deep at {}", crate::text::shown_path(path)));
        return;
    }
    let key = same_file_key(path);
    if chain.contains(&key) {
        // A cycle: git refuses it ("exceeded maximum include depth"), and so does VibeRepos.
        out.problem = Some(format!("{} includes itself", crate::text::shown_path(path)));
        return;
    }
    if out.files >= MAX_FILES {
        out.problem = Some(format!("includes read more than {MAX_FILES} files at {}", crate::text::shown_path(path)));
        return;
    }
    let bytes = match read_capped(path, MAX_FILE) {
        Ok(Some(b)) => b,
        Ok(None) => return,
        Err(e) => {
            out.problem = Some(e);
            return;
        }
    };
    out.files += 1;
    *read += bytes.len() as u64;
    if *read > MAX_TOTAL {
        out.problem = Some(format!("includes read more than {} KiB in all at {}", MAX_TOTAL / 1024, crate::text::shown_path(path)));
        return;
    }
    let entries = match parse(&bytes) {
        Ok(e) => e,
        Err(e) => {
            out.problem = Some(format!("{}, {e}", crate::text::shown_path(path)));
            return;
        }
    };
    if out.entries.len() + entries.len() > MAX_ENTRIES {
        out.problem = Some(format!("includes hold more than {MAX_ENTRIES} entries at {}", crate::text::shown_path(path)));
        return;
    }
    chain.push(key);
    for e in entries {
        let include = e.key == "path" && (e.section == "include" && e.sub.is_none() || e.section == "includeif" && e.sub.is_some());
        let target = if include { e.value.clone() } else { None };
        // Checked at each entry too: a parent's entries after an include come on top of the
        // child's, so the check above alone is not a total (RIN4-2).
        if out.entries.len() >= MAX_ENTRIES {
            out.problem = Some(format!("includes hold more than {MAX_ENTRIES} entries at {}", crate::text::shown_path(path)));
            break;
        }
        out.entries.push(e);
        if let Some(t) = target
            && let Some(p) = include_path(&t, path, home)
        {
            // An include that may be on another machine (by where it leads: a share, a
            // mapped drive, a link on the way) is asked the way a gitdir is; a device path
            // is never opened (IN-VibeRepos-2, RIN4-1).
            if let Err(e) = crate::walk::unc_answers(&p) {
                out.problem = Some(format!("an include: {e}"));
                break;
            }
            load_file(&p, home, depth + 1, chain, read, out);
            if out.problem.is_some() {
                break;
            }
        }
    }
    chain.pop();
}

/// Where an include points: `~/x` under `home`, a relative path beside the including file, an
/// absolute one as it is. Empty or `%(prefix)/` ones lead nowhere VibeRepos reads (None).
pub fn include_path(value: &str, from: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let v = value.replace('/', "\\");
    if v.is_empty() || v.starts_with("%(prefix)") {
        return None;
    }
    if let Some(rest) = v.strip_prefix("~\\") {
        return home.map(|h| h.join(rest));
    }
    let p = PathBuf::from(&v);
    if p.is_absolute() || v.starts_with("\\\\") { Some(p) } else { Some(from.parent()?.join(p)) }
}

impl Loaded {
    /// The last value of `section.key` (no subsection), as git takes the last one.
    pub fn get(&self, section: &str, key: &str) -> Option<&Entry> {
        self.entries.iter().rev().find(|e| e.section == section && e.sub.is_none() && e.key == key)
    }

    /// A boolean the way git reads one (a bare key is true); None when absent or not a boolean.
    pub fn boolean(&self, section: &str, key: &str) -> Option<bool> {
        let e = self.get(section, key)?;
        parse_bool(e.value.as_deref())
    }

    /// The filter drivers the files define a program for (`filter.<driver>.clean`, `.smudge`,
    /// `.process`), each once, in the order first seen.
    pub fn filters(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in &self.entries {
            if e.section == "filter"
                && matches!(e.key.as_str(), "clean" | "smudge" | "process")
                && let Some(d) = &e.sub
                && !out.contains(d)
            {
                out.push(d.clone());
            }
        }
        out
    }

    /// The settings that name a program git would run on fetch (SPEC section 3, and a few more:
    /// DECISIONS.md): `core.sshCommand`, `core.gitProxy`, `core.askPass`,
    /// `core.alternateRefsCommand`, `credential.helper` (also per URL), `remote.<name>.uploadpack`
    /// and `remote.<name>.vcs` (a remote helper by name). An empty value names none.
    pub fn fetch_commands(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in &self.entries {
            if e.value.as_deref().is_some_and(|v| v.trim().is_empty()) {
                continue;
            }
            let name = match (e.section.as_str(), e.sub.is_some(), e.key.as_str()) {
                ("core", false, "sshcommand") => "core.sshCommand",
                ("core", false, "gitproxy") => "core.gitProxy",
                ("core", false, "askpass") => "core.askPass",
                ("core", false, "alternaterefscommand") => "core.alternateRefsCommand",
                ("credential", _, "helper") => "credential.helper",
                ("remote", true, "uploadpack") => "remote.<name>.uploadpack",
                ("remote", true, "vcs") => "remote.<name>.vcs",
                _ => continue,
            };
            if !out.iter().any(|o| o == name) {
                out.push(name.to_string());
            }
        }
        out
    }
}

/// git's booleans: true, yes, on, 1 (and a bare key); false, no, off, 0 and the empty string.
pub fn parse_bool(v: Option<&str>) -> Option<bool> {
    let Some(v) = v else { return Some(true) };
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" | "" => Some(false),
        n => n.parse::<i64>().ok().map(|n| n != 0),
    }
}

/// The `-c` arguments that switch a repo's filter drivers off for one git call. None when a
/// driver's name cannot be written as a `-c` key (it holds `=`, which git splits at, or a line
/// break): such a repo is not read with status at all.
pub fn filter_overrides(drivers: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for d in drivers {
        if d.contains(['=', '\n', '\r', '\0']) {
            return None;
        }
        for (k, v) in [("clean", ""), ("smudge", ""), ("process", ""), ("required", "false")] {
            out.push("-c".to_string());
            out.push(format!("filter.{d}.{k}={v}"));
        }
    }
    Some(out)
}
