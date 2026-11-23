//! The tray app's process: one UI thread with a hidden window, one worker thread that owns the engine.
//!
//! The UI thread sleeps in `MsgWaitForMultipleObjectsEx` on window messages and three handles: a waitable timer set
//! to the next check, and change notifications on the app's folder (`config.json`, `state.json`) and on the skills
//! folder. Nothing polls. A change in the skills folder only leads to a read-only look, two seconds after the last
//! change; safety never depends on it, because every check compares the disk with the record first.

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FindCloseChangeNotification,
    FindFirstChangeNotificationW, FindNextChangeNotification,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    CancelWaitableTimer, CreateMutexW, CreateWaitableTimerW, GetCurrentThread, INFINITE, SetThreadPriority, SetWaitableTimer, Sleep,
    THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END,
};
use windows_sys::Win32::UI::Shell::{NIN_SELECT, NINF_KEY};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW,
    GetWindowThreadProcessId, IDCANCEL, IsChild, IsDialogMessageW, KillTimer, MB_ICONERROR, MB_OK, MF_SEPARATOR, MF_STRING, MSG, MWMO_INPUTAVAILABLE,
    MessageBoxW, MsgWaitForMultipleObjectsEx, PBT_APMRESUMEAUTOMATIC, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostMessageW, PostQuitMessage, QS_ALLINPUT,
    RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem, SetTimer, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP,
    WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENDSESSION, WM_NULL, WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_QUIT,
    WM_SETTINGCHANGE, WM_TIMECHANGE, WM_TIMER, WS_EX_TOOLWINDOW, WS_POPUP,
};

use super::page::{self, Note};
use super::{bench, dialogs, register_class, sentence, tray, trim_memory, window};
use crate::app::{App, Done, Job, Order, SettingsSource, Shown, SourceDraft, UpdateState, perform_with_updates};
use crate::autostart::BACKGROUND;
use crate::board::{APP_NAME, Level, SourceView, span};
use crate::cli::{Host, SystemHost};
use crate::config::{CONFIG_FILE, Config, Paths, instance_tag};
use crate::engine::{Engine, EngineError, Stamp, stamp_of};
use crate::interval::{Interval, UnixSecs, now_secs};
use crate::replacement::VerifiedTray;
use crate::sources::{GlobalSettings, SourceConfig};
use crate::win::{proc, wide, wide_os};

/// Tray menu and remote-control commands, sent as `WM_COMMAND`.
pub const CMD_SETTINGS: usize = 1001;
pub const CMD_CHECK: usize = 1002;
pub const CMD_QUIT: usize = 1003;
/// Debug builds only, for `scripts\relaunch-smoke.ps1`: restart as after an install, with the right or a wrong digest.
#[cfg(debug_assertions)]
pub const CMD_TEST_RESTART: usize = 1098;
#[cfg(debug_assertions)]
pub const CMD_TEST_RESTART_WRONG: usize = 1099;
/// The worker finished an order.
const WM_WORKER: u32 = WM_APP + 2;
/// Something changed: start the next job, set the timer, update the icon and the window.
const WM_KICK: u32 = WM_APP + 3;
const TIMER_LOOK: usize = 1;
/// A restart waits while a question, the tray menu or a sign-out is pending, and tries again this often.
const TIMER_RESTART: usize = 2;
const RESTART_RETRY_MS: u32 = 500;
/// A look at the skills folder waits until it has been quiet this long.
const LOOK_DELAY_MS: u32 = 2000;
const STARTUP_OFFER_FILE: &str = "startup-offer-seen";
/// How long quitting waits for the job that is running and for saving what is waiting to be saved.
const QUIT_WAIT: Duration = Duration::from_secs(2);
/// How a tray that installed an app update starts the new one, which takes over when this one has quit. Older builds
/// pass these to newer ones, so they never change meaning. `-open` opens the window, as it was open before.
const RESTARTED: &str = "--restarted";
const RESTARTED_OPEN: &str = "--restarted-open";
/// How long the new tray must keep running before the old one quits for it.
const RESTART_CHECK: Duration = Duration::from_secs(1);
/// How long a restarted tray waits for the one before it to quit. Then it goes quietly.
const RESTART_WAIT: Duration = Duration::from_secs(120);
const CLASS: &str = "TinySkillSyncTray";
const SKILL_CHANGES: u32 = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE;
const HOME_CHANGES: u32 = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE;
const FILETIME_UNIX_OFFSET: i64 = 11_644_473_600;

/// A change notification on a folder. It is signalled when something in the folder changes; nothing is read.
struct Watch {
    handle: HANDLE,
}

impl Watch {
    fn open(path: &Path, subtree: bool, filter: u32) -> Option<Watch> {
        let handle = unsafe { FindFirstChangeNotificationW(wide_os(path.as_os_str()).as_ptr(), subtree.into(), filter) };
        (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Watch { handle })
    }

    /// Waits for the next change. False when the folder is gone.
    fn rearm(&self) -> bool {
        unsafe { FindNextChangeNotification(self.handle) != 0 }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        unsafe { FindCloseChangeNotification(self.handle) };
    }
}

struct Shell {
    app: App,
    home: PathBuf,
    orders: Sender<(Order, Arc<AtomicBool>)>,
    done: Receiver<Done>,
    cancel: Arc<AtomicBool>,
    timer: HANDLE,
    home_watch: Option<Watch>,
    skills_watch: Option<Watch>,
    /// `config.json` as last read or written here.
    cfg_stamp: Option<Stamp>,
    /// `state.json` as last seen on disk.
    seen_state: Option<Stamp>,
    /// `config.json` cannot be read. Until it can, the settings in use stay as they were, and if the app has not
    /// started yet it does not start: it would not know which skills folder to look at.
    cfg_error: bool,
    /// Settings files beside the executable are read and written only by the worker.
    portable_mode: bool,
    saving_settings: bool,
    /// The first worker load decides whether this machine should open the window.
    initial_load: bool,
    background: bool,
    open_after_initial: bool,
    /// How the last thing the user asked for went, for the status line.
    feedback: Option<String>,
    /// When the worker was handed the job it runs.
    sent_at: Option<Instant>,
    /// The differences of a skill the worker read, until the window takes them.
    diff: Option<Shown>,
    finishing_install: bool,
    transactions: crate::updater::Transactions,
    quit_error: Option<String>,
    /// An app update this tray installed, for it to restart into once it is not busy with the user.
    restart: Option<Result<VerifiedTray, String>>,
    /// Started by the tray before it, which had its window open: open it again once settings are loaded.
    reopen: bool,
}

thread_local! {
    static SHELL: RefCell<Option<Shell>> = const { RefCell::new(None) };
    static MAIN: Cell<HWND> = const { Cell::new(null_mut()) };
    static KICK_POSTED: Cell<bool> = const { Cell::new(false) };
    static SHUT_DOWN: Cell<bool> = const { Cell::new(false) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    static MENU_OPEN: Cell<bool> = const { Cell::new(false) };
    /// Windows asked whether it may end the session, and has not said it changed its mind.
    static ENDING: Cell<bool> = const { Cell::new(false) };
    /// The new tray this one started and is quitting for. It waits for this process to end, so it can still be stopped.
    static HANDOFF: Cell<Option<proc::Started>> = const { Cell::new(None) };
}

fn with_shell<R>(f: impl FnOnce(&mut Shell) -> R) -> Option<R> {
    SHELL.with(|s| {
        let mut s = s.try_borrow_mut().ok()?;
        s.as_mut().map(f)
    })
}

pub fn main_window() -> HWND {
    MAIN.with(Cell::get)
}

/// How the tray was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// By the user: opens the window of a copy that is already running.
    Normal,
    /// By Windows at sign-in (autostart::BACKGROUND): in the tray only, and quietly.
    SignIn,
    /// By the tray before it, after installing an app update: waits for it to quit, then takes over.
    Restarted { open: bool },
    /// `--quit`: asks the copy that is running to quit.
    Quit,
}

fn start_of(args: &[String]) -> Option<Start> {
    let [only] = args else { return args.is_empty().then_some(Start::Normal) };
    [("--quit", Start::Quit), (BACKGROUND, Start::SignIn), (RESTARTED, Start::Restarted { open: false }), (RESTARTED_OPEN, Start::Restarted { open: true })]
        .into_iter()
        .find_map(|(flag, start)| only.eq_ignore_ascii_case(flag).then_some(start))
}

/// Runs the tray app until it quits. The process exit code.
pub fn run() -> i32 {
    let home = Paths::home_dir();
    let title = format!("{APP_NAME} {}", instance_tag(&home));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(start) = start_of(&args) else {
        message_box(&format!(
            "'{}' is not understood. {APP_NAME} takes no options, except {BACKGROUND} to start in the tray without opening its window, and --quit to close the copy that is running.",
            args.join(" ")
        ));
        return 2;
    };
    if start == Start::Quit {
        return quit_other(&title);
    }
    let background = start != Start::Normal;
    let restarted = matches!(start, Start::Restarted { .. });
    // One copy per app folder. The mutex is never closed: Windows releases it when the process ends.
    let waiting_since = Instant::now();
    loop {
        let mutex = unsafe { CreateMutexW(null(), 0, wide(&format!("Local\\tiny-skill-sync-{}", instance_tag(&home))).as_ptr()) };
        if mutex.is_null() || unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            break;
        }
        if !restarted {
            // Already running: a start at sign-in has nothing to show.
            return if background { 0 } else { wake_other(&title) };
        }
        // The tray that started this one is still quitting. If it has not gone after RESTART_WAIT, this copy leaves
        // and that one keeps running.
        unsafe { CloseHandle(mutex) };
        if waiting_since.elapsed() >= RESTART_WAIT {
            return 0;
        }
        unsafe { Sleep(100) };
    }
    if let Err(e) = std::fs::create_dir_all(&home) {
        message_box(&format!("The folder {} cannot be created: {e}", home.display()));
        return 2;
    }

    let cfg_file = home.join(CONFIG_FILE);
    let host = SystemHost;
    let executable_dir = match host.executable_dir() {
        Ok(directory) => directory,
        Err(error) => {
            message_box(&sentence(&error));
            return 2;
        }
    };
    let remote = match host.remote() {
        Ok(remote) => remote,
        Err(e) => {
            message_box(&sentence(&e));
            return 2;
        }
    };
    let cfg = Config::default();
    let skills = host.skills_dir(&cfg);
    let engine = Engine::new(Paths::new(home.clone(), skills.clone()), remote, host.tokens());

    if !register_class(CLASS, Some(main_proc), null_mut()) {
        message_box("The tray window cannot be created.");
        return 2;
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            wide(CLASS).as_ptr(),
            wide(&title).as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    };
    if hwnd.is_null() {
        message_box("The tray window cannot be created.");
        return 2;
    }
    MAIN.with(|m| m.set(hwnd));
    TASKBAR_CREATED.with(|t| t.set(unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) }));

    let (orders, inbox) = mpsc::channel();
    let (outbox, done) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let transactions = crate::updater::Transactions::default();
    if let Err(e) = spawn_worker(engine, inbox, outbox, hwnd, transactions.clone()) {
        message_box(&format!("The worker thread cannot be started: {e}"));
        return 2;
    }

    let now = now_secs();
    let mut app = App::new(cfg, skills.clone());
    app.set_settings_source(SettingsSource::new(cfg_file.clone(), executable_dir));
    app.queue_initial_settings_load();
    if start == Start::SignIn {
        app.started_by_windows(now);
    }
    if restarted {
        app.restarted(env!("CARGO_PKG_VERSION"), now);
    }
    for problem in app.cfg().problems() {
        app.board.say(now, Level::Attention, format!("Settings problem: {problem}"));
    }
    let paths = Paths::new(home.clone(), skills.clone());
    let shell = Shell {
        app,
        orders,
        done,
        cancel,
        timer: unsafe { CreateWaitableTimerW(null(), 0, null()) },
        home_watch: Watch::open(&home, false, HOME_CHANGES),
        skills_watch: Watch::open(&skills, true, SKILL_CHANGES),
        cfg_stamp: stamp_of(&cfg_file),
        seen_state: stamp_of(&paths.state_file()),
        cfg_error: false,
        portable_mode: false,
        saving_settings: false,
        initial_load: true,
        background,
        open_after_initial: false,
        feedback: None,
        sent_at: None,
        diff: None,
        finishing_install: false,
        transactions,
        quit_error: None,
        restart: None,
        reopen: start == Start::Restarted { open: true },
        home,
    };
    SHELL.with(|s| *s.borrow_mut() = Some(shell));
    tray::init(hwnd);
    bench::mark("tray", None);
    on_kick();
    let code = pump(None);
    shutdown(false);
    SHELL.with(|s| s.borrow_mut().take());
    bench::mark("exit", None);
    code
}

fn find_other(title: &str) -> HWND {
    unsafe { FindWindowW(wide(CLASS).as_ptr(), wide(title).as_ptr()) }
}

/// Another copy is running: open its window instead.
fn wake_other(title: &str) -> i32 {
    // It may still be starting.
    for _ in 0..20 {
        let other = find_other(title);
        if !other.is_null() {
            unsafe {
                let mut pid = 0;
                GetWindowThreadProcessId(other, &mut pid);
                AllowSetForegroundWindow(pid);
                PostMessageW(other, WM_COMMAND, CMD_SETTINGS, 0);
            }
            return 0;
        }
        unsafe { Sleep(100) };
    }
    0
}

/// `--quit`: asks the copy that is running to quit. 1 when none is.
fn quit_other(title: &str) -> i32 {
    let other = find_other(title);
    if other.is_null() || unsafe { PostMessageW(other, WM_COMMAND, CMD_QUIT, 0) } == 0 { 1 } else { 0 }
}

fn message_box(text: &str) {
    unsafe { MessageBoxW(null_mut(), wide(text).as_ptr(), wide(APP_NAME).as_ptr(), MB_OK | MB_ICONERROR) };
}

fn spawn_worker(
    mut engine: Engine,
    orders: Receiver<(Order, Arc<AtomicBool>)>,
    done: Sender<Done>,
    hwnd: HWND,
    transactions: crate::updater::Transactions,
) -> std::io::Result<()> {
    let hwnd = hwnd as usize;
    std::thread::Builder::new().name("worker".into()).stack_size(1 << 20).spawn(move || {
        let mut updates = crate::updater::Updates::system();
        updates.transactions = transactions;
        for (order, cancel) in orders {
            // Background work (a timed check, the first look after sign-in) runs at lower CPU, I/O and memory
            // priority, so it never gets in the way.
            let background = order.background;
            if background {
                unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
            }
            let answer = perform_with_updates(&mut engine, &order, &cancel, Some(&mut updates), now_secs());
            if background {
                unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) };
            }
            if done.send(answer).is_err() {
                break;
            }
            unsafe { PostMessageW(hwnd as HWND, WM_WORKER, 0, 0) };
        }
    })?;
    Ok(())
}

impl Shell {
    fn paths(&self) -> Paths {
        Paths::new(self.home.clone(), self.app.skills_dir().clone())
    }

    fn cfg_file(&self) -> PathBuf {
        self.home.join(CONFIG_FILE)
    }

    /// Hands the worker the next job, and sets the timer for the one after.
    fn dispatch(&mut self) {
        if let Some(order) = self.app.next_order() {
            self.sent_at = Some(Instant::now());
            // A flag belongs to exactly one order. A late cancellation can never poison its successor.
            self.cancel = Arc::new(AtomicBool::new(false));
            if let Err(mpsc::SendError((order, _))) = self.orders.send((order, self.cancel.clone())) {
                // Only if the worker died; the job fails like any other, so nothing waits for it forever.
                let why = "the worker thread stopped; quit and start Tiny Skill Sync again".to_string();
                let failed = Done {
                    job: order.job,
                    result: Err(EngineError::Failed(why.clone())),
                    next_wake: None,
                    last_checks: Vec::new(),
                    kept: Vec::new(),
                    facts: Vec::new(),
                    plugin_records: Vec::new(),
                    lists_nothing: Vec::new(),
                    private: Vec::new(),
                    stamp: None,
                    settings: None,
                    config_saves: order.config_saves.into_iter().map(|save| save.outcome(Err(why.clone()))).collect(),
                    settings_checked: false,
                    update: None,
                    seen: None,
                };
                self.on_done(failed);
            }
        }
        let at = self.app.arm(now_secs());
        unsafe {
            match at {
                Some(at) => {
                    // Absolute UTC: it follows clock changes, and fires at once on waking if the time passed.
                    let due = at.saturating_add(FILETIME_UNIX_OFFSET).saturating_mul(10_000_000);
                    SetWaitableTimer(self.timer, &due, 0, None, null(), 0);
                }
                None => {
                    CancelWaitableTimer(self.timer);
                }
            }
        }
    }

    fn on_done(&mut self, mut done: Done) -> bool {
        let job = done.job.clone();
        if !self.app.accepts_update(&done) {
            // An older read must not undo the switch the user just turned off.
            done.settings = None;
        }
        if self.finishing_install {
            self.finishing_install = false;
            self.quit_error = done.result.as_ref().err().map(|e| e.to_string());
            unsafe { PostMessageW(main_window(), WM_CLOSE, 0, 0) };
        }
        if done.settings.as_ref().and_then(|s| s.result.as_ref().ok()).is_some_and(|s| !s.matches_local_file(&self.cfg_file())) {
            done.settings = None;
            self.app.reload_before_next_job();
        }
        if let Some(settings) = done.settings.take() {
            let initial = self.initial_load;
            let first_run = settings.first_run;
            self.initial_load = false;
            match settings.result {
                Ok(loaded) => {
                    self.cfg_error = false;
                    self.portable_mode = loaded.portable;
                    self.cfg_stamp = loaded.local_stamp;
                    if self.app.cfg() != &loaded.config {
                        for problem in loaded.config.problems() {
                            self.app.board.say(now_secs(), Level::Attention, format!("Settings problem: {problem}"));
                        }
                    }
                    let moved = loaded.skills_dir != *self.app.skills_dir();
                    self.app.settings_reloaded(loaded, &job);
                    if moved {
                        self.skills_watch = None;
                    }
                }
                Err(error) => {
                    self.cfg_error = true;
                    self.app.board.say(now_secs(), Level::Error, format!("{}. Nothing is checked until it is fixed.", sentence(&error)));
                }
            }
            if initial && (self.cfg_error || self.reopen || (first_run && !self.background)) {
                self.open_after_initial = true;
            }
        }

        bench::mark(bench::job_label(&job), self.sent_at.take().map(|t| t.elapsed()));
        let config_saves = done.config_saves.clone();
        let restart = done.update.as_mut().and_then(UpdateState::take_restart);
        let settled = self.app.finish(done, now_secs());
        if settled.restart && !SHUT_DOWN.with(Cell::get) && restart.is_some() {
            self.restart = restart;
        }
        if !config_saves.is_empty() {
            self.saving_settings = false;
        }
        if !config_saves.is_empty()
            && config_saves.iter().all(|saved| saved.result.is_ok())
            && matches!(self.feedback.as_deref(), Some("Saving settings…" | "Saving app update settings…"))
        {
            self.feedback = None;
        }
        if let Some(id) = settled.removed {
            self.drop_from_config(&id);
        }
        if settled.feedback.is_some() {
            self.feedback = settled.feedback;
        } else if settled.stale_feedback {
            self.feedback = None;
        }
        if settled.diff.is_some() {
            self.diff = settled.diff;
        }
        // The job's own changes signalled the watch; a new one starts clean. Edits made while it ran show at the next look.
        if !matches!(
            job,
            Job::Look
                | Job::Plan
                | Job::Flush
                | Job::PreviewSource(_)
                | Job::SaveSource(..)
                | Job::AppUpdate(_)
                | Job::ScheduledUpdate
                | Job::InstallOnQuit { .. }
        ) {
            self.skills_watch = None;
        }
        if self.skills_watch.is_none() {
            self.skills_watch = Watch::open(self.app.skills_dir(), true, SKILL_CHANGES);
        }
        if self.home_watch.is_none() {
            self.home_watch = Watch::open(&self.home, false, HOME_CHANGES);
        }
        if matches!(job, Job::Startup | Job::Poll | Job::AppUpdate(_) | Job::ScheduledUpdate) && self.app.is_idle() && !window::is_open() {
            trim_memory();
        }
        config_saves.iter().all(|s| s.result.is_ok())
    }

    /// The engine let go of a source; it leaves the settings too, with all its types.
    fn drop_from_config(&mut self, id: &str) {
        let before = self.app.cfg().clone();
        let mut after = before.clone();
        after.remove(id);
        if before != after {
            // Stop scheduling it immediately even if the worker cannot update the settings.
            self.app.set_cfg(after.clone(), self.app.skills_dir().clone());
            self.app.queue_config_save(before, after, self.portable_mode, false);
            self.saving_settings = true;
        }
    }

    /// Something in the app's folder changed: the settings, or the record (`tss` ran).
    fn check_home(&mut self) {
        let file = self.cfg_file();
        let stamp = stamp_of(&file);
        if stamp.is_some() && stamp != self.cfg_stamp {
            self.cfg_stamp = stamp;
            if !self.portable_mode {
                // A source save also signals this watch. Reload on the worker, after its CAS reply.
                if !matches!(self.app.busy(), Some(Job::SaveSource(..))) {
                    self.cancel.store(true, Ordering::Release);
                }
                self.app.reload_before_next_job();
            }
        }
        let state = stamp_of(&self.paths().state_file());
        if state.is_some() && state != self.seen_state {
            self.seen_state = state;
            if state != self.app.state_stamp() {
                self.app.want(Job::Look);
            }
        }
    }

    fn status(&self, now: UnixSecs) -> String {
        if let Some(job) = self.app.busy().filter(|job| job.shows_work()) {
            let text = match job {
                Job::Startup => "Looking at the skills folder…",
                Job::Poll | Job::Check(_) => "Checking for updates…",
                Job::AppUpdate(crate::updater::Action::Check) => "Checking the app release…",
                Job::AppUpdate(_) | Job::ScheduledUpdate | Job::InstallOnQuit { .. } => "Updating the app…",
                Job::Remove { .. } => "Removing…",
                _ => "Working…",
            };
            return text.to_string();
        }
        if let Some(feedback) = &self.feedback {
            return feedback.clone();
        }
        if self.cfg_error {
            return CFG_UNREADABLE.to_string();
        }
        if !self.app.settings_initialized() {
            return "Loading settings…".to_string();
        }
        if !self.app.cfg().repos().iter().any(|r| r.enabled) {
            return "Add a source to start".to_string();
        }
        match self.app.next_wake() {
            Some(at) if at > now => format!("Next check in {}", span(at - now)),
            Some(_) => "Next check: soon".to_string(),
            None => String::new(),
        }
    }

    /// What stops everything, if anything does.
    fn problem(&self) -> Option<String> {
        if self.cfg_error {
            return Some(CFG_UNREADABLE.to_string());
        }
        self.app.board.problem().or(self.app.board.failure()).map(str::to_string)
    }

    /// Something is happening, or just happened, that the status line says.
    fn news(&self) -> bool {
        self.app.busy().is_some_and(Job::shows_work) || self.feedback.is_some()
    }
}

const CFG_UNREADABLE: &str = "Settings cannot be read; see Activity";

/// What the main window shows.
pub struct Snapshot {
    pub now: UnixSecs,
    pub cfg: Config,
    /// One per source, as the list and a source's page show it.
    pub sources: Vec<SourceView>,
    pub draft: Option<SourceDraft>,
    pub skills_dir: PathBuf,
    pub work_dir: PathBuf,
    pub notes: Vec<Note>,
    pub status: String,
    pub app_update_status: String,
    pub problem: Option<String>,
    pub news: bool,
    pub health: crate::board::Health,
}

pub fn snapshot() -> Option<Snapshot> {
    with_shell(|s| {
        let now = now_secs();
        let (sources, health) = s.app.board.source_views_with_health(s.app.cfg(), now);
        Snapshot {
            now,
            cfg: s.app.cfg().clone(),
            sources,
            draft: s.app.draft().cloned(),
            skills_dir: s.app.skills_dir().clone(),
            work_dir: s.paths().work_dir(),
            notes: page::activity_of(&s.app.board),
            status: s.status(now),
            app_update_status: s.app.release_summary(),
            problem: s.problem(),
            news: s.news(),
            health,
        }
    })
}

fn with_source_edit<T>(edit: impl FnOnce(&mut App) -> Result<T, String>) -> Result<T, String> {
    let result = with_shell(|s| {
        if s.cfg_error || !s.app.settings_initialized() || s.saving_settings || s.app.portable_probe_pending() {
            return Err("Settings are unavailable or still being checked. Try again after they load.".into());
        }
        edit(&mut s.app)
    })
    .unwrap_or_else(|| Err("The app is busy. Try again.".into()));
    match &result {
        Err(why) => tell(sentence(why)),
        Ok(_) => kick(),
    }
    result
}

pub fn preview_location(text: &str, every: Option<Interval>) -> Result<u64, String> {
    with_source_edit(|app| app.preview_location(text, every))
}

pub fn retry_source(ticket: u64) -> Result<(), String> {
    with_source_edit(|app| app.retry_source(ticket))
}

pub fn edit_draft(ticket: u64, change: impl FnOnce(&mut SourceConfig)) -> Result<bool, String> {
    with_source_edit(|app| app.edit_draft(ticket, change))
}

pub fn edit_draft_location(ticket: u64, text: &str, every: Option<Interval>) -> Result<(), String> {
    with_source_edit(|app| app.edit_draft_location(ticket, text, every))
}

pub fn confirm_source(ticket: u64, approve_plugins: bool) {
    let _ = with_source_edit(|app| app.confirm_source(ticket, approve_plugins));
}

/// Changes one source's settings (its types, folder rules and repository items). Returns whether anything changed.
pub fn edit_source(id: &str, change: impl FnOnce(&mut SourceConfig)) -> Result<bool, String> {
    settings_edit(|s| s.app.edit_source(id, change))
}

/// Changes the settings every source inherits. Returns whether anything changed.
pub fn edit_global(change: impl FnOnce(&mut GlobalSettings)) -> Result<bool, String> {
    settings_edit(|s| s.app.edit_global(change))
}

fn settings_edit(edit: impl FnOnce(&mut Shell) -> Result<bool, String>) -> Result<bool, String> {
    let result = with_shell(|s| {
        if s.cfg_error || !s.app.settings_initialized() || s.app.portable_probe_pending() {
            return Err("Settings are unavailable or still being checked. Try again after they load.".to_string());
        }
        if s.saving_settings || s.app.saving_source() {
            return Err("Settings are still being saved. Try again in a moment.".to_string());
        }
        let changed = edit(s)?;
        if changed {
            s.saving_settings = true;
            s.feedback = Some("Saving settings…".into());
        }
        Ok(changed)
    })
    .unwrap_or_else(|| Err("The app is busy. Try again.".into()));
    match &result {
        Err(why) => tell(sentence(why)),
        Ok(_) => kick(),
    }
    result
}

pub fn saving_source() -> bool {
    with_shell(|s| s.app.saving_source()).unwrap_or(true)
}

pub fn dismiss_source() {
    with_shell(|s| {
        if matches!(s.app.busy(), Some(Job::PreviewSource(_))) {
            s.cancel.store(true, Ordering::Release);
        }
        s.app.dismiss_source();
    });
}
/// The differences of a skill the worker read since this was last asked.
pub fn take_diff() -> Option<Shown> {
    with_shell(|s| s.diff.take()).flatten()
}

/// The user asked for a job. Whatever they were told about the last one gives way to it.
pub fn want(job: Job) -> bool {
    let queued = with_shell(|s| {
        s.feedback = None;
        if matches!(job, Job::Resolve { .. } | Job::ResolvePlugin { .. } | Job::Remove { .. })
            && matches!(s.app.busy(), Some(Job::Poll | Job::Check(_) | Job::DeleteGone(_)))
        {
            s.cancel.store(true, Ordering::Relaxed);
        }
        s.app.want(job)
    });
    kick();
    queued.unwrap_or(false)
}

/// Shows `text` on the status line until the next thing the user asks for.
pub fn tell(text: String) {
    with_shell(|s| s.feedback = Some(text));
    kick();
}

pub fn startup_offer_seen() -> Result<bool, String> {
    startup_offer_seen_at(&Paths::home_dir().join(STARTUP_OFFER_FILE))
}

fn startup_offer_seen_at(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("the Start with Windows offer could not be checked: {error}")),
    }
}

pub fn mark_startup_offer_seen() -> Result<(), String> {
    mark_startup_offer_seen_at(&Paths::home_dir().join(STARTUP_OFFER_FILE))
}

fn mark_startup_offer_seen_at(path: &Path) -> Result<(), String> {
    let result = OpenOptions::new().write(true).create_new(true).open(path);
    match result {
        Ok(mut file) => {
            if let Err(error) = file.write_all(b"seen\n").and_then(|()| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(path);
                Err(format!("the Start with Windows offer could not be remembered: {error}"))
            } else {
                Ok(())
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("the Start with Windows offer could not be remembered: {error}")),
    }
}

/// Reloads shared companions when a new window opens. The read runs through the worker queue.
pub fn reload_settings() {
    with_shell(|s| {
        s.feedback = None;
        s.app.request_settings_reload();
    });
    kick();
}

/// The worker compares and saves the edit before it takes effect, in both storage modes.
pub fn edit_config(change: impl FnOnce(&mut Config) -> Result<bool, String>) -> Result<bool, String> {
    let result = with_shell(|s| {
        if !s.app.settings_initialized() {
            return Err("Settings are still loading; try again in a moment".to_string());
        }
        if s.cfg_error {
            return Err("Settings could not be loaded; fix them before editing".to_string());
        }
        if s.saving_settings || s.app.saving_source() {
            return Err("Settings are still being saved; try again in a moment".to_string());
        }
        if s.app.portable_probe_pending() {
            return Err("Settings are being checked; try again in a moment".to_string());
        }
        let before = s.app.cfg().clone();
        let mut cfg = before.clone();
        if !change(&mut cfg)? {
            return Ok(false);
        }
        s.app.queue_config_save(before, cfg, s.portable_mode, false);
        s.saving_settings = true;
        s.feedback = Some("Saving settings…".into());
        Ok(true)
    })
    .unwrap_or_else(|| Err("the app is busy; try again".to_string()));
    match &result {
        Err(e) => tell(sentence(e)),
        Ok(_) => kick(),
    }
    result
}

/// Update preferences are revocable immediately; compare-before-save runs on the worker.
pub fn edit_app_update(change: impl FnOnce(&mut Config) -> Result<bool, String>) -> Result<bool, String> {
    let result: Result<bool, String> = with_shell(|s| {
        if s.saving_settings || s.app.saving_source() {
            return Err("App update choice not saved. Wait for the previous save to finish and try again.".into());
        }
        if s.cfg_error || !s.app.settings_initialized() || s.app.portable_probe_pending() {
            return Err("App update choice not saved. Wait for settings to finish loading or fix the settings error first.".into());
        }
        let before = s.app.cfg().clone();
        let mut after = before.clone();
        if !change(&mut after)? {
            return Ok(false);
        }
        if s.app.busy() == Some(&Job::ScheduledUpdate) {
            s.cancel.store(true, Ordering::Release);
        }
        s.app.set_cfg(after.clone(), s.app.skills_dir().clone());
        s.app.queue_config_save(before, after, s.portable_mode, true);
        s.saving_settings = true;
        s.feedback = Some("Saving app update settings…".into());
        Ok(true)
    })
    .unwrap_or_else(|| Err("The app is busy; try again".into()));
    if let Err(why) = &result {
        tell(sentence(why));
    } else {
        kick();
    }
    result
}

pub fn is_idle() -> bool {
    with_shell(|s| s.app.is_idle()).unwrap_or(false)
}

#[cfg(test)]
mod startup_offer_tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("tss-shell-{label}-{}", crate::fsx::unique_suffix()));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn startup_offer_marker_is_local_idempotent_state() {
        let directory = temp_dir("startup-offer");
        let marker = directory.join(STARTUP_OFFER_FILE);

        assert!(!startup_offer_seen_at(&marker).unwrap());
        mark_startup_offer_seen_at(&marker).unwrap();
        mark_startup_offer_seen_at(&marker).unwrap();
        assert!(startup_offer_seen_at(&marker).unwrap());

        fs::remove_dir_all(directory).unwrap();
    }
}

/// Asks for [`on_kick`] once, however often it is asked for before it runs.
pub fn kick() {
    if !KICK_POSTED.with(|k| k.replace(true)) && unsafe { PostMessageW(main_window(), WM_KICK, 0, 0) } == 0 {
        KICK_POSTED.with(|k| k.set(false));
    }
}

fn on_kick() {
    KICK_POSTED.with(|k| k.set(false));
    let Some((health, tip)) = with_shell(|s| {
        s.dispatch();
        (s.app.health(), s.app.tooltip())
    }) else {
        kick();
        return;
    };
    tray::show(health, &tip);
    window::refresh();
}

/// The handles the UI thread waits on. The folders are not watched while a job runs: the job changes them itself.
fn wait_handles() -> ([HANDLE; 3], usize) {
    let mut handles = [null_mut(); 3];
    let mut count = 0;
    with_shell(|s| {
        let busy = s.app.busy().is_some();
        let watches = [&s.home_watch, &s.skills_watch].into_iter().filter(|_| !busy).flatten().map(|w| w.handle);
        for handle in std::iter::once(s.timer).filter(|h| !h.is_null()).chain(watches) {
            handles[count] = handle;
            count += 1;
        }
    });
    (handles, count)
}

fn on_signal(handle: HANDLE) {
    let look_soon = with_shell(|s| {
        if handle == s.timer {
            s.app.on_timer(now_secs());
            return false;
        }
        if let Some(watch) = s.home_watch.as_ref().filter(|w| w.handle == handle) {
            if !watch.rearm() {
                s.home_watch = None;
            }
            s.check_home();
            return false;
        }
        if let Some(watch) = s.skills_watch.as_ref().filter(|w| w.handle == handle) {
            if !watch.rearm() {
                s.skills_watch = None;
            }
            return true;
        }
        false
    });
    if look_soon == Some(true) {
        unsafe { SetTimer(main_window(), TIMER_LOOK, LOOK_DELAY_MS, None) };
    }
}

/// Feeds a window's keyboard messages to the dialog manager: Tab, Enter and Escape work as in a dialog box.
fn is_dialog_message(msg: &MSG) -> bool {
    [dialogs::active(), window::window()]
        .into_iter()
        .filter(|w| !w.is_null())
        .any(|w| (msg.hwnd == w || unsafe { IsChild(w, msg.hwnd) } != 0) && unsafe { IsDialogMessageW(w, msg) } != 0)
}

/// The message loop. With `done`, a nested one that returns as soon as `done()` is true, or on `WM_QUIT` (which is
/// posted again for the outer loop). Returns the exit code `WM_QUIT` carried.
pub fn pump(done: Option<&dyn Fn() -> bool>) -> i32 {
    let finished = || done.is_some_and(|d| d());
    loop {
        if finished() {
            return 0;
        }
        let (handles, count) = wait_handles();
        let woke = unsafe { MsgWaitForMultipleObjectsEx(count as u32, handles.as_ptr(), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        if (WAIT_OBJECT_0..WAIT_OBJECT_0 + count as u32).contains(&woke) {
            on_signal(handles[(woke - WAIT_OBJECT_0) as usize]);
            kick();
        } else if woke == WAIT_FAILED {
            // A handle went bad. The folders are watched again after the next job.
            with_shell(|s| {
                s.home_watch = None;
                s.skills_watch = None;
            });
            unsafe { Sleep(10) };
        }
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if msg.message == WM_QUIT {
                let code = msg.wParam as i32;
                if done.is_some() {
                    unsafe { PostQuitMessage(code) };
                }
                return code;
            }
            if !is_dialog_message(&msg) {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if finished() {
                return 0;
            }
        }
    }
}

/// Quitting: the main window closes, the job that runs gets two seconds, what waits to be saved is saved, and
/// the icon goes. Safe to call more than once.
fn shutdown(clean: bool) -> bool {
    if SHUT_DOWN.with(|s| s.replace(true)) {
        let ready = with_shell(|s| {
            if !clean {
                s.cancel.store(true, Ordering::Release);
                s.transactions.close();
            }
            !s.finishing_install
        })
        .unwrap_or(false);
        if ready {
            if let Some(error) = with_shell(|s| s.quit_error.take()).flatten() {
                message_box(&format!("The app update did not finish: {error}"));
            }
            tray::remove();
        }
        return ready;
    }
    bench::mark("quit", None);
    window::close();
    let waiting = with_shell(|s| {
        s.cancel.store(true, Ordering::Release);
        if !clean {
            s.transactions.close();
        }
        s.app.quit();
        let deadline = Instant::now() + QUIT_WAIT;
        let mut saves_ok = true;
        let flushed = loop {
            if s.app.busy().is_none() {
                let Some(order) = s.app.next_order() else { break false };
                if s.orders.send((order, Arc::new(AtomicBool::new(false)))).is_err() {
                    break false;
                }
            }
            match s.done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(done) => {
                    let flush = done.job == Job::Flush;
                    let ok = done.result.is_ok();
                    saves_ok &= s.on_done(done);
                    if flush {
                        break ok;
                    }
                }
                Err(_) => break false,
            }
        };
        unsafe { CancelWaitableTimer(s.timer) };
        if s.app.finish_on_quit(install_allowed(clean, flushed, saves_ok, s.cfg_error), deadline) {
            // Still use the normal worker. Once replacement starts, a timeout is not permission
            // to tear down a half-written pair. Keep pumping messages until its result arrives.
            s.finishing_install = true;
            s.dispatch();
            true
        } else {
            if clean && s.app.has_quit_update() && (!flushed || !saves_ok || s.cfg_error || Instant::now() >= deadline) {
                s.quit_error = Some("Could not confirm a clean Quit in time. The pending app update was not installed.".into());
            }
            // This also seals the race with a last-minute entry. Wait for filesystem work already
            // in progress, not an outstanding request to GitHub. No new transaction can start.
            s.finishing_install = s.transactions.close();
            s.finishing_install
        }
    })
    .unwrap_or(false);
    if !waiting {
        if let Some(error) = with_shell(|s| s.quit_error.take()).flatten() {
            message_box(&error);
        }
        tray::remove();
    }
    !waiting
}

fn install_allowed(explicit_quit: bool, flushed: bool, saves_ok: bool, settings_error: bool) -> bool {
    explicit_quit && flushed && saves_ok && !settings_error
}

fn show_menu(hwnd: HWND, x: i32, y: i32) {
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, CMD_SETTINGS, wide("Open Tiny Skill Sync").as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_CHECK, wide("Sync now").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, CMD_QUIT, wide("Quit").as_ptr());
        SetMenuDefaultItem(menu, CMD_SETTINGS as u32, 0);
        // Without this the menu does not close when the user clicks elsewhere.
        SetForegroundWindow(hwnd);
        MENU_OPEN.with(|m| m.set(true));
        TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, x, y, 0, hwnd, null());
        MENU_OPEN.with(|m| m.set(false));
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

/// Restarts into the app update this tray installed, once no question, menu or sign-out is pending: starts the new
/// tray, which waits for this one, and quits. Quit, a close or a sign-out wins, and then the new tray is not started
/// or is stopped before it takes over. True when this tray is going away.
fn restart(hwnd: HWND) -> bool {
    let Some(pending) = with_shell(|s| s.restart.is_some()) else {
        // The shell is in use further up the stack.
        unsafe { SetTimer(hwnd, TIMER_RESTART, RESTART_RETRY_MS, None) };
        return false;
    };
    if !pending {
        return false;
    }
    let leaving = quit_or_close_queued(hwnd) || SHUT_DOWN.with(Cell::get);
    if !leaving && (ENDING.with(Cell::get) || !dialogs::active().is_null() || MENU_OPEN.with(Cell::get)) {
        unsafe { SetTimer(hwnd, TIMER_RESTART, RESTART_RETRY_MS, None) };
        return false;
    }
    let Some(Some(tray)) = with_shell(|s| s.restart.take()) else { return false };
    if leaving {
        return false;
    }
    let flag = OsStr::new(if window::is_open() { RESTARTED_OPEN } else { RESTARTED });
    // The proven file stays held until the start returns, so the program started is the one that was checked.
    let started = tray.and_then(|tray| proc::start(tray.path(), &[flag]).map_err(|e| format!("the new copy could not be started: {e}")));
    let why = match started {
        Ok(child) => match child.ended_within(RESTART_CHECK) {
            Some(code) => format!("the new copy stopped at once (exit code {code})"),
            // Looking at the queue also lets Windows deliver a sign-out it sent meanwhile.
            None if quit_or_close_queued(hwnd) || SHUT_DOWN.with(Cell::get) => {
                child.stop();
                return false;
            }
            None if ENDING.with(Cell::get) => {
                child.stop();
                "Windows began to sign out".to_string()
            }
            None => {
                HANDOFF.with(|h| h.set(Some(child)));
                if shutdown(false) {
                    if quit_or_close_queued(hwnd) {
                        stop_handoff();
                    }
                    unsafe { DestroyWindow(hwnd) };
                }
                return true;
            }
        },
        Err(why) => why,
    };
    with_shell(|s| s.feedback = Some(s.app.restart_failed(&why, now_secs())));
    kick();
    false
}

/// True if Quit or a close is waiting for the tray. The commands are taken out and posted again in their order, behind
/// other messages already waiting; a flood of them is looked at only so far.
fn quit_or_close_queued(hwnd: HWND) -> bool {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    let close = unsafe { PeekMessageW(&mut msg, hwnd, WM_CLOSE, WM_CLOSE, PM_NOREMOVE) } != 0;
    let mut waiting = Vec::new();
    while waiting.len() < 256 && unsafe { PeekMessageW(&mut msg, hwnd, WM_COMMAND, WM_COMMAND, PM_REMOVE) } != 0 {
        waiting.push((msg.wParam, msg.lParam));
    }
    for &(wp, lp) in &waiting {
        unsafe { PostMessageW(hwnd, WM_COMMAND, wp, lp) };
    }
    close || waiting.iter().any(|&(wp, _)| wp & 0xFFFF == CMD_QUIT)
}

/// The user or Windows asked this tray to go while it hands over to a new one: the new one, still waiting for this
/// process to end, is stopped, so the app stays closed.
fn stop_handoff() {
    if let Some(child) = HANDOFF.with(Cell::take) {
        child.stop();
    }
}

fn on_tray(hwnd: HWND, wp: WPARAM, lp: LPARAM) {
    let event = (lp as u32) & 0xFFFF;
    if !matches!(event, NIN_SELECT | WM_CONTEXTMENU) && event != NIN_SELECT | NINF_KEY {
        return;
    }
    let ask = dialogs::active();
    if !ask.is_null() {
        unsafe { SetForegroundWindow(ask) };
    } else if event == WM_CONTEXTMENU {
        let x = i32::from((wp & 0xFFFF) as u16 as i16);
        let y = i32::from(((wp >> 16) & 0xFFFF) as u16 as i16);
        show_menu(hwnd, x, y);
    } else {
        open_window();
    }
}

/// Opens the window, or brings it to the front. Someone is looking, so nothing waits for sign-in to settle.
fn open_window() {
    with_shell(|s| {
        s.app.user_here();
    });
    kick();
    window::open();
    window::offer_startup_once();
}

fn on_command(hwnd: HWND, id: usize) {
    if SHUT_DOWN.with(Cell::get) {
        if id == CMD_QUIT {
            stop_handoff();
        }
        return;
    }
    let ask = dialogs::active();
    match id {
        CMD_SETTINGS if !ask.is_null() => unsafe {
            SetForegroundWindow(ask);
        },
        CMD_SETTINGS => open_window(),
        CMD_CHECK => {
            want(Job::Check(None));
        }
        // The question is answered "cancel" first; then quitting is asked for again.
        CMD_QUIT if !ask.is_null() => unsafe {
            PostMessageW(ask, WM_COMMAND, IDCANCEL as usize, 0);
            PostMessageW(hwnd, WM_COMMAND, CMD_QUIT, 0);
        },
        CMD_QUIT if shutdown(true) => unsafe {
            DestroyWindow(hwnd);
        },
        #[cfg(debug_assertions)]
        CMD_TEST_RESTART | CMD_TEST_RESTART_WRONG => {
            crate::updater::TEST_RESTART.store(if id == CMD_TEST_RESTART { 1 } else { 2 }, Ordering::Release);
            want(Job::AppUpdate(crate::updater::Action::Install));
        }
        _ => {}
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_KICK => on_kick(),
        WM_WORKER => {
            let open_window_after_initial = with_shell(|s| {
                while let Ok(done) = s.done.try_recv() {
                    s.on_done(done);
                }
                std::mem::take(&mut s.open_after_initial)
            });
            if restart(hwnd) {
                return 0;
            }
            if open_window_after_initial == Some(true) {
                open_window();
            } else if open_window_after_initial.is_none() {
                unsafe { PostMessageW(hwnd, WM_WORKER, 0, 0) };
            }
            window::offer_startup_once();
            kick();
        }
        tray::WM_TRAY => on_tray(hwnd, wp, lp),
        WM_COMMAND => on_command(hwnd, wp & 0xFFFF),
        WM_TIMER if wp == TIMER_LOOK => {
            unsafe { KillTimer(hwnd, TIMER_LOOK) };
            match with_shell(|s| s.app.want(Job::Look)) {
                Some(_) => kick(),
                None => unsafe {
                    SetTimer(hwnd, TIMER_LOOK, 100, None);
                },
            }
        }
        WM_TIMER if wp == TIMER_RESTART => {
            unsafe { KillTimer(hwnd, TIMER_RESTART) };
            restart(hwnd);
        }
        WM_POWERBROADCAST => {
            if wp == PBT_APMRESUMEAUTOMATIC as usize {
                with_shell(|s| s.app.resumed(now_secs()));
                kick();
            }
            return 1;
        }
        WM_TIMECHANGE => kick(),
        WM_QUERYENDSESSION => {
            ENDING.with(|e| e.set(true));
            return 1;
        }
        WM_ENDSESSION => {
            if wp != 0 {
                stop_handoff();
                shutdown(false);
            } else {
                ENDING.with(|e| e.set(false));
            }
        }
        WM_CLOSE => {
            stop_handoff();
            if shutdown(false) {
                unsafe { DestroyWindow(hwnd) };
            }
        }
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        // The scale or the display changed: the tray icon is drawn again if the taskbar wants another size.
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
            tray::rescale();
            return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
        }
        _ if msg != 0 && msg == TASKBAR_CREATED.with(Cell::get) => {
            tray::rescale();
            tray::readd();
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_completion_side_save_or_an_unclean_exit_never_authorizes_the_install_order() {
        let base = std::env::var_os("TSS_TEST_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let home = base.join(format!("tss-it-quit-save-{}", crate::fsx::unique_suffix()));
        std::fs::create_dir(&home).unwrap();
        let path = home.join(CONFIG_FILE);
        let before = Config::default();
        let mut off = before.clone();
        off.app_update.enabled = false;
        off.save(&path).unwrap();
        let saved = before.save_if_unchanged(&path, &before).is_ok();
        assert!(!saved);
        assert!(!install_allowed(true, true, saved, false));
        assert!(!install_allowed(false, true, true, false));
        assert!(!install_allowed(true, false, true, false));
        assert!(!install_allowed(true, true, true, true));
        assert!(install_allowed(true, true, true, false));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn one_copy_per_app_folder_whatever_its_case() {
        let tag = instance_tag(Path::new(r"C:\Users\Someone\AppData\Local\tiny-skill-sync"));
        assert_eq!(tag, instance_tag(Path::new(r"c:\users\someone\appdata\local\TINY-SKILL-SYNC")));
        assert_ne!(tag, instance_tag(Path::new(r"C:\Temp\tss-home")));
        assert_eq!(tag.len(), 16);
    }

    #[test]
    fn a_restarted_tray_is_told_apart_from_one_the_user_or_windows_started() {
        let start = |args: &[&str]| start_of(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        assert_eq!(start(&[]), Some(Start::Normal));
        assert_eq!(start(&["--BACKGROUND"]), Some(Start::SignIn));
        assert_eq!(start(&["--Quit"]), Some(Start::Quit));
        // Older trays start newer ones with these, so their meaning never changes.
        assert_eq!(start(&["--restarted"]), Some(Start::Restarted { open: false }));
        assert_eq!(start(&["--RESTARTED-OPEN"]), Some(Start::Restarted { open: true }));
        assert_eq!(start(&["--restarted", "--quit"]), None);
        assert_eq!(start(&["--restart"]), None);
    }
}
