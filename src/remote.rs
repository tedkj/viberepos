//! Remote URLs (SPEC section 4), pure: https, http, ssh://, git://, scp-like `user@host:path`,
//! `file://`, a plain path, parsed into host and path. Credentials are never shown or copied: the
//! user info (and a query string, where a token can hide too) is gone from every URL VibeRepos
//! shows ("https://github.com/x/y.git", never "https://tok@..."). One that cannot be parsed is
//! shown cleaned and without anything before an `@`.
//!
//! No public/private word: VibeRepos does not ask GitHub, and a repo's name says nothing about
//! its visibility (the old `-pub` naming rule was one user's convention, copy-ux M9).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Https,
    Http,
    Ssh,
    Git,
    /// `user@host:path`
    Scp,
    /// `file://` or a plain path: another folder on this PC or a share.
    Local,
    /// Another scheme or a remote helper (`x::y`): only cleaned.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub kind: Kind,
    /// Lower case; None for a local folder or what could not be parsed.
    pub host: Option<String>,
    /// The path on the host (`x/y.git`, without a leading slash), as written.
    pub path: String,
    /// The URL as shown: no user info, no query, cleaned of invisible characters.
    pub shown: String,
}

impl Url {
    /// The Remote column's word for it: the host, "local folder", or the cleaned URL.
    pub fn column(&self) -> String {
        match (&self.kind, &self.host) {
            (Kind::Local, _) => "local folder".into(),
            (_, Some(h)) => h.clone(),
            _ => self.shown.clone(),
        }
    }
}

/// Everything after the last `@` (user info cannot hold a raw `@`; a password with one is
/// written `%40`).
fn after_at(s: &str) -> &str {
    s.rsplit_once('@').map_or(s, |(_, r)| r)
}

fn drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || b[2] == b'\\' || b[2] == b'/')
}

/// The host part of an authority (`[::1]:22`, `host:443`, `host`): lower case, without its port.
fn host_of(authority: &str) -> Option<String> {
    let a = after_at(authority);
    let h = if let Some(rest) = a.strip_prefix('[') {
        rest.split_once(']').map(|(h, _)| h)?
    } else {
        a.split(':').next()?
    };
    let h = h.trim();
    (!h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%'))).then(|| h.to_ascii_lowercase())
}

pub fn parse(raw: &str) -> Url {
    let s = raw.trim();
    let clean = |t: &str| crate::text::clean(t);
    // Without a query or fragment: a token can hide in either.
    let cut = |t: &str| -> String { t.split(['?', '#']).next().unwrap_or("").to_string() };
    if let Some((scheme, rest)) = s.split_once("://") {
        // The query and fragment go before the authority is split from the path, as git splits
        // them (`credential.c`: a `?` or `#` ends the host part): `file://u@?token=x`, `https://h?t`.
        let rest_cut = cut(rest);
        let rest = rest_cut.as_str();
        let scheme_l = scheme.to_ascii_lowercase();
        let kind = match scheme_l.as_str() {
            "https" => Kind::Https,
            "http" => Kind::Http,
            "ssh" | "git+ssh" | "ssh+git" => Kind::Ssh,
            "git" => Kind::Git,
            "file" => Kind::Local,
            _ => Kind::Other,
        };
        if kind == Kind::Local {
            // User info lives in the authority only (before the path's first slash): an `@`
            // in the path is the folder's own.
            let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            return Url { kind, host: None, path: rest.to_string(), shown: clean(&format!("file://{}{path}", after_at(authority))) };
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let host = host_of(authority);
        let bare_auth = after_at(authority);
        let path = path.to_string();
        let shown = clean(&format!("{scheme_l}://{bare_auth}{}{path}", if path.is_empty() && !rest.contains('/') { "" } else { "/" }));
        return Url { kind: if host.is_some() { kind } else { Kind::Other }, host, path, shown };
    }
    if s.contains("::") && !drive_path(s) && !s.starts_with("\\\\") {
        // A remote helper (`hg::https://...`): only cleaned, user info gone.
        return Url { kind: Kind::Other, host: None, path: String::new(), shown: clean(after_at(&cut(s))) };
    }
    // scp-like: a colon before any slash, and not a drive letter. `[user@host:port]:path` is
    // git's way of writing a port; otherwise everything up to the last `@` that still has a
    // colon after it is user info (`tok:SECRET@host:path` included).
    if !drive_path(s) {
        if let Some(inner_end) = s.strip_prefix('[').and_then(|r| r.find("]:")) {
            let inner = &s[1..1 + inner_end];
            let path = cut(&s[inner_end + 3..]);
            if let Some(h) = host_of(inner) {
                return Url { kind: Kind::Scp, shown: clean(&format!("[{}]:{path}", after_at(inner))), host: Some(h), path };
            }
        } else {
            let sc = cut(s);
            let t = match sc.char_indices().rfind(|&(i, c)| c == '@' && sc[i + 1..].contains(':')) {
                Some((i, _)) => &sc[i + 1..],
                None => sc.as_str(),
            };
            if let Some(colon) = t.find(':')
                && colon > 0
                && !t[..colon].contains(['/', '\\'])
            {
                let (authority, path) = (&t[..colon], &t[colon + 1..]);
                if let Some(h) = host_of(authority) {
                    let path = path.to_string();
                    return Url { kind: Kind::Scp, shown: clean(&format!("{authority}:{path}")), host: Some(h), path };
                }
                return Url { kind: Kind::Other, host: None, path: String::new(), shown: clean(after_at(&sc)) };
            }
        }
    }
    // A plain path: a folder on this PC or a share, shown exactly (an `@` in it is the folder's
    // own). What git would take for scp-like (a colon before any slash) but this could not
    // parse keeps nothing before an `@`.
    let first_colon = s.find(':');
    let local = drive_path(s) || first_colon.is_none_or(|c| s[..c].contains(['/', '\\']));
    Url { kind: Kind::Local, host: None, path: s.to_string(), shown: clean(if local { s } else { after_at(s) }) }
}

/// The GitHub login the user's git config names: `github.user`, else the login in a GitHub
/// no-reply `user.email` (`<id>+<login>@users.noreply.github.com` or
/// `<login>@users.noreply.github.com`).
pub fn login(github_user: Option<&str>, email: Option<&str>) -> Option<String> {
    let valid = |l: &str| !l.is_empty() && l.len() <= 39 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if let Some(u) = github_user.map(str::trim).filter(|u| valid(u)) {
        return Some(u.to_string());
    }
    let e = email?.trim();
    let (local, domain) = e.rsplit_once('@')?;
    if !domain.eq_ignore_ascii_case("users.noreply.github.com") {
        return None;
    }
    let l = match local.split_once('+') {
        Some((id, l)) if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) => l,
        Some(_) => return None,
        None => local,
    };
    valid(l).then(|| l.to_string())
}
