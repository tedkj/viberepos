//! Text from outside (folder names, branch names, remote URLs, commit subjects, git's stderr,
//! config values) as it may be shown: in the window, toasts, the status line, copies and the CSV.
//! Cleaned at display only; the path handed to Windows, git and the clipboard's "Copy path" stays
//! raw. (VibePrint's, copied.)

/// Characters that change how the text around them reads without showing themselves: controls,
/// bidirectional embeddings, overrides, isolates and marks, zero-width spaces and line
/// separators, the soft hyphen and the Unicode tag characters (U+E0000..U+E007F). The joiners
/// U+200C (ZWNJ) and U+200D (ZWJ) are not among them: emoji sequences and Persian, Arabic and
/// Indic names need them (family rule, 2026-10-20).
pub fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{E0000}'..='\u{E007F}')
}

/// Outside text as it may be shown: invisible characters gone, runs of whitespace (a newline
/// or tab included) one space, trimmed, at most `max` characters (an ellipsis marks a cut).
pub fn clean_max(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4));
    let mut space = false;
    let mut n = 0usize;
    let mut cut = false;
    for c in s.chars() {
        if c.is_whitespace() || c == '\u{2028}' || c == '\u{2029}' {
            space = !out.is_empty();
            continue;
        }
        if invisible(c) {
            continue;
        }
        if space {
            if n + 1 >= max {
                cut = true;
                break;
            }
            out.push(' ');
            n += 1;
            space = false;
        }
        if n >= max {
            cut = true;
            break;
        }
        out.push(c);
        n += 1;
    }
    if cut {
        // Room for the ellipsis: drop the last character (never a byte: chars only).
        out.pop();
        out.push('…');
    }
    out
}

/// Outside text, at most 255 characters.
pub fn clean(s: &str) -> String {
    clean_max(s, 255)
}

/// A path or other long outside text (git's stderr in the detail panel): at most 2 048
/// characters.
pub fn clean_long(s: &str) -> String {
    clean_max(s, 2048)
}

/// UTF-16 from Windows as shown text: a lone surrogate becomes U+FFFD, then cleaned (the
/// template's, for the install check's signer name).
pub fn clean_wide(w: &[u16], max: usize) -> String {
    clean_max(&String::from_utf16_lossy(w), max)
}

/// Bytes from git (UTF-8 by its rules, but nothing checks): invalid UTF-8 replaced, then cleaned.
pub fn clean_bytes(b: &[u8], max: usize) -> String {
    clean_max(&String::from_utf8_lossy(b), max)
}

/// A path as shown: a lone surrogate becomes U+FFFD, invisible characters go (the raw path is
/// what Explorer, the terminal and Copy path get).
pub fn shown_path(p: &std::path::Path) -> String {
    clean_max(&p.to_string_lossy(), 2048)
}

/// "1 repo", "5 repos".
pub fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{} {many}", thousands(n as u64)) }
}

/// 1234567 as "1 234 567" (a thin gap in groups of three, the spec's "1 234 folders").
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}
