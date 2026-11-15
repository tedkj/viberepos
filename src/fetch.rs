//! Fetch (SPEC section 5): the one thing VibeRepos does that changes refs. `git fetch --all
//! --recurse-submodules=no` with gc, maintenance, the commit-graph write, hooks, fsmonitor,
//! interactive credentials and the `ext::` transport switched off, the repo's filters switched
//! off as for the reads, 120 s per repo. Never pull, merge, rebase, checkout or prune beyond the
//! user's own `fetch.prune`. A repo whose own config names a program for fetch is not fetched;
//! that config is read again right before the fetch (what the read saw is a plan, not a fact).

use crate::read::{Ctx, Failure};
use crate::walk::Found;

/// How one fetch ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Fetched,
    /// In plain words (the row), and all of git's stderr (the panel).
    Failed(Failure),
    Skipped(String),
}

/// The `-c` switches a fetch adds to the reads' own.
pub const FETCH_SWITCHES: [&str; 12] = [
    "-c",
    "gc.auto=0",
    "-c",
    "maintenance.auto=false",
    "-c",
    "fetch.writeCommitGraph=false",
    "-c",
    "credential.interactive=false",
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "submodule.recurse=false",
];

/// Why a repo is not fetched, before trying (None: fetch it).
pub fn skip_reason(has_remote: bool, fetch_commands: &[String], config_problem: Option<&str>, dubious: bool, git_found: bool) -> Option<String> {
    if !git_found {
        return Some("git not found".into());
    }
    if dubious {
        return Some("git refuses to read it (dubious ownership)".into());
    }
    if let Some(p) = config_problem {
        return Some(format!("its config could not be checked ({}); fetch it yourself", crate::text::clean(p)));
    }
    if !fetch_commands.is_empty() {
        return Some(format!("its config sets a command for fetch ({}); fetch it yourself", fetch_commands.join(", ")));
    }
    if !has_remote {
        return Some("no remote".into());
    }
    None
}

/// git's stderr from a failed fetch in plain words (English: LC_ALL=C).
pub fn words(stderr: &str) -> String {
    let s = stderr;
    let has = |needles: &[&str]| needles.iter().any(|n| s.contains(n));
    if has(&["Host key verification failed", "REMOTE HOST IDENTIFICATION HAS CHANGED", "host key is known", "No matching host key"]) {
        return "SSH host key not trusted".into();
    }
    if has(&["Authentication failed", "terminal prompts disabled", "could not read Username", "could not read Password", "unable to get password", "unable to get username", "Permission denied (publickey", "Invalid username or password", "HTTP Basic: Access denied", "returned error: 401", "returned error: 403"]) {
        return "needs sign-in: fetch it once yourself".into();
    }
    if has(&["Could not resolve host", "Could not resolve hostname", "Connection timed out", "Connection refused", "Failed to connect", "Network is unreachable", "No route to host", "timed out", "Couldn't connect to server", "unable to access"]) {
        return "host not reachable".into();
    }
    if has(&["Repository not found", "does not appear to be a git repository", "repository not found", "not found", "does not exist"]) {
        return "repository not found".into();
    }
    let first = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("git failed");
    crate::text::clean_max(first, 200)
}

/// Fetch one repo (its common git folder). The config is checked again first.
pub fn fetch(ctx: &Ctx, found: &Found) -> Outcome {
    let config = crate::read::check_config(found, ctx.home.as_deref());
    if let Some(why) = skip_reason(true, &config.fetch_commands, config.problem.as_deref(), false, true) {
        return Outcome::Skipped(why);
    }
    let Some(mut overrides) = crate::gitconfig::filter_overrides(&config.filters) else {
        return Outcome::Skipped("its config names a filter that cannot be switched off; fetch it yourself".into());
    };
    let mut args: Vec<String> = FETCH_SWITCHES.iter().map(|s| s.to_string()).collect();
    args.append(&mut overrides);
    match ctx.call(&found.path, &args, &["fetch", "--all", "--recurse-submodules=no"], crate::run::FETCH_TIMEOUT, true) {
        Ok(_) => Outcome::Fetched,
        Err(f) if f.short.starts_with("timed out") => {
            let gd = found.common.as_deref().map(crate::text::shown_path).unwrap_or_default();
            Outcome::Failed(Failure { short: format!("timed out; git may have left a .lock file in {gd}"), full: f.full })
        }
        Err(f) => Outcome::Failed(Failure { short: words(&f.full), full: f.full }),
    }
}
