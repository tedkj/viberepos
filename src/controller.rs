//! The window's brain. It lives on the UI thread: it keeps the prefs (in memory, saved on the
//! prefs thread), the rows, the selection, the filter and sort, the toasts and the theme, and it
//! starts every piece of work on a worker: finding git, the walk, the reads (four at a time),
//! the fetches (four at a time). Every answer carries the epoch it belongs to (the scan's seq, the
//! fetch run's id) and an answer for another epoch does nothing. The .slint files lay out what it
//! hands them; every word and choice is made here.

use crate::backend::Backend;
use crate::check::{Dot, Fetched, Local, Release, Rows};
use crate::export::CsvFormat;
use crate::fetch::Outcome;
use crate::gitexe::{self, Git};
use crate::model::{self, FetchState, Filter, Row, Sort, View};
use crate::portable::Mode;
use crate::prefs::{Change, Store};
use crate::read::Info;
use crate::text::{clean, count, thousands};
use crate::walk::{Found, Progress, Walked};
use crate::{AppWindow, CheckDot, CheckRow, ChipItem, DetailLine, Prefs, RepoRow, RootItem, Theme, ToastItem, ToastKind};
use slint::platform::Key;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A dialog's answer, called on the dialog's own thread.
pub type Answer = Box<dyn FnOnce(Option<PathBuf>) + Send>;
/// Opens a folder (Explorer, a terminal) or a web page; why not, when it cannot.
pub type Opener<T> = Box<dyn Fn(&T) -> Result<(), String>>;

/// What the controller needs from the outside world; the tests hand it fakes.
pub struct Services {
    pub copy: Box<dyn Fn(&str) -> bool>,
    /// A path's exact units onto the clipboard.
    pub copy_path: Box<dyn Fn(&Path) -> bool>,
    pub open_folder: Opener<Path>,
    pub open_terminal: Opener<Path>,
    pub open_url: Opener<str>,
    /// A saved file shown selected in Explorer (a toast's "Show").
    pub reveal: Box<dyn Fn(&Path)>,
    /// The folder picker (Roots: Add folder), its start folder.
    pub pick_folder: Box<dyn Fn(Option<PathBuf>, Answer)>,
    /// The git.exe picker.
    pub pick_git: Box<dyn Fn(Answer)>,
    /// Save As for the CSV, its default name.
    pub save_csv: Box<dyn Fn(String, Answer)>,
    pub backend: Arc<dyn Backend>,
    /// Now, Unix seconds.
    pub now: Box<dyn Fn() -> i64>,
    /// A moment as "14:02:11" and as "2026-10-23", local time (tests: UTC).
    pub clock: Box<dyn Fn(i64) -> (String, String)>,
    /// A moment for the CSV ("2026-10-23T14:02:11+02:00").
    pub iso: Arc<dyn Fn(i64) -> String + Send + Sync>,
    pub csv_format: Box<dyn Fn() -> CsvFormat>,
    pub mode: Mode,
    /// The exe's drive letter (a portable copy keeps roots on it relative to it).
    pub exe_drive: Option<char>,
    /// Tests: work runs on the UI thread, at once or held (`hold`) until `release`.
    pub manual: bool,
    pub hold: Cell<bool>,
    /// The install check's offline half, on its worker: this copy's files, their signatures and
    /// hashes (`trust::inspect_from`; true: installed, so the uninstaller too).
    pub inspect: Fetcher<bool, Local>,
    /// The install check's online half, on its worker: the release list at a path on the site,
    /// waited for at most `release.wait`. Only `main` hands in the real one (the exe's
    /// `net::fetch_release_list`, in no test build); the tests hand in fakes.
    pub fetch: Fetcher<str, Fetched>,
    /// What the check holds the list to: the app, its version, the list's key, the code signer.
    pub release: Release,
}

/// A blocking call the check's worker makes.
pub type Fetcher<A, R> = Arc<dyn Fn(&A) -> R + Send + Sync>;

impl Services {
    /// The real services. `fetch` is the release-list fetcher: `main` hands in the exe's
    /// `net::fetch_release_list`, and is the only place that names it.
    pub fn system(mode: Mode, exe_drive: Option<char>, local_data: PathBuf, fetch: Fetcher<str, Fetched>) -> Services {
        // The exe's identity as this process started from it, taken now, before an install
        // could replace it, and handed to every check (FED3, RVD7).
        let started = crate::trust::exe_identity();
        let backend: Arc<dyn Backend> = Arc::new(crate::backend::System { local_data, extra_env: Vec::new() });
        // A dialog's answer comes on its own thread: it is handed to the window's thread here.
        let marshal = |done: Answer| -> Answer {
            let done = Arc::new(Mutex::new(Some(done)));
            Box::new(move |p| {
                let d = done.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(f) = lock(&d).take() {
                        f(p);
                    }
                });
            })
        };
        Services {
            copy: Box::new(|t| crate::host::copy_text(t).is_ok()),
            copy_path: Box::new(|p| crate::host::copy_path(p).is_ok()),
            // Started on a thread of their own: a terminal's current folder on a share that does
            // not answer holds CreateProcess for ~21 s. A failure comes back as a toast.
            open_folder: Box::new(|p| {
                let p = p.to_path_buf();
                std::thread::spawn(move || {
                    if let Err(e) = crate::host::reveal(&p, false) {
                        toast_later(e);
                    }
                });
                Ok(())
            }),
            open_terminal: Box::new(|p| {
                if crate::walk::units(p).len() > crate::host::MAX_CWD {
                    return Err("Too long for a terminal's current folder".into());
                }
                let p = p.to_path_buf();
                std::thread::spawn(move || {
                    if let Err(e) = crate::host::open_terminal(&p) {
                        toast_later(e);
                    }
                });
                Ok(())
            }),
            open_url: Box::new(crate::host::open_url),
            reveal: Box::new(|p| {
                let _ = crate::host::reveal(p, true);
            }),
            pick_folder: Box::new(move |start, done| crate::dialogs::pick_folder(crate::host::active_window(), start, marshal(done))),
            pick_git: Box::new(move |done| crate::dialogs::pick_git(crate::host::active_window(), marshal(done))),
            save_csv: Box::new(move |name, done| crate::dialogs::save_csv(crate::host::active_window(), name, marshal(done))),
            backend,
            now: Box::new(|| crate::host::now_unix_ms() / 1000),
            clock: Box::new(|t| {
                let c = crate::host::local(t).0;
                (c.time(), c.date())
            }),
            iso: Arc::new(crate::export::iso_local),
            csv_format: Box::new(crate::host::csv_format),
            mode,
            exe_drive,
            manual: false,
            hold: Cell::new(false),
            inspect: Arc::new(move |installed: &bool| crate::trust::inspect_from(started, &crate::trust::own_exe(), *installed)),
            fetch,
            release: Release::this(),
        }
    }
}

/// How often the window looks whether a running check has its answer.
const CHECK_POLL_MS: u64 = 50;

/// `fetch` on a thread of its own, waited for at most `deadline` (FETCH_DEADLINE, review RAB-3):
/// past it the answer is `TimedOut`, whatever the site or a proxy is doing, and the thread is
/// left to end by itself (the real fetcher's read loop stops at the same deadline, and WinHTTP's
/// own timeouts bound each step); its late answer goes nowhere.
fn fetch_within(fetch: Fetcher<str, Fetched>, path: String, deadline: Duration) -> Fetched {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(fetch(&path));
    });
    match rx.recv_timeout(deadline) {
        Ok(f) => f,
        Err(RecvTimeoutError::Timeout) => Fetched::TimedOut,
        Err(RecvTimeoutError::Disconnected) => Fetched::Failed("the fetch stopped before it answered".into()),
    }
}

/// What Windows records of any program it runs: said where portable mode is (the chip's tip; it
/// was About's portable paragraph).
const PORTABLE_TRACE: &str = "Nothing goes to this PC's profile or registry, but Windows still records that a program ran (Prefetch, recent-programs lists).";

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One change to the prefs, applied by the prefs thread onto the newest saved ones.
type PrefsChange = Change<Prefs>;

/// After a change, when the window looks whether the prefs thread met a garbled file.
const PREFS_TELL_MS: u64 = 500;
/// Reads and fetches at a time.
pub const POOL: usize = 4;
/// Back in the window this long after the last refresh: refresh.
pub const STALE_SECS: i64 = 60;
pub const GET_GIT: &str = "https://git-scm.com/download/win";

/// The thread that saves prefs changes, in order (the scaffold's).
fn prefs_writer(store: Store<Prefs>, pending: Arc<AtomicUsize>) -> Sender<PrefsChange> {
    let (tx, rx) = channel::<PrefsChange>();
    std::thread::spawn(move || {
        while let Ok(f) = rx.recv() {
            store.change(Arc::new(move |p: &mut Prefs| {
                *p = std::mem::take(p).sanitized();
                f(p);
            }));
            pending.fetch_sub(1, Ordering::SeqCst);
        }
    });
    tx
}

/// A worker's answer to the window; each carries the epoch it belongs to.
pub enum Msg {
    Git { found: gitexe::Found, chosen: Option<PathBuf> },
    Login(Option<String>),
    FirstRoots(Vec<PathBuf>),
    Progress(u64, Progress),
    Walked(u64, Walked),
    Read(u64, usize, Box<Info>),
    FetchStarted(u64, usize),
    FetchDone(u64, usize, Outcome),
    /// Stop came first: never started.
    FetchNotStarted(u64, usize),
    RootPicked(Option<PathBuf>),
    GitPicked(Option<PathBuf>),
    SaveTo(Option<PathBuf>),
    Saved(Result<PathBuf, String>),
}

/// Where git stands.
#[derive(Clone, Debug, Default, PartialEq)]
enum GitState {
    #[default]
    Looking,
    Found(Git),
    Missing,
    TooOld(Git),
    Broken { exe: PathBuf, why: String },
}

/// A fetch run.
struct FetchRun {
    id: u64,
    /// Groups to fetch (one per common git folder).
    total: usize,
    started: usize,
    ok: usize,
    failed: usize,
    not_started: usize,
    /// Repos with a remote left out before the run (their config, git refusing them).
    skipped: usize,
    /// Groups whose config changed between the read and the fetch: not fetched after all.
    late_skips: usize,
    stop: Arc<AtomicBool>,
    /// lead row -> every row sharing its git folder.
    groups: HashMap<usize, Vec<usize>>,
}

impl FetchRun {
    fn done(&self) -> usize {
        self.ok + self.failed + self.not_started
    }
}

#[derive(Default)]
struct State {
    prefs: Prefs,
    git: GitState,
    login: Option<String>,
    seq: u64,
    scanning: bool,
    progress: Option<Progress>,
    walked: Option<Walked>,
    rows: Vec<Row>,
    /// First reads still out for this scan.
    reading: usize,
    read_at: Option<i64>,
    refreshed_at: Option<i64>,
    scan_cancel: Arc<AtomicBool>,
    order: Vec<usize>,
    visible: Vec<usize>,
    selected: Option<usize>,
    /// The path to select again after a rescan.
    keep: Option<PathBuf>,
    name: String,
    fetch: Option<FetchRun>,
    fetch_seq: u64,
    /// A refresh asked for while a fetch ran: done when it ends.
    refresh_after_fetch: bool,
    roots_changed: bool,
    started: bool,
    toast_seq: i32,
    toast_paths: HashMap<i32, PathBuf>,
    /// Each toast's timer generation (only the newest timer counts) and its full time in ms.
    toast_time: HashMap<i32, (u64, u64)>,
    /// About's install check: its number (an answer for another one does nothing), and while it
    /// runs, where its answer comes from.
    check_seq: u64,
    check_rx: Option<Receiver<(u64, Rows)>>,
}

/// Work the tests hold back until `release`.
type Held = Box<dyn FnOnce(&dyn Fn(Msg))>;
/// One job for the pool; it reports through the sender it is handed.
type Work = Box<dyn FnOnce(&dyn Fn(Msg)) + Send>;

thread_local! {
    static APP: RefCell<std::rc::Weak<App>> = const { RefCell::new(std::rc::Weak::new()) };
}

pub struct App {
    ui: Weak<AppWindow>,
    svc: Services,
    store: Store<Prefs>,
    st: RefCell<State>,
    prefs_tx: Sender<PrefsChange>,
    /// Changes sent to the prefs thread and not yet handed to the store.
    prefs_pending: Arc<AtomicUsize>,
    toasts: Rc<VecModel<ToastItem>>,
    rows_model: Rc<VecModel<RepoRow>>,
    me: RefCell<std::rc::Weak<App>>,
    inbox: Arc<Mutex<Vec<Msg>>>,
    held_jobs: RefCell<Vec<Held>>,
    check_rows: Rc<VecModel<CheckRow>>,
    check_timer: Timer,
}

fn s(t: impl AsRef<str>) -> SharedString {
    t.as_ref().into()
}

fn key(k: Key) -> SharedString {
    k.into()
}

impl App {
    /// Wire the controller to a window made by `crate::window`, and start: find git, then the
    /// roots (a first start looks in the usual places), then the first refresh.
    pub fn attach(ui: &AppWindow, store: Store<Prefs>, svc: Services) -> Rc<App> {
        let prefs = store.load().sanitized();
        let prefs_pending = Arc::new(AtomicUsize::new(0));
        let prefs_tx = prefs_writer(store.clone(), prefs_pending.clone());
        let name = String::new();
        let app = Rc::new(App {
            ui: ui.as_weak(),
            svc,
            store,
            st: RefCell::new(State { prefs, name, ..State::default() }),
            prefs_tx,
            prefs_pending,
            toasts: Rc::new(VecModel::default()),
            rows_model: Rc::new(VecModel::default()),
            me: RefCell::new(std::rc::Weak::new()),
            inbox: Arc::new(Mutex::new(Vec::new())),
            held_jobs: RefCell::new(Vec::new()),
            check_rows: Rc::new(VecModel::default()),
            check_timer: Timer::default(),
        });
        *app.me.borrow_mut() = Rc::downgrade(&app);
        APP.with(|a| *a.borrow_mut() = Rc::downgrade(&app));
        ui.set_toasts(ModelRc::from(app.toasts.clone()));
        ui.set_rows(ModelRc::from(app.rows_model.clone()));
        ui.set_check_rows(ModelRc::from(app.check_rows.clone()));
        app.show_rows(&Rows::idle());
        let bits = if cfg!(target_pointer_width = "64") { "64-bit" } else { "32-bit" };
        let how = if app.svc.mode.portable() { "portable" } else { "installed for you" };
        ui.set_about_line(s(format!("version {} · {bits} · {how}", crate::VERSION)));
        app.wire(ui);
        if app.store.broken() {
            app.toast("prefs.json could not be read: the settings are back to their defaults, and the old file will be kept as prefs.json.broken.", ToastKind::Error, None);
        }
        if matches!(app.svc.mode, Mode::ReadOnly { .. }) {
            app.toast("Portable, read-only: settings will not be kept.", ToastKind::Info, None);
        }
        app.render();
        ui.invoke_focus_table();
        let chosen = app.st.borrow().prefs.git.clone().map(PathBuf::from);
        app.find_git(chosen);
        app
    }

    fn wire(self: &Rc<Self>, ui: &AppWindow) {
        macro_rules! on {
            ($setter:ident, |$a:ident $(, $arg:ident)*| $body:expr) => {{
                let w = Rc::downgrade(self);
                ui.$setter(move |$($arg),*| {
                    let Some($a) = w.upgrade() else { return Default::default() };
                    $body
                });
            }};
        }
        on!(on_toggle_theme, |a| a.toggle_theme());
        on!(on_check_install, |a| a.check_install());
        on!(on_open_site, |a| a.open_site());
        on!(on_copy_text, |a, t| (a.svc.copy)(&t));
        on!(on_toast_dismissed, |a, id| a.drop_toast(id));
        on!(on_toast_action, |a, id| a.toast_action(id));
        on!(on_dialogs_closed, |a| a.dialogs_closed());
        on!(on_refresh, |a| a.refresh());
        on!(on_fetch_all, |a| a.fetch_all());
        on!(on_stop_fetch, |a| a.stop_fetch());
        on!(on_open_roots, |a| a.open_roots());
        on!(on_copy_all, |a| a.copy_all());
        on!(on_save_csv, |a| a.save_csv());
        on!(on_chip_picked, |a, i| a.chip(i));
        on!(on_name_edited, |a, t| a.name_edited(&t));
        on!(on_name_escaped, |a| a.name_escaped());
        on!(on_get_git, |a| a.get_git());
        on!(on_choose_git, |a| a.choose_git());
        on!(on_row_clicked, |a, id| a.select_row(id as usize, false));
        on!(on_row_opened, |a, id| {
            a.select_row(id as usize, false);
            a.open_folder();
        });
        on!(on_sort, |a, k| a.sort(k));
        on!(on_empty_clicked, |a| a.add_root());
        on!(on_open_folder, |a| a.open_folder());
        on!(on_open_terminal, |a| a.open_terminal());
        on!(on_copy_path, |a| a.copy_path());
        on!(on_fetch_one, |a| a.fetch_one());
        on!(on_copy_command, |a| a.copy_command());
        on!(on_roots_add, |a| a.add_root());
        on!(on_roots_remove, |a, i| a.remove_root(i as usize));
        on!(on_roots_closed, |a| a.close_roots());
        on!(on_key, |a, t, ctrl, shift| a.key(&t, ctrl, shift));
    }

    fn ui(&self) -> Option<AppWindow> {
        self.ui.upgrade()
    }

    fn now(&self) -> i64 {
        (self.svc.now)()
    }

    // ---------- workers ----------

    /// Run `work` off the UI thread; each message it sends comes back to `handle` (tests: at
    /// once, or held until `release`). A burst of messages is one wake-up of the window.
    fn bg(&self, work: impl FnOnce(&dyn Fn(Msg)) + Send + 'static) {
        if self.svc.manual {
            if self.svc.hold.get() {
                self.held_jobs.borrow_mut().push(Box::new(work));
            } else {
                let out = RefCell::new(Vec::new());
                work(&|m| out.borrow_mut().push(m));
                for m in out.into_inner() {
                    self.handle(m);
                }
                self.render();
            }
            return;
        }
        let post = self.poster();
        let _ = std::thread::Builder::new().name("worker".into()).spawn(move || work(&|m| post(m)));
    }

    /// A sender for worker threads: the message lands in the inbox, and the first of a burst
    /// wakes the window.
    fn poster(&self) -> Arc<dyn Fn(Msg) + Send + Sync> {
        let inbox = self.inbox.clone();
        let manual = self.svc.manual;
        Arc::new(move |m| {
            let first = {
                let mut i = lock(&inbox);
                i.push(m);
                i.len() == 1
            };
            if first && !manual {
                let _ = slint::invoke_from_event_loop(|| {
                    APP.with(|a| {
                        if let Some(app) = a.borrow().upgrade() {
                            app.drain();
                        }
                    })
                });
            }
        })
    }

    /// Run `jobs` on a pool of POOL threads (tests: each as its own `bg` job).
    fn pool(&self, jobs: Vec<Work>) {
        if self.svc.manual {
            for j in jobs {
                self.bg(j);
            }
            return;
        }
        let queue = Arc::new(Mutex::new(jobs.into_iter().collect::<VecDeque<_>>()));
        let post = self.poster();
        for _ in 0..POOL {
            let (q, post) = (queue.clone(), post.clone());
            let _ = std::thread::Builder::new().name("pool".into()).spawn(move || {
                loop {
                    let job = lock(&q).pop_front();
                    let Some(job) = job else { return };
                    job(&|m| post(m));
                }
            });
        }
    }

    /// Answers the workers left in the inbox, handled in order, then one render.
    pub fn drain(&self) {
        let msgs = std::mem::take(&mut *lock(&self.inbox));
        if msgs.is_empty() {
            return;
        }
        for m in msgs {
            self.handle(m);
        }
        self.render();
    }

    /// Tests: the work held back so far.
    pub fn held(&self) -> usize {
        self.held_jobs.borrow().len()
    }

    /// Tests: let the held work answer, in the order it was started (work it starts runs too,
    /// unless `hold` is still on).
    pub fn release(&self) {
        loop {
            let jobs = std::mem::take(&mut *self.held_jobs.borrow_mut());
            if jobs.is_empty() {
                return;
            }
            for j in jobs {
                let out = RefCell::new(Vec::new());
                j(&|m| out.borrow_mut().push(m));
                for m in out.into_inner() {
                    self.handle(m);
                }
            }
            self.render();
            if self.svc.hold.get() {
                return;
            }
        }
    }

    /// Tests: release only the first `n` held jobs.
    pub fn release_some(&self, n: usize) {
        let jobs: Vec<Held> = {
            let mut h = self.held_jobs.borrow_mut();
            let n = n.min(h.len());
            h.drain(..n).collect()
        };
        for j in jobs {
            let out = RefCell::new(Vec::new());
            j(&|m| out.borrow_mut().push(m));
            for m in out.into_inner() {
                self.handle(m);
            }
        }
        self.render();
    }

    fn handle(&self, m: Msg) {
        match m {
            Msg::Git { found, chosen } => self.git_found(found, chosen),
            Msg::Login(l) => self.st.borrow_mut().login = l,
            Msg::FirstRoots(v) => self.first_roots(v),
            Msg::Progress(seq, p) => {
                let mut st = self.st.borrow_mut();
                if seq == st.seq && st.scanning {
                    st.progress = Some(p);
                }
            }
            Msg::Walked(seq, w) => self.walked(seq, w),
            Msg::Read(seq, idx, info) => {
                let mut st = self.st.borrow_mut();
                if seq != st.seq || idx >= st.rows.len() {
                    return;
                }
                if st.rows[idx].info.is_none() {
                    st.reading = st.reading.saturating_sub(1);
                }
                st.rows[idx].info = Some(*info);
                if st.reading == 0 && st.read_at.is_none() {
                    st.read_at = Some(self.now());
                }
            }
            Msg::FetchStarted(id, lead) => self.fetch_state(id, lead, FetchState::Fetching),
            Msg::FetchDone(id, lead, o) => self.fetch_done(id, lead, o),
            Msg::FetchNotStarted(id, lead) => {
                let ok = self.st.borrow().fetch.as_ref().is_some_and(|f| f.id == id);
                if ok {
                    self.fetch_state(id, lead, FetchState::Idle);
                    if let Some(f) = self.st.borrow_mut().fetch.as_mut() {
                        f.not_started += 1;
                    }
                    self.maybe_end_fetch();
                }
            }
            Msg::RootPicked(p) => self.root_picked(p),
            Msg::GitPicked(p) => self.git_picked(p),
            Msg::SaveTo(p) => self.save_to(p),
            Msg::Saved(r) => match r {
                Ok(p) => {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    self.toast(&format!("Saved {}", clean(&name)), ToastKind::Ok, Some(p));
                }
                Err(e) => self.toast(&format!("Could not save the CSV: {}", clean(&e)), ToastKind::Error, None),
            },
        }
    }

    // ---------- git ----------

    fn find_git(&self, chosen: Option<PathBuf>) {
        let backend = self.svc.backend.clone();
        self.bg(move |send| {
            let found = backend.find_git(chosen.as_deref());
            send(Msg::Git { found, chosen });
        });
    }

    fn git_found(&self, found: gitexe::Found, chosen: Option<PathBuf>) {
        let picked_now = self.st.borrow().started;
        let state = match &found {
            gitexe::Found::Ok(g) => GitState::Found(g.clone()),
            gitexe::Found::TooOld(g) => GitState::TooOld(g.clone()),
            gitexe::Found::Missing => GitState::Missing,
            gitexe::Found::Broken { exe, why } => GitState::Broken { exe: exe.clone(), why: why.clone() },
        };
        if let Some(c) = &chosen {
            let used = found.git().is_some_and(|g| g.exe.as_os_str().eq_ignore_ascii_case(c.as_os_str()));
            if picked_now {
                // Choose git.exe…: kept only when that very file answered as a usable git.
                if !used {
                    let why = match &state {
                        GitState::TooOld(g) => format!("VibeRepos needs Git 2.36 or newer; that one is {}.", clean(&g.version)),
                        GitState::Broken { why, .. } => format!("It did not answer as git: {}.", clean(why)),
                        _ => "It could not be used.".into(),
                    };
                    self.toast(&format!("{} is not kept. {why}", crate::text::shown_path(c)), ToastKind::Error, None);
                    return;
                }
                let keep = c.to_string_lossy().into_owned();
                self.save_prefs(move |p| p.git = Some(keep.clone()));
            } else if !used {
                self.toast(&format!("The git.exe chosen before ({}) cannot be used now: VibeRepos looked in PATH and the usual folders instead.", crate::text::shown_path(c)), ToastKind::Error, None);
            }
        }
        self.st.borrow_mut().git = state;
        if let Some(g) = found.git().cloned() {
            let backend = self.svc.backend.clone();
            self.bg(move |send| send(Msg::Login(backend.login(&g))));
        }
        let first = !std::mem::replace(&mut self.st.borrow_mut().started, true);
        if first {
            if self.st.borrow().prefs.roots.is_none() {
                let backend = self.svc.backend.clone();
                self.bg(move |send| send(Msg::FirstRoots(backend.first_roots())));
            } else {
                self.refresh();
            }
        } else {
            // A git chosen by hand: read everything with it.
            self.refresh();
        }
    }

    fn git(&self) -> Option<Git> {
        match &self.st.borrow().git {
            GitState::Found(g) => Some(g.clone()),
            _ => None,
        }
    }

    fn get_git(&self) {
        if let Err(e) = (self.svc.open_url)(GET_GIT) {
            self.toast(&e, ToastKind::Error, None);
        }
    }

    /// About's website link, through the same `open_url` as git's download page.
    fn open_site(&self) {
        if let Err(e) = (self.svc.open_url)(crate::check::SITE) {
            self.toast(&e, ToastKind::Error, None);
        }
    }

    fn choose_git(&self) {
        let post = self.poster();
        (self.svc.pick_git)(Box::new(move |p| post(Msg::GitPicked(p))));
        if self.svc.manual {
            self.drain();
        }
    }

    fn git_picked(&self, p: Option<PathBuf>) {
        let Some(p) = p else { return };
        if !gitexe::named_git_exe(&p) || !p.is_absolute() {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.toast(&format!("Choose a file named git.exe; {} is not one.", clean(&name)), ToastKind::Error, None);
            return;
        }
        self.find_git(Some(p));
    }

    // ---------- roots ----------

    /// The roots as paths (a portable copy's `?:\` ones on its drive now).
    fn roots(&self) -> Vec<PathBuf> {
        let st = self.st.borrow();
        let kept = st.prefs.roots.clone().unwrap_or_default();
        kept.iter().filter_map(|k| if self.svc.mode.portable() { crate::portable::resolve_root(k, self.svc.exe_drive) } else { Some(PathBuf::from(k)) }).collect()
    }

    /// A first start's look: the roots found are kept; none found keeps nothing (the window asks
    /// for a folder, and the next start looks again).
    fn first_roots(&self, v: Vec<PathBuf>) {
        let kept: Vec<String> = v.iter().filter_map(|p| p.to_str().map(|s| self.keep_form(s))).collect();
        if kept.is_empty() {
            self.st.borrow_mut().prefs.roots = Some(Vec::new());
            self.refresh();
            return;
        }
        let k2 = kept.clone();
        self.save_prefs(move |p| {
            if p.roots.is_none() {
                p.roots = Some(k2.clone());
            }
        });
        self.refresh();
    }

    fn keep_form(&self, root: &str) -> String {
        if self.svc.mode.portable() { crate::portable::keep_root(root, self.svc.exe_drive) } else { root.to_string() }
    }

    fn open_roots(&self) {
        self.st.borrow_mut().roots_changed = false;
        if let Some(ui) = self.ui() {
            ui.set_roots_open(true);
        }
        self.render();
    }

    fn add_root(&self) {
        let start = self.roots().into_iter().next();
        let post = self.poster();
        (self.svc.pick_folder)(start, Box::new(move |p| post(Msg::RootPicked(p))));
        if self.svc.manual {
            self.drain();
        }
    }

    fn root_picked(&self, p: Option<PathBuf>) {
        let Some(p) = p else { return };
        let p = crate::walk::normalize_root(&p);
        let Some(text) = p.to_str().map(str::to_string) else {
            self.toast("That folder's name cannot be kept in the settings (it is not valid Unicode).", ToastKind::Error, None);
            return;
        };
        let kept = self.keep_form(&text);
        let dup = self.st.borrow().prefs.roots.as_ref().is_some_and(|r| r.iter().any(|x| x.eq_ignore_ascii_case(&kept)));
        if dup {
            self.toast(&format!("{} is a root already.", clean(&text)), ToastKind::Info, None);
            return;
        }
        let k = kept.clone();
        self.save_prefs(move |p| p.roots.get_or_insert_with(Vec::new).push(k.clone()));
        self.st.borrow_mut().roots_changed = true;
        // Added from the empty table (no Roots dialog open): look at once.
        if !self.ui().is_some_and(|u| u.get_roots_open()) {
            self.refresh();
        }
    }

    fn remove_root(&self, i: usize) {
        let gone = self.st.borrow().prefs.roots.as_ref().and_then(|r| r.get(i).cloned());
        let Some(gone) = gone else { return };
        self.save_prefs(move |p| {
            if let Some(r) = p.roots.as_mut() {
                r.retain(|x| x != &gone);
            }
        });
        self.st.borrow_mut().roots_changed = true;
        self.render();
    }

    fn close_roots(&self) {
        if let Some(ui) = self.ui() {
            ui.set_roots_open(false);
        }
        if std::mem::take(&mut self.st.borrow_mut().roots_changed) {
            self.refresh();
        }
        self.render();
    }

    // ---------- the scan ----------

    /// Rescan the roots and read every repo: a new epoch; answers of the old one do nothing.
    pub fn refresh(&self) {
        {
            let mut st = self.st.borrow_mut();
            if st.git == GitState::Looking {
                return; // the first refresh comes when git is known
            }
            if st.fetch.is_some() {
                st.refresh_after_fetch = true;
                return;
            }
            st.scan_cancel.store(true, Ordering::SeqCst);
            st.scan_cancel = Arc::new(AtomicBool::new(false));
            st.seq += 1;
            st.scanning = true;
            st.progress = None;
            st.refreshed_at = Some(self.now());
            let keep = st.selected.and_then(|i| st.rows.get(i)).map(|r| r.found.path.clone());
            let old = st.keep.take();
            st.keep = keep.or(old);
        }
        let roots = self.roots();
        let (seq, cancel) = {
            let st = self.st.borrow();
            (st.seq, st.scan_cancel.clone())
        };
        if roots.is_empty() {
            let mut st = self.st.borrow_mut();
            st.scanning = false;
            st.rows.clear();
            st.walked = None;
            st.selected = None;
            drop(st);
            self.render();
            return;
        }
        let backend = self.svc.backend.clone();
        self.render();
        self.bg(move |send| {
            let w = backend.walk(&roots, &cancel, &|p| send(Msg::Progress(seq, p.clone())));
            send(Msg::Walked(seq, w));
        });
    }

    fn walked(&self, seq: u64, w: Walked) {
        let jobs: Vec<(usize, Found)> = {
            let mut st = self.st.borrow_mut();
            if seq != st.seq {
                return;
            }
            st.scanning = false;
            st.rows = w.found.iter().cloned().map(Row::new).collect();
            st.walked = Some(Walked { found: Vec::new(), ..w });
            st.read_at = None;
            let keep = st.keep.take();
            let sel = keep.and_then(|k| st.rows.iter().position(|r| r.found.path == k));
            st.selected = sel;
            let readable = matches!(st.git, GitState::Found(_));
            let jobs: Vec<(usize, Found)> = if readable { st.rows.iter().enumerate().filter(|(_, r)| r.found.kind.readable()).map(|(i, r)| (i, r.found.clone())).collect() } else { Vec::new() };
            // Rows git cannot be asked about are read already: they say why.
            st.reading = jobs.len();
            if st.reading == 0 {
                st.read_at = Some(self.now());
            }
            jobs
        };
        let Some(git) = self.git() else { return };
        let (seq, cancel) = {
            let st = self.st.borrow();
            (st.seq, st.scan_cancel.clone())
        };
        let work: Vec<Work> = jobs
            .into_iter()
            .map(|(i, f)| {
                let (backend, git, cancel) = (self.svc.backend.clone(), git.clone(), cancel.clone());
                Box::new(move |send: &dyn Fn(Msg)| {
                    if cancel.load(Ordering::SeqCst) {
                        return; // a newer scan: its own reads answer
                    }
                    send(Msg::Read(seq, i, Box::new(backend.read(&git, &f))));
                }) as Box<dyn FnOnce(&dyn Fn(Msg)) + Send>
            })
            .collect();
        self.pool(work);
    }

    /// Back in the window: a refresh when the last one is a minute old or more.
    pub fn window_activated(&self) {
        let due = {
            let st = self.st.borrow();
            let dialog = self.ui().is_some_and(|u| u.get_about_open() || u.get_roots_open());
            !st.scanning && st.fetch.is_none() && !dialog && st.refreshed_at.is_some_and(|t| self.now() - t >= STALE_SECS)
        };
        if due {
            self.refresh();
        }
    }

    // ---------- fetch ----------

    fn can_fetch_all(&self) -> bool {
        let st = self.st.borrow();
        matches!(st.git, GitState::Found(_)) && !st.scanning && st.reading == 0 && st.fetch.is_none() && st.rows.iter().any(|r| r.has_remote())
    }

    fn fetch_all(&self) {
        if !self.can_fetch_all() {
            return;
        }
        let all: Vec<usize> = self.st.borrow().order.clone();
        self.start_fetch(all);
    }

    /// The row's reason not to be fetched (None: it can be).
    fn fetch_skip(&self, r: &Row) -> Option<String> {
        let info = r.info.as_ref()?;
        crate::fetch::skip_reason(r.has_remote(), &info.config.fetch_commands, info.config.problem.as_deref(), info.dubious.is_some(), self.git().is_some())
    }

    /// Fetch the repos among `rows` that have a remote, once per common git folder, four at a
    /// time.
    fn start_fetch(&self, rows: Vec<usize>) {
        let Some(git) = self.git() else { return };
        let (id, seq, stop, jobs) = {
            let mut groups: Vec<(String, usize, Vec<usize>)> = Vec::new();
            let mut skipped = 0;
            {
                let mut st = self.st.borrow_mut();
                for r in st.rows.iter_mut() {
                    r.fetch = FetchState::Idle;
                }
            }
            for &i in &rows {
                let (r, has_info) = {
                    let st = self.st.borrow();
                    (st.rows[i].clone(), st.rows[i].info.is_some())
                };
                if !has_info || !r.found.kind.readable() {
                    continue;
                }
                if let Some(why) = self.fetch_skip(&r) {
                    if r.has_remote() {
                        skipped += 1;
                    }
                    self.st.borrow_mut().rows[i].fetch = FetchState::Skipped(why);
                    continue;
                }
                let k = r.found.common.as_ref().unwrap_or(&r.found.path).to_string_lossy().to_lowercase();
                match groups.iter_mut().find(|g| g.0 == k) {
                    Some(g) => g.2.push(i),
                    None => groups.push((k, i, vec![i])),
                }
            }
            let mut st = self.st.borrow_mut();
            st.fetch_seq += 1;
            let id = st.fetch_seq;
            let stop = Arc::new(AtomicBool::new(false));
            let mut map = HashMap::new();
            let mut jobs = Vec::new();
            for (_, lead, members) in groups {
                for &m in &members {
                    st.rows[m].fetch = FetchState::Queued;
                }
                let found: Vec<(usize, Found)> = members.iter().map(|&m| (m, st.rows[m].found.clone())).collect();
                jobs.push((lead, st.rows[lead].found.clone(), found));
                map.insert(lead, members);
            }
            if jobs.is_empty() {
                drop(st);
                self.toast(if skipped > 0 { "Nothing to fetch: every repo with a remote is skipped (see each one)." } else { "Nothing to fetch: no repo has a remote." }, ToastKind::Info, None);
                self.render();
                return;
            }
            st.fetch = Some(FetchRun { id, total: jobs.len(), started: 0, ok: 0, failed: 0, not_started: 0, skipped, late_skips: 0, stop: stop.clone(), groups: map });
            (id, st.seq, stop, jobs)
        };
        let work: Vec<Work> = jobs
            .into_iter()
            .map(|(lead, found, members)| {
                let (backend, git, stop) = (self.svc.backend.clone(), git.clone(), stop.clone());
                Box::new(move |send: &dyn Fn(Msg)| {
                    if stop.load(Ordering::SeqCst) {
                        send(Msg::FetchNotStarted(id, lead));
                        return;
                    }
                    send(Msg::FetchStarted(id, lead));
                    let o = backend.fetch(&git, &found);
                    // Read again at once (every worktree sharing that git folder too).
                    let reads: Vec<(usize, Info)> = members.iter().map(|(i, f)| (*i, backend.read(&git, f))).collect();
                    for (i, info) in reads {
                        send(Msg::Read(seq, i, Box::new(info)));
                    }
                    send(Msg::FetchDone(id, lead, o));
                }) as Box<dyn FnOnce(&dyn Fn(Msg)) + Send>
            })
            .collect();
        self.render();
        self.pool(work);
    }

    fn fetch_one(&self) {
        let sel = self.st.borrow().selected;
        let Some(i) = sel else { return };
        if self.st.borrow().fetch.is_some() {
            return;
        }
        self.start_fetch(vec![i]);
    }

    fn stop_fetch(&self) {
        if let Some(f) = self.st.borrow().fetch.as_ref() {
            f.stop.store(true, Ordering::SeqCst);
        }
        self.render();
    }

    /// Every row of a fetch group (`lead`'s) gets `state`, for the current run only.
    fn fetch_state(&self, id: u64, lead: usize, state: FetchState) {
        let mut st = self.st.borrow_mut();
        let Some(f) = st.fetch.as_mut() else { return };
        if f.id != id {
            return;
        }
        if state == FetchState::Fetching {
            f.started += 1;
        }
        let members = f.groups.get(&lead).cloned().unwrap_or_default();
        for m in members {
            if let Some(r) = st.rows.get_mut(m) {
                r.fetch = state.clone();
            }
        }
    }

    fn fetch_done(&self, id: u64, lead: usize, o: Outcome) {
        if !self.st.borrow().fetch.as_ref().is_some_and(|f| f.id == id) {
            return;
        }
        let state = match &o {
            Outcome::Fetched => FetchState::Fetched,
            Outcome::Failed(f) => FetchState::Failed(f.clone()),
            Outcome::Skipped(w) => FetchState::Skipped(w.clone()),
        };
        self.fetch_state(id, lead, state);
        if let Some(f) = self.st.borrow_mut().fetch.as_mut() {
            match o {
                Outcome::Fetched => f.ok += 1,
                Outcome::Failed(_) => f.failed += 1,
                // Its config changed since the read: not fetched after all.
                Outcome::Skipped(_) => {
                    f.not_started += 1;
                    f.late_skips += 1;
                }
            }
        }
        self.maybe_end_fetch();
    }

    /// The run ends when every group answered, however each one ended.
    fn maybe_end_fetch(&self) {
        let end = self.st.borrow().fetch.as_ref().is_some_and(|f| f.done() >= f.total);
        if !end {
            return;
        }
        let Some(f) = self.st.borrow_mut().fetch.take() else { return };
        let stopped = f.stop.load(Ordering::SeqCst) && f.not_started > f.late_skips;
        let of = f.total - f.late_skips.min(f.total);
        let mut text = if stopped { format!("Stopped: fetched {} of {of}", f.ok) } else { format!("Fetched {} of {of}", f.ok) };
        if f.failed > 0 {
            text.push_str(&format!(": {} failed", f.failed));
        }
        let skipped = f.skipped + f.late_skips;
        if skipped > 0 {
            text.push_str(&format!(" · {skipped} skipped"));
        }
        self.toast(&text, if f.failed > 0 { ToastKind::Error } else { ToastKind::Ok }, None);
        if std::mem::take(&mut self.st.borrow_mut().refresh_after_fetch) {
            self.refresh();
        }
    }

    // ---------- the view ----------

    fn view(&self) -> View {
        let st = self.st.borrow();
        let git_word = match st.git {
            GitState::TooOld(_) => "git too old",
            GitState::Broken { .. } => "git not usable",
            _ => "git not found",
        };
        View { now: self.now(), login: st.login.clone(), git_missing: !matches!(st.git, GitState::Found(_) | GitState::Looking), git_word }
    }

    fn filter(&self) -> Filter {
        self.st.borrow().prefs.filter()
    }

    fn sort_key(&self) -> Sort {
        self.st.borrow().prefs.sort()
    }

    fn chip(&self, i: i32) {
        let Some(f) = Filter::ALL.get(i.max(0) as usize).copied() else { return };
        self.save_prefs(move |p| p.filter = Some(f.key().into()));
        self.render();
    }

    fn sort(&self, k: i32) {
        let s = match k {
            1 => Sort::LastCommit,
            2 => Sort::Fetched,
            _ => Sort::Path,
        };
        self.save_prefs(move |p| p.sort = Some(s.key().into()));
        self.render();
        self.scroll_to_selected();
    }

    fn name_edited(&self, t: &str) {
        self.st.borrow_mut().name = t.to_string();
        self.render();
    }

    /// Escape in the name field: clears it (the filter goes); an empty field lets Escape on.
    fn name_escaped(&self) -> bool {
        if self.st.borrow().name.is_empty() {
            return false;
        }
        self.st.borrow_mut().name.clear();
        if let Some(ui) = self.ui() {
            ui.set_name_text(s(""));
        }
        self.render();
        true
    }

    fn row_state(r: &Row, v: &View) -> i32 {
        if r.info.is_none() && r.found.kind.readable() && !v.git_missing || matches!(r.fetch, FetchState::Queued | FetchState::Fetching) {
            return 0;
        }
        if r.error(v).is_some() || matches!(r.fetch, FetchState::Failed(_)) {
            return 4;
        }
        let info = r.info.as_ref();
        let op = info.is_some_and(|i| matches!(i.op, Ok(Some(_))));
        let st = info.and_then(|i| i.status.as_ref().ok());
        if op || st.is_some_and(|s| s.conflicts > 0 || s.upstream_gone()) {
            return 3;
        }
        if r.dirty() || r.unpushed() || r.behind() > 0 {
            return 2;
        }
        1
    }

    /// Everything the window shows, from the state (one pass per batch of answers).
    pub fn render(&self) {
        let Some(ui) = self.ui() else { return };
        let v = self.view();
        let (filter, sort) = (self.filter(), self.sort_key());
        let mut st = self.st.borrow_mut();
        st.order = model::order(&st.rows, sort);
        st.visible = model::visible(&st.rows, &st.order, filter, &st.name, &v);
        // The selection stays only while its row is shown.
        if st.selected.is_some_and(|i| !st.visible.contains(&i)) {
            st.selected = None;
        }
        let rows: Vec<RepoRow> = st
            .visible
            .iter()
            .map(|&i| {
                let r = &st.rows[i];
                let c = model::cells(r, &v);
                let fetch = r.fetch.word().unwrap_or_default();
                let error = if !c.error.is_empty() {
                    c.error.clone()
                } else if let FetchState::Failed(f) = &r.fetch {
                    format!("fetch failed: {}", f.short)
                } else {
                    String::new()
                };
                let path_tip = {
                    let rel = crate::text::shown_path(&r.found.rel);
                    if rel.is_empty() { crate::text::shown_path(&r.found.path) } else { rel }
                };
                let mut a = model::a11y(&c);
                if !fetch.is_empty() {
                    a = format!("{a} · {fetch}");
                }
                RepoRow {
                    id: i as i32,
                    state: Self::row_state(r, &v),
                    repo: s(&c.repo),
                    tag: s(&c.tag),
                    path_tip: s(path_tip),
                    op: s(&c.op),
                    branch: s(&c.branch),
                    changes: s(&c.changes),
                    conflicts: c.conflicts,
                    sync: s(&c.sync),
                    sync_tip: s(model::sync_tip(r, &v)),
                    stash: s(&c.stash),
                    remote: s(&c.remote),
                    hint: s(&c.hint),
                    last: s(&c.last),
                    last_tip: s(&c.last_tip),
                    fetched: s(&c.fetched),
                    error: s(error),
                    fetch: s(fetch),
                    selected: st.selected == Some(i),
                    a11y: s(a),
                }
            })
            .collect();
        if self.rows_model.row_count() == rows.len() {
            for (i, r) in rows.into_iter().enumerate() {
                if self.rows_model.row_data(i).as_ref() != Some(&r) {
                    self.rows_model.set_row_data(i, r);
                }
            }
        } else {
            self.rows_model.set_vec(rows);
        }
        // The chips and their counts.
        let counts = model::chip_counts(&st.rows);
        let chips: Vec<ChipItem> = Filter::ALL.iter().enumerate().map(|(i, f)| ChipItem { label: s(f.word()), count: s(thousands(counts[i] as u64)), active: *f == filter }).collect();
        ui.set_chips(ModelRc::new(VecModel::from(chips)));
        ui.set_sort_key(match sort {
            Sort::Path => 0,
            Sort::LastCommit => 1,
            Sort::Fetched => 2,
        });
        // The empty table.
        let roots = st.prefs.roots.clone().unwrap_or_default();
        let (note, action) = if roots.is_empty() && st.git != GitState::Looking && st.started {
            ("No folders to look in. Add the folder that holds your projects.".to_string(), "Add folder…")
        } else if st.rows.is_empty() && (st.scanning || st.git == GitState::Looking || !st.started) {
            ("Looking for repos…".into(), "")
        } else if st.rows.is_empty() {
            (format!("No git repos under {}.", roots.iter().map(|r| clean(r)).collect::<Vec<_>>().join(", ")), "Add folder…")
        } else if !st.name.trim().is_empty() {
            (format!("No repo's name holds \u{201C}{}\u{201D}{}.", clean(st.name.trim()), if filter == Filter::All { String::new() } else { format!(" among {}", filter.word().to_lowercase()) }), "")
        } else {
            (format!("No repo is {} now.", filter.word().to_lowercase()), "")
        };
        ui.set_table_note(s(note));
        ui.set_table_action(s(action));
        // The bars.
        ui.set_scanning(st.scanning);
        ui.set_fetching(st.fetch.is_some());
        ui.set_stopping(st.fetch.as_ref().is_some_and(|f| f.stop.load(Ordering::SeqCst)));
        let can_fetch = matches!(st.git, GitState::Found(_)) && !st.scanning && st.reading == 0 && st.fetch.is_none() && st.rows.iter().any(|r| r.has_remote());
        ui.set_can_fetch(can_fetch);
        let portable = self.svc.mode.portable();
        ui.set_portable(portable);
        let data = self.svc.mode.dir().map(|d| crate::text::shown_path(&d.join("VibeRepos-data"))).unwrap_or_default();
        ui.set_portable_tip(s(match &self.svc.mode {
            Mode::Portable { .. } => format!("Settings are kept in {data}. {PORTABLE_TRACE}"),
            Mode::ReadOnly { .. } => format!("{data} cannot be written: settings are kept until VibeRepos closes. {PORTABLE_TRACE}"),
            Mode::Installed => String::new(),
        }));
        // The REMOTE column's tip. No public/private word: nothing VibeRepos reads tells it, and the
        // old "-pub in the name" rule was one user's convention (copy-ux M9).
        ui.set_hint_line(s("The host of each repo's main remote."));
        // Git.
        let (banner, right, tip) = match &st.git {
            GitState::Looking => (String::new(), "looking for git…".to_string(), String::new()),
            GitState::Found(g) => {
                let short = g.version.split(".windows").next().unwrap_or(&g.version).to_string();
                (String::new(), format!("git {short}"), format!("git {} at {}", g.version, crate::text::shown_path(&g.exe)))
            }
            GitState::Missing => {
                let b = "Git is not installed (looked in PATH and the usual folders). VibeRepos reads repos through git and does not bring its own.".to_string();
                (b.clone(), "git not found".into(), b)
            }
            GitState::TooOld(g) => {
                let b = format!("VibeRepos needs Git 2.36 or newer; found {} at {}.", g.parsed.0.to_string() + "." + &g.parsed.1.to_string() + "." + &g.parsed.2.to_string(), crate::text::shown_path(&g.exe));
                (b.clone(), format!("git {} (too old)", g.parsed.0.to_string() + "." + &g.parsed.1.to_string()), b)
            }
            GitState::Broken { exe, why } => {
                let b = format!("The git at {} did not answer as git: {}.", crate::text::shown_path(exe), clean(why));
                (b.clone(), "git not usable".into(), b)
            }
        };
        ui.set_banner(s(banner));
        ui.set_status_right(s(right));
        // Which git, and where (it was About's git line too).
        ui.set_status_tip(s(tip));
        // The status line.
        let mut warn = false;
        let left = if st.scanning {
            match &st.progress {
                Some(p) => format!("Scanning {} · {} · {}", clean(&p.root), count(p.folders as usize, "folder", "folders"), count(p.repos, "repo", "repos")),
                None => "Scanning…".into(),
            }
        } else if let Some(f) = &st.fetch {
            format!("Fetching {} of {} · {} ok · {} failed{}", f.started.min(f.total), f.total, f.ok, f.failed, if f.stop.load(Ordering::SeqCst) { " · stopping" } else { "" })
        } else if st.reading > 0 {
            let total = st.rows.iter().filter(|r| r.found.kind.readable()).count();
            format!("Reading {} of {}", thousands((total - st.reading.min(total)) as u64), count(total, "repo", "repos"))
        } else if st.rows.is_empty() {
            String::new()
        } else {
            let mut t = model::summary(&st.rows);
            if let Some(at) = st.read_at {
                t.push_str(&format!(" · read {}", (self.svc.clock)(at).0));
            }
            if let Some(w) = &st.walked {
                if w.links > 0 {
                    t.push_str(&format!(" · {} not followed", count(w.links as usize, "link", "links")));
                }
                if let Some(p) = w.partial() {
                    warn = true;
                    t.push_str(&format!(" · {p}"));
                }
            }
            t
        };
        let left = if portable && !left.is_empty() { format!("Portable · {left}") } else { left };
        ui.set_status_left(s(left));
        ui.set_status_warn(warn);
        // The roots dialog's list (notes from what is known: no disk read here).
        let failed: Vec<usize> = st.walked.as_ref().map(|w| w.root_errors.iter().map(|e| e.0).collect()).unwrap_or_default();
        let items: Vec<RootItem> = roots
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let shown = if self.svc.mode.portable() { crate::portable::resolve_root(k, self.svc.exe_drive).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| k.clone()) } else { k.clone() };
                let note = if failed.contains(&i) {
                    "could not be read at the last scan".to_string()
                } else if k.starts_with("?:\\") {
                    "on this stick (follows its letter)".into()
                } else {
                    String::new()
                };
                RootItem { path: s(clean(&shown)), note: s(note) }
            })
            .collect();
        ui.set_roots(ModelRc::new(VecModel::from(items)));
        // The detail panel.
        match st.selected.map(|i| (i, &st.rows[i])) {
            Some((_, r)) => {
                let c = model::cells(r, &v);
                ui.set_detail_shown(true);
                ui.set_detail_title(s(&c.repo));
                ui.set_detail_tag(s(&c.tag));
                let lines: Vec<DetailLine> = model::detail(r, &v).into_iter().map(|l| DetailLine { key: s(l.key), value: s(l.value), mono: l.mono, section: l.section, warn: l.warn }).collect();
                ui.set_detail_lines(ModelRc::new(VecModel::from(lines)));
                let why = if st.fetch.is_some() {
                    Some("A fetch is running".to_string())
                } else if !matches!(st.git, GitState::Found(_)) {
                    Some("git not found".into())
                } else if let Some(info) = &r.info {
                    crate::fetch::skip_reason(r.has_remote(), &info.config.fetch_commands, info.config.problem.as_deref(), info.dubious.is_some(), true)
                } else {
                    Some("Still reading".into())
                };
                ui.set_detail_can_fetch(why.is_none());
                ui.set_detail_fetch_tip(s(why.map(|w| format!("Not fetched: {w}")).unwrap_or_else(|| "git fetch this repo (refs only)".into())));
                ui.set_detail_command(s(r.info.as_ref().and_then(|i| i.dubious.as_ref()).map(|d| d.command.clone()).unwrap_or_default()));
            }
            None => ui.set_detail_shown(false),
        }
    }

    // ---------- selection and keys ----------

    fn select_row(&self, i: usize, scroll: bool) {
        {
            let mut st = self.st.borrow_mut();
            if i >= st.rows.len() {
                return;
            }
            st.selected = Some(i);
        }
        self.render();
        if scroll {
            self.scroll_to_selected();
        }
    }

    fn scroll_to_selected(&self) {
        let Some(ui) = self.ui() else { return };
        let st = self.st.borrow();
        if let Some(pos) = st.selected.and_then(|i| st.visible.iter().position(|&v| v == i)) {
            ui.set_scroll_to(pos as i32);
            ui.set_scroll_seq(ui.get_scroll_seq() + 1);
        }
    }

    fn move_selection(&self, to: impl Fn(Option<usize>, usize) -> usize) {
        let (vis, cur) = {
            let st = self.st.borrow();
            (st.visible.clone(), st.selected.and_then(|i| st.visible.iter().position(|&v| v == i)))
        };
        if vis.is_empty() {
            return;
        }
        let pos = to(cur, vis.len()).min(vis.len() - 1);
        self.select_row(vis[pos], true);
    }

    /// A key nothing focused took. True when it was used.
    fn key(&self, t: &str, ctrl: bool, _shift: bool) -> bool {
        let k = |x: Key| t == key(x).as_str();
        if ctrl {
            return match t.to_ascii_lowercase().as_str() {
                "c" => {
                    self.copy_status();
                    true
                }
                "f" => {
                    if let Some(ui) = self.ui() {
                        ui.invoke_focus_name();
                    }
                    true
                }
                _ => false,
            };
        }
        if k(Key::F5) {
            if !self.st.borrow().scanning {
                self.refresh();
            }
            return true;
        }
        if k(Key::DownArrow) {
            self.move_selection(|c, _| c.map_or(0, |c| c + 1));
        } else if k(Key::UpArrow) {
            self.move_selection(|c, _| c.map_or(0, |c| c.saturating_sub(1)));
        } else if k(Key::Home) {
            self.move_selection(|_, _| 0);
        } else if k(Key::End) {
            self.move_selection(|_, n| n - 1);
        } else if k(Key::PageDown) {
            self.move_selection(|c, _| c.map_or(0, |c| c + 15));
        } else if k(Key::PageUp) {
            self.move_selection(|c, _| c.map_or(0, |c| c.saturating_sub(15)));
        } else if k(Key::Return) {
            if self.st.borrow().selected.is_none() {
                return false;
            }
            self.open_folder();
        } else {
            return false;
        }
        true
    }

    // ---------- actions on the selected repo ----------

    fn selected_row(&self) -> Option<Row> {
        let st = self.st.borrow();
        st.selected.and_then(|i| st.rows.get(i).cloned())
    }

    fn open_folder(&self) {
        let Some(r) = self.selected_row() else { return };
        if let Err(e) = (self.svc.open_folder)(&r.found.path) {
            self.toast(&clean(&e), ToastKind::Error, None);
        }
    }

    fn open_terminal(&self) {
        let Some(r) = self.selected_row() else { return };
        if let Err(e) = (self.svc.open_terminal)(&r.found.path) {
            self.toast(&clean(&e), ToastKind::Error, None);
        }
    }

    fn copy_path(&self) -> bool {
        let Some(r) = self.selected_row() else { return false };
        (self.svc.copy_path)(&r.found.path)
    }

    fn copy_command(&self) -> bool {
        let Some(c) = self.selected_row().and_then(|r| r.info).and_then(|i| i.dubious).map(|d| d.command) else { return false };
        (self.svc.copy)(&c)
    }

    fn copy_status(&self) {
        let Some(r) = self.selected_row() else { return };
        if !(self.svc.copy)(&crate::export::row_text(&r, &self.view())) {
            self.toast("Could not copy: the clipboard is busy.", ToastKind::Error, None);
        }
    }

    fn shown_rows(&self) -> Vec<Row> {
        let st = self.st.borrow();
        st.visible.iter().map(|&i| st.rows[i].clone()).collect()
    }

    fn copy_all(&self) -> bool {
        let rows = self.shown_rows();
        if rows.is_empty() {
            return false;
        }
        let refs: Vec<&Row> = rows.iter().collect();
        let header = {
            let st = self.st.borrow();
            let git = match &st.git {
                GitState::Found(g) => format!("git {}", g.version.split(".windows").next().unwrap_or(&g.version)),
                _ => "git not found".into(),
            };
            let mut h = format!("VibeRepos {} · {} · {} · filter: {}", crate::VERSION, (self.svc.clock)(self.now()).0.get(..5).unwrap_or(""), git, st.prefs.filter().word());
            if !st.name.trim().is_empty() {
                h.push_str(&format!(" · name: {}", clean(st.name.trim())));
            }
            h
        };
        (self.svc.copy)(&crate::export::table(&refs, &self.view(), &header))
    }

    fn save_csv(&self) {
        if self.shown_rows().is_empty() {
            return;
        }
        let name = format!("VibeRepos {}.csv", (self.svc.clock)(self.now()).1);
        let post = self.poster();
        (self.svc.save_csv)(name, Box::new(move |p| post(Msg::SaveTo(p))));
        if self.svc.manual {
            self.drain();
        }
    }

    /// Write the CSV of the rows shown when Save As was answered (on a worker: the target may
    /// be a share that does not answer).
    fn save_to(&self, p: Option<PathBuf>) {
        let Some(p) = p else { return };
        let rows = self.shown_rows();
        let refs: Vec<&Row> = rows.iter().collect();
        let body = crate::export::csv(&refs, &self.view(), &(self.svc.csv_format)(), &*self.svc.iso);
        self.bg(move |send| {
            let r = crate::prefs::write_atomic(&p, body.as_bytes()).map(|_| p.clone()).map_err(|e| crate::walk::error_words(&e));
            send(Msg::Saved(r));
        });
    }

    // ---------- prefs ----------

    /// The prefs as this window has them (its own choices, in memory).
    pub fn prefs(&self) -> Prefs {
        self.st.borrow().prefs.clone()
    }

    /// One change to the prefs: at once in memory, and by the prefs thread onto the newest
    /// saved ones. A file found garbled on the way is told shortly after.
    pub fn save_prefs(&self, change: impl Fn(&mut Prefs) + Send + Sync + 'static) {
        let f: PrefsChange = Arc::new(change);
        f(&mut self.st.borrow_mut().prefs);
        self.prefs_pending.fetch_add(1, Ordering::SeqCst);
        if self.prefs_tx.send(f).is_err() {
            self.prefs_pending.fetch_sub(1, Ordering::SeqCst);
        }
        let w = self.me.borrow().clone();
        Timer::single_shot(Duration::from_millis(PREFS_TELL_MS), move || {
            if let Some(a) = w.upgrade() {
                a.tell_garbled();
            }
        });
    }

    fn tell_garbled(&self) {
        if self.store.take_garbled() {
            self.toast("prefs.json was damaged: it is kept as prefs.json.broken, and this window's settings are saved in its place.", ToastKind::Error, None);
        }
    }

    /// At exit (and in tests): let the prefs thread hand its last change to the store (at most
    /// two seconds), then write what is unwritten, on a thread of its own, waited for at most two
    /// seconds too: a prefs file on a share that stopped answering never holds the window's
    /// thread for good (VibePing PING-8, review X15CON-4). A prefs thread still busy then is not
    /// waited for: at exit the process ends it, and the changes it had not written are lost,
    /// unsaid.
    pub fn finish_prefs(&self) {
        let start = Instant::now();
        let limit = Duration::from_secs(2);
        while self.prefs_pending.load(Ordering::SeqCst) > 0 && start.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(2));
        }
        if self.prefs_pending.load(Ordering::SeqCst) > 0 {
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = channel::<()>();
        std::thread::spawn(move || {
            store.flush();
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(limit);
    }

    /// At exit: no more reads or fetches start (running ones end with their git), then the prefs.
    pub fn finish(&self) {
        {
            let st = self.st.borrow();
            st.scan_cancel.store(true, Ordering::SeqCst);
            if let Some(f) = &st.fetch {
                f.stop.store(true, Ordering::SeqCst);
            }
        }
        self.finish_prefs();
    }

    fn toggle_theme(&self) {
        let Some(ui) = self.ui() else { return };
        let theme = ui.global::<Theme>();
        let dark = !theme.get_dark();
        theme.set_dark(dark);
        self.save_prefs(move |p| p.theme = Some(if dark { "dark" } else { "light" }.into()));
    }

    // ---------- About's install check ----------

    /// "Check this install": one at a time, on a worker (it reads the files and may go online);
    /// the window looks for the answer every CHECK_POLL_MS meanwhile and goes on. Closing About
    /// leaves it running: the answer lands in the rows, which About shows when it opens again.
    fn check_install(&self) {
        let Some(ui) = self.ui() else { return };
        let seq = {
            let mut st = self.st.borrow_mut();
            if st.check_rx.is_some() {
                return;
            }
            st.check_seq += 1;
            st.check_seq
        };
        let (tx, rx) = channel();
        self.st.borrow_mut().check_rx = Some(rx);
        ui.set_checking(true);
        let (inspect, fetch, release) = (self.svc.inspect.clone(), self.svc.fetch.clone(), self.svc.release.clone());
        let installed = !self.svc.mode.portable();
        std::thread::spawn(move || {
            let local = inspect(&installed);
            // The exe updated or replaced under this one while it ran: nothing to compare (FED3).
            let rows = if local.replaced {
                crate::check::replaced_rows(&release, &local)
            } else {
                let fetched = fetch_within(fetch, crate::check::list_path(release.app), release.wait);
                crate::check::rows(&release, &local, fetched)
            };
            let _ = tx.send((seq, rows));
        });
        let w = self.me.borrow().clone();
        self.check_timer.start(TimerMode::Repeated, Duration::from_millis(CHECK_POLL_MS), move || {
            if let Some(a) = w.upgrade() {
                a.poll_check();
            }
        });
    }

    /// The check's answer, once it is there: the rows, and the button back.
    fn poll_check(&self) {
        let got = {
            let st = self.st.borrow();
            let Some(rx) = st.check_rx.as_ref() else { return };
            match rx.try_recv() {
                Ok((seq, rows)) => Some((seq == st.check_seq).then_some(rows)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Some(Rows::stopped())),
            }
        };
        let Some(rows) = got else { return };
        self.check_timer.stop();
        self.st.borrow_mut().check_rx = None;
        if let Some(rows) = rows {
            self.show_rows(&rows);
        }
        if let Some(ui) = self.ui() {
            ui.set_checking(false);
            ui.set_checked(true);
        }
    }

    fn show_rows(&self, rows: &Rows) {
        let dot = |d: Dot| match d {
            Dot::Idle => CheckDot::Idle,
            Dot::Ok => CheckDot::Ok,
            Dot::Warn => CheckDot::Warn,
            Dot::Danger => CheckDot::Danger,
        };
        let all: Vec<CheckRow> = rows.all().iter().map(|(name, r)| CheckRow { name: s(name), text: s(&r.text), dot: dot(r.dot) }).collect();
        self.check_rows.set_vec(all);
    }

    // ---------- toasts ----------

    /// A toast, bottom right: errors stay 10 s, the rest 5 s. With `reveal` it offers "Show".
    pub fn toast(&self, text: &str, kind: ToastKind, reveal: Option<PathBuf>) {
        let id = {
            let mut st = self.st.borrow_mut();
            st.toast_seq += 1;
            let id = st.toast_seq;
            if let Some(p) = reveal.clone() {
                st.toast_paths.insert(id, p);
            }
            id
        };
        while self.toasts.row_count() >= 3 {
            let old = self.toasts.row_data(0).map(|t| t.id).unwrap_or(0);
            self.drop_toast(old);
        }
        self.toasts.push(ToastItem { id, text: s(clean_toast(text)), kind, action: if reveal.is_some() { s("Show") } else { s("") } });
        self.arm_toast(id, if kind == ToastKind::Error { 10_000 } else { 5_000 });
    }

    pub fn toast_texts(&self) -> Vec<String> {
        self.toasts.iter().map(|t| t.text.to_string()).collect()
    }

    /// A toast's timer, `ms` from now. Only its newest timer counts, and while a dialog is open
    /// it waits (`dialogs_closed` gives it its full time again).
    fn arm_toast(&self, id: i32, ms: u64) {
        let generation = {
            let mut st = self.st.borrow_mut();
            let t = st.toast_time.entry(id).or_insert((0, ms));
            t.0 += 1;
            t.0
        };
        let w = self.me.borrow().clone();
        Timer::single_shot(Duration::from_millis(ms), move || {
            let Some(a) = w.upgrade() else { return };
            let current = a.st.borrow().toast_time.get(&id).is_some_and(|t| t.0 == generation);
            if current && !a.ui().is_some_and(|ui| ui.get_about_open() || ui.get_roots_open()) {
                a.drop_toast(id);
            }
        });
    }

    fn dialogs_closed(&self) {
        let all: Vec<(i32, u64)> = self.st.borrow().toast_time.iter().map(|(id, t)| (*id, t.1)).collect();
        for (id, ms) in all {
            self.arm_toast(id, ms);
        }
    }

    fn drop_toast(&self, id: i32) {
        if let Some(i) = self.toasts.iter().position(|t| t.id == id) {
            self.toasts.remove(i);
        }
        self.st.borrow_mut().toast_time.remove(&id);
        self.st.borrow_mut().toast_paths.remove(&id);
    }

    fn toast_action(&self, id: i32) {
        let p = self.st.borrow().toast_paths.get(&id).cloned();
        if let Some(p) = p {
            (self.svc.reveal)(&p);
        }
        self.drop_toast(id);
    }

    // ---------- tests ----------

    /// Tests: the rows as Rust has them (all, in order).
    pub fn row_paths(&self) -> Vec<PathBuf> {
        let st = self.st.borrow();
        st.order.iter().map(|&i| st.rows[i].found.path.clone()).collect()
    }

    /// Tests: whether a fetch run is going.
    pub fn fetching(&self) -> bool {
        self.st.borrow().fetch.is_some()
    }

    /// Tests: the current scan's seq.
    pub fn seq(&self) -> u64 {
        self.st.borrow().seq
    }

    /// Tests: hand the window an answer as a worker would (an old epoch's included).
    pub fn inject(&self, m: Msg) {
        self.handle(m);
        self.render();
    }
}

/// An error toast from another thread (a launch that failed after its button returned).
pub fn toast_later(text: String) {
    let _ = slint::invoke_from_event_loop(move || {
        APP.with(|a| {
            if let Some(app) = a.borrow().upgrade() {
                app.toast(&text, ToastKind::Error, None);
            }
        })
    });
}

/// Toast text from outside is cleaned; a toast keeps up to 400 characters.
fn clean_toast(t: &str) -> String {
    crate::text::clean_max(t, 400)
}
