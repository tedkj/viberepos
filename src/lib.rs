//! VibeRepos: Every git repo on this PC at a glance: what is not committed, not pushed, behind, or
//! left half done. A VibeWare app: Rust and Slint on the software renderer, one small exe, no
//! runtime beside it. Git is found on the PC, never bundled.
//!
//! The engine is pure where it can be (`discover`, `gitconfig`, `status`, `refs`, `remote`,
//! `model`, `export`), and the rest is small Windows code (`walk`, `run`, `gitexe`, `read`,
//! `fetch`, `ops`). `controller` ties it to the window, which only lays out and draws; window
//! tests run it on a scripted world (`fake`) without git.

pub mod backend;
pub mod check;
pub mod controller;
pub mod dialogs;
pub mod discover;
pub mod export;
pub mod fake;
pub mod fetch;
pub mod gitconfig;
pub mod gitexe;
pub mod host;
pub mod launch;
pub mod model;
pub mod ops;
pub mod portable;
pub mod prefs;
pub mod read;
pub mod refs;
pub mod remote;
pub mod run;
pub mod status;
pub mod text;
pub mod trust;
pub mod walk;

use serde::{Deserialize, Serialize};
use slint::ComponentHandle;

slint::include_modules!();

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the window remembers between runs (prefs.json in %APPDATA%\com.tedkj.viberepos, or beside
/// a portable exe).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// "dark" or "light" once the user has picked one; until then the system's choice.
    pub theme: Option<String>,
    /// The folders that hold the repos, as kept (a portable copy keeps one on its own drive as
    /// `?:\...`). None until the first start has looked for them.
    pub roots: Option<Vec<String>>,
    /// The filter chip: all, dirty, unpushed, behind, no-remote.
    pub filter: Option<String>,
    /// The sort: path, last-commit, fetched.
    pub sort: Option<String>,
    /// A git.exe the user chose (checked again at every start).
    pub git: Option<String>,
}

impl Prefs {
    pub fn dark(&self) -> bool {
        match self.theme.as_deref() {
            Some("dark") => true,
            Some("light") => false,
            _ => host::system_dark(),
        }
    }

    /// Only the values the window offers survive a hand-edited file; the rest fall back.
    pub fn sanitized(mut self) -> Prefs {
        if !matches!(self.theme.as_deref(), None | Some("dark") | Some("light")) {
            self.theme = None;
        }
        if self.filter.as_deref().is_some_and(|f| model::Filter::from_key(f).is_none()) {
            self.filter = None;
        }
        if self.sort.as_deref().is_some_and(|s| model::Sort::from_key(s).is_none()) {
            self.sort = None;
        }
        if self.git.as_deref().is_some_and(|g| !gitexe::named_git_exe(std::path::Path::new(g)) || !std::path::Path::new(g).is_absolute()) {
            self.git = None;
        }
        if let Some(r) = &mut self.roots {
            r.retain(|x| !x.trim().is_empty() && !x.contains('\0'));
            let mut seen: Vec<String> = Vec::new();
            r.retain(|x| {
                let k = x.to_lowercase();
                if seen.contains(&k) {
                    false
                } else {
                    seen.push(k);
                    true
                }
            });
            r.truncate(64);
        }
        self
    }

    pub fn filter(&self) -> model::Filter {
        self.filter.as_deref().and_then(model::Filter::from_key).unwrap_or_default()
    }

    pub fn sort(&self) -> model::Sort {
        self.sort.as_deref().and_then(model::Sort::from_key).unwrap_or_default()
    }
}

/// The window with its basics wired: theme, version, copying. The controller adds the rest (the
/// theme switch too: it saves on the prefs thread).
pub fn window(prefs: prefs::Store<Prefs>) -> Result<AppWindow, slint::PlatformError> {
    let ui = AppWindow::new()?;
    let p = prefs.load().sanitized();
    ui.global::<Theme>().set_dark(p.dark());
    ui.set_version(VERSION.into());
    ui.on_copy_text(|t| host::copy_text(&t).is_ok());
    Ok(ui)
}
