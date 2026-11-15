//! Copies and the CSV (SPEC section 6): one row as text (Copy status), the shown rows as an
//! aligned plain-text table (Copy all), and the shown rows as CSV in the user's own list
//! separator and decimal symbol (the family rule: RFC 4180, CRLF, a UTF-8 BOM, a leading
//! apostrophe on a field Excel would run as a formula). Text from outside is cleaned; remote URLs
//! carry no credentials. A copy reads everything a row knows, not what the window has room for.

use crate::model::{Row, View, cells};
use crate::ops::Fetched;
use crate::status::Head;

/// The separators a CSV is written with (VibeWiFi's, the family's).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvFormat {
    pub sep: String,
    pub dec: String,
}

impl Default for CsvFormat {
    fn default() -> Self {
        CsvFormat { sep: ",".into(), dec: ".".into() }
    }
}

impl CsvFormat {
    /// The locale's list separator (LOCALE_SLIST) and decimal symbol (LOCALE_SDECIMAL), checked:
    /// a separator that is empty, more than one character, a quote, a line break, a space or a
    /// digit cannot separate fields, and one equal to the decimal symbol would split numbers.
    /// Then: the decimal symbol decides (',' gets ';', anything else ',' and '.').
    pub fn from_locale(list: &str, dec: &str) -> CsvFormat {
        let one = |s: &str| {
            let mut c = s.chars();
            match (c.next(), c.next()) {
                (Some(x), None) => Some(x),
                _ => None,
            }
        };
        let dec_ok = one(dec).filter(|c| matches!(c, '.' | ',' | '\u{066B}' | '\u{00B7}'));
        let sep_ok = one(list).filter(|c| !matches!(c, '"' | '\r' | '\n' | ' ' | '\'' | '=') && !c.is_ascii_digit() && !c.is_alphabetic() && Some(*c) != dec_ok);
        match (sep_ok, dec_ok) {
            (Some(s), Some(d)) => CsvFormat { sep: s.to_string(), dec: d.to_string() },
            (None, Some(',')) => CsvFormat { sep: ";".into(), dec: ",".into() },
            (Some(s), None) if s != '.' => CsvFormat { sep: s.to_string(), dec: ".".into() },
            _ => CsvFormat::default(),
        }
    }

    /// One field, quoted when it must be (RFC 4180 with this separator: it, a quote, a line
    /// break or edge spaces). Text from outside (`text`) is cleaned of invisible characters, and
    /// one Excel would run as a formula (= + - @ first) is kept as text by a leading apostrophe.
    pub fn field(&self, v: &str, text: bool) -> String {
        let mut v = if text { crate::text::clean_max(v, 4096) } else { v.to_string() };
        if text && v.starts_with(['=', '+', '-', '@', '\t', '\r']) {
            v.insert(0, '\'');
        }
        if v.contains(self.sep.as_str()) || v.contains(['"', '\r', '\n']) || v.starts_with(' ') || v.ends_with(' ') {
            format!("\"{}\"", v.replace('"', "\"\""))
        } else {
            v
        }
    }
}

/// Days from 1970-01-01 to a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// "2026-10-23T14:02:11+02:00": local time with its offset.
pub fn iso_local(unix: i64) -> String {
    let (c, off) = crate::host::local(unix);
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    format!("{}T{}{sign}{:02}:{:02}", c.date(), c.time(), a / 60, a % 60)
}

pub const HEADER: [&str; 21] = [
    "path",
    "repo",
    "kind",
    "branch",
    "detached",
    "commit",
    "staged",
    "unstaged",
    "untracked",
    "conflicts",
    "operation",
    "stashes",
    "upstream",
    "ahead",
    "behind",
    "other branches unpushed",
    "fetched",
    "primary remote host",
    "remote URL",
    "last commit",
    "error",
];

/// The fields of one row, in HEADER's order, as text (unknown values empty).
pub fn fields(r: &Row, v: &View, iso: &dyn Fn(i64) -> String) -> [String; 21] {
    let c = cells(r, v);
    let info = r.info.as_ref();
    let status = info.and_then(|i| i.status.as_ref().ok());
    let num = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
    let primary = r.primary_remote();
    let url = primary.map(|p| crate::remote::parse(&p.url));
    [
        crate::text::shown_path(&r.found.path),
        c.repo.clone(),
        r.found.kind.word().into(),
        match status.map(|s| &s.head) {
            Some(Head::Branch(b)) => crate::text::clean(b),
            _ => String::new(),
        },
        match status.map(|s| &s.head) {
            Some(Head::Detached) => "yes".into(),
            Some(Head::Branch(_)) => "no".into(),
            _ => String::new(),
        },
        status.and_then(|s| s.oid.clone()).unwrap_or_default(),
        num(status.map(|s| s.staged)),
        num(status.map(|s| s.unstaged)),
        num(status.map(|s| s.untracked)),
        num(status.map(|s| s.conflicts)),
        info.and_then(|i| i.op.as_ref().ok().copied().flatten()).map(|o| o.word().to_string()).unwrap_or_default(),
        num(status.map(|s| s.stash)),
        status.and_then(|s| s.upstream.as_deref()).map(crate::text::clean).unwrap_or_default(),
        num(status.and_then(|s| s.ab).map(|a| a.0)),
        num(status.and_then(|s| s.ab).map(|a| a.1)),
        if info.is_some_and(|i| i.branches.is_ok()) { r.other_unpushed().to_string() } else { String::new() },
        match info.map(|i| &i.fetched) {
            Some(Fetched::At(t)) => iso(*t),
            _ => String::new(),
        },
        url.as_ref().map(|u| u.column()).unwrap_or_default(),
        url.as_ref().map(|u| u.shown.clone()).unwrap_or_default(),
        r.last_commit_time().map(iso).unwrap_or_default(),
        c.error.clone(),
    ]
}

/// The rows as CSV: BOM, header, one line per row, CRLF.
pub fn csv(rows: &[&Row], v: &View, f: &CsvFormat, iso: &dyn Fn(i64) -> String) -> String {
    let mut out = String::with_capacity(256 + rows.len() * 256);
    out.push('\u{FEFF}');
    out.push_str(&HEADER.join(&f.sep));
    out.push_str("\r\n");
    for r in rows {
        let fs = fields(r, v, iso);
        let cells: Vec<String> = fs.iter().enumerate().map(|(i, x)| f.field(x, !matches!(i, 6..=9 | 11 | 13..=15))).collect();
        out.push_str(&cells.join(&f.sep));
        out.push_str("\r\n");
    }
    out
}

/// One row as text (Copy status).
pub fn row_text(r: &Row, v: &View) -> String {
    let c = cells(r, v);
    let mut lines = vec![format!("{}{}", c.repo, if c.tag.is_empty() { String::new() } else { format!(" ({})", c.tag) }), crate::text::shown_path(&r.found.path)];
    let mut push = |k: &str, val: &str| {
        if !val.is_empty() {
            lines.push(format!("{k}: {val}"));
        }
    };
    push("In progress", &c.op);
    push("Branch", &c.branch);
    push("Changes", &c.changes);
    push("Sync", &c.sync);
    push("Stashes", &c.stash);
    push("Remote", &[c.remote.as_str(), c.hint.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · "));
    push("Last commit", &c.last);
    push("Fetched", &c.fetched);
    push("Error", &c.error);
    lines.join("\n")
}

/// The shown rows as an aligned plain-text table under a header line (Copy all).
pub fn table(rows: &[&Row], v: &View, header: &str) -> String {
    let heads = ["Repo", "Branch", "Changes", "Sync", "Stash", "Remote", "Last commit", "Fetched", "Error"];
    let body: Vec<[String; 9]> = rows
        .iter()
        .map(|r| {
            let c = cells(r, v);
            let repo = if c.tag.is_empty() { c.repo.clone() } else { format!("{} ({})", c.repo, c.tag) };
            let branch = if c.op.is_empty() { c.branch.clone() } else { format!("{} {}", c.op, c.branch) };
            let remote = if c.hint.is_empty() { c.remote.clone() } else { format!("{} · {}", c.remote, c.hint) };
            [repo, branch, c.changes, c.sync, c.stash, remote, c.last, c.fetched, c.error]
        })
        .collect();
    let mut w: Vec<usize> = heads.iter().map(|h| h.chars().count()).collect();
    for b in &body {
        for (i, x) in b.iter().enumerate() {
            w[i] = w[i].max(x.chars().count());
        }
    }
    let line = |cells: Vec<&str>| -> String {
        let mut s = String::new();
        for (i, x) in cells.iter().enumerate() {
            s.push_str(x);
            if i + 1 < cells.len() {
                s.push_str(&" ".repeat(w[i] - x.chars().count() + 2));
            }
        }
        s.trim_end().to_string()
    };
    let mut out = vec![header.to_string(), String::new(), line(heads.to_vec())];
    for b in &body {
        out.push(line(b.iter().map(String::as_str).collect()));
    }
    out.join("\n")
}
