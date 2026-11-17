//! The tray app's process: one UI thread with a hidden window, one worker thread that owns the engine.
//!
//! The UI thread sleeps in `MsgWaitForMultipleObjectsEx` on window messages and three handles: a waitable timer set
//! to the next check, and change notifications on the app's folder (`config.json`, `state.json`) and on the skills
//! folder. Nothing polls. A change in the skills folder only leads to a read-only look, two seconds after the last
//! change; safety never depends on it, because every check compares the disk with the record first.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
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
    MessageBoxW, MsgWaitForMultipleObjectsEx, PBT_APMRESUMEAUTOMATIC, PM_REMOVE, PeekMessageW, PostMessageW, PostQuitMessage, QS_ALLINPUT,
    RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem, SetTimer, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP,
    WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_NULL, WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_QUIT, WM_TIMECHANGE, WM_TIMER,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use super::{bench, dialogs, local_time, register_class, sentence, settings, tray, trim_memory};
use crate::app::{App, Done, Job, Order, perform};
use crate::board::{APP_NAME, Board, Level, RepoView, span};
use crate::cli::{Host, SystemHost};
use crate::config::{CONFIG_FILE, Config, Paths};
use crate::engine::{Engine, EngineError, Stamp, stamp_of};
use crate::interval::{UnixSecs, now_secs};
use crate::pathsafe::{printable, printable_lines};
use crate::win::{wide, wide_os};

/// Tray menu and remote-control commands, sent as `WM_COMMAND`.
pub const CMD_SETTINGS: usize = 1001;
pub const CMD_CHECK: usize = 1002;
pub const CMD_QUIT: usize = 1003;
/// The worker finished an order.
const WM_WORKER: u32 = WM_APP + 2;
/// Something changed: start the next job, set the timer, update the icon and the settings window.
const WM_KICK: u32 = WM_APP + 3;
const TIMER_LOOK: usize = 1;
/// A look at the skills folder waits until it has been quiet this long.
const LOOK_DELAY_MS: u32 = 2000;
/// How long quitting waits for the job that is running and for saving what is waiting to be saved.
const QUIT_WAIT: Duration = Duration::from_secs(2);
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
    orders: Sender<Order>,
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
    /// How the last thing the user asked for went, for the status line.
    feedback: Option<String>,
    /// When the worker was handed the job it runs.
    sent_at: Option<Instant>,
}

thread_local! {
    static SHELL: RefCell<Option<Shell>> = const { RefCell::new(None) };
    static MAIN: Cell<HWND> = const { Cell::new(null_mut()) };
    static KICK_POSTED: Cell<bool> = const { Cell::new(false) };
    static SHUT_DOWN: Cell<bool> = const { Cell::new(false) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
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

/// Runs the tray app until it quits. The process exit code.
pub fn run() -> i32 {
    let home = Paths::home_dir();
    let title = format!("{APP_NAME} {}", instance_tag(&home));
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [only] if only.eq_ignore_ascii_case("--quit") => return quit_other(&title),
        _ => {
            message_box(&format!("'{}' is not understood. {APP_NAME} takes no options, except --quit to close the copy that is running.", args.join(" ")));
            return 2;
        }
    }
    // One copy per app folder. The mutex is never closed: Windows releases it when the process ends.
    let mutex = unsafe { CreateMutexW(null(), 0, wide(&format!("Local\\tiny-skill-sync-{}", instance_tag(&home))).as_ptr()) };
    if !mutex.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return wake_other(&title);
    }
    if let Err(e) = std::fs::create_dir_all(&home) {
        message_box(&format!("The folder {} cannot be created: {e}", home.display()));
        return 2;
    }

    let cfg_file = home.join(CONFIG_FILE);
    let first_run = !cfg_file.exists();
    let (cfg, cfg_error) = match Config::load(&cfg_file) {
        Ok(cfg) => (cfg, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let host = SystemHost;
    let remote = match host.remote() {
        Ok(remote) => remote,
        Err(e) => {
            message_box(&sentence(&e));
            return 2;
        }
    };
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
    if let Err(e) = spawn_worker(engine, inbox, outbox, cancel.clone(), hwnd) {
        message_box(&format!("The worker thread cannot be started: {e}"));
        return 2;
    }

    let now = now_secs();
    let mut app = App::new(cfg, skills.clone());
    for problem in app.cfg().problems() {
        app.board.say(now, Level::Attention, format!("Settings problem: {problem}"));
    }
    if let Some(e) = &cfg_error {
        app.board.say(now, Level::Error, format!("{}. Nothing is checked until it is fixed.", sentence(e)));
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
        cfg_error: cfg_error.is_some(),
        feedback: None,
        sent_at: None,
        home,
    };
    SHELL.with(|s| *s.borrow_mut() = Some(shell));
    tray::init(hwnd);
    bench::mark("tray", None);
    on_kick();
    if first_run || cfg_error.is_some() {
        settings::open();
    }

    let code = pump(None);
    shutdown();
    SHELL.with(|s| s.borrow_mut().take());
    bench::mark("exit", None);
    code
}

/// Keeps one copy per app folder: `TSS_HOME` gives tests a copy of their own.
fn instance_tag(home: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in home.to_string_lossy().to_lowercase().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn find_other(title: &str) -> HWND {
    unsafe { FindWindowW(wide(CLASS).as_ptr(), wide(title).as_ptr()) }
}

/// Another copy is running: open its settings window instead.
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

fn spawn_worker(mut engine: Engine, orders: Receiver<Order>, done: Sender<Done>, cancel: Arc<AtomicBool>, hwnd: HWND) -> std::io::Result<()> {
    let hwnd = hwnd as usize;
    std::thread::Builder::new().name("worker".into()).stack_size(1 << 20).spawn(move || {
        for order in orders {
            // A timed check is background work: lower CPU, I/O and memory priority, so it never gets in the way.
            let background = order.job == Job::Poll;
            if background {
                unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
            }
            let answer = perform(&mut engine, &order, &cancel);
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
        let paused = self.cfg_error && !self.app.started();
        if !paused {
            if let Some(order) = self.app.next_order() {
                self.sent_at = Some(Instant::now());
                if let Err(mpsc::SendError(order)) = self.orders.send(order) {
                    // Only if the worker died; the job fails like any other, so nothing waits for it forever.
                    let why = "the worker thread stopped; quit and start Tiny Skill Sync again".to_string();
                    let failed = Done {
                        job: order.job,
                        result: Err(EngineError::Failed(why)),
                        next_wake: None,
                        last_checks: Vec::new(),
                        kept: Vec::new(),
                        lists_nothing: Vec::new(),
                        stamp: None,
                    };
                    self.on_done(failed);
                }
            }
        }
        let at = if paused { None } else { self.app.arm(now_secs()) };
        unsafe {
            match at {
                Some(at) => {
                    // Absolute UTC: it follows clock changes, and fires at once on waking if the time passed.
                    let due = (at + FILETIME_UNIX_OFFSET) * 10_000_000;
                    SetWaitableTimer(self.timer, &due, 0, None, null(), 0);
                }
                None => {
                    CancelWaitableTimer(self.timer);
                }
            }
        }
    }

    fn on_done(&mut self, done: Done) {
        let job = done.job.clone();
        bench::mark(bench::job_label(&job), self.sent_at.take().map(|t| t.elapsed()));
        let settled = self.app.finish(done, now_secs());
        if let Some(id) = settled.removed {
            self.drop_from_config(&id);
        }
        if settled.feedback.is_some() {
            self.feedback = settled.feedback;
        }
        // The job's own changes signalled the watch; a new one starts clean. Edits made while it ran show at the next look.
        if !matches!(job, Job::Look | Job::Plan | Job::Flush) {
            self.skills_watch = None;
        }
        if self.skills_watch.is_none() {
            self.skills_watch = Watch::open(self.app.skills_dir(), true, SKILL_CHANGES);
        }
        if self.home_watch.is_none() {
            self.home_watch = Watch::open(&self.home, false, HOME_CHANGES);
        }
        if matches!(job, Job::Startup | Job::Poll) && self.app.is_idle() && !settings::is_open() {
            trim_memory();
        }
    }

    /// The engine let go of a repository; it leaves the settings too.
    fn drop_from_config(&mut self, id: &str) {
        let file = self.cfg_file();
        let saved = Config::load(&file).and_then(|mut cfg| {
            cfg.repos.retain(|r| r.id != id);
            cfg.save(&file).map(|()| cfg)
        });
        match saved {
            Ok(cfg) => {
                self.cfg_stamp = stamp_of(&file);
                self.apply_cfg(cfg, false);
            }
            Err(e) => {
                // Not checked again in this session, whatever the file says.
                let mut cfg = self.app.cfg().clone();
                cfg.repos.retain(|r| r.id != id);
                self.apply_cfg(cfg, false);
                let text = format!(
                    "{}. The skills of '{id}' are no longer managed, but it is still in the settings: remove it from {} by hand.",
                    sentence(&e),
                    file.display()
                );
                self.app.board.say(now_secs(), Level::Error, text);
            }
        }
    }

    /// Uses new settings. `tell` says what is wrong with them, for settings changed outside the app.
    fn apply_cfg(&mut self, cfg: Config, tell: bool) {
        if tell {
            let now = now_secs();
            for problem in cfg.problems() {
                self.app.board.say(now, Level::Attention, format!("Settings problem: {problem}"));
            }
        }
        let skills = Paths::skills_dir(&cfg);
        let moved = skills != *self.app.skills_dir();
        self.app.set_cfg(cfg, skills);
        if moved {
            self.skills_watch = Watch::open(self.app.skills_dir(), true, SKILL_CHANGES);
        }
    }

    /// Something in the app's folder changed: the settings, or the record (`tss` ran).
    fn check_home(&mut self) {
        let file = self.cfg_file();
        let stamp = stamp_of(&file);
        if stamp.is_some() && stamp != self.cfg_stamp {
            self.cfg_stamp = stamp;
            match Config::load(&file) {
                Ok(cfg) => {
                    self.cfg_error = false;
                    self.apply_cfg(cfg, true);
                }
                Err(e) => {
                    self.cfg_error = true;
                    let text = format!("{}. The settings in use are unchanged, and nothing is saved to the file until it is fixed.", sentence(&e));
                    self.app.board.say(now_secs(), Level::Error, text);
                }
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
                Job::Remove { .. } => "Removing…",
                _ => "Working…",
            };
            return text.to_string();
        }
        if let Some(feedback) = &self.feedback {
            return feedback.clone();
        }
        if self.cfg_error {
            return "config.json cannot be read; see the messages below".to_string();
        }
        if !self.app.cfg().repos.iter().any(|r| r.enabled) {
            return "Add a repository to start".to_string();
        }
        match self.app.next_wake() {
            Some(at) if at > now => format!("Next check in {}", span(at - now)),
            Some(_) => "Next check: soon".to_string(),
            None => String::new(),
        }
    }

    fn notes(&self) -> String {
        notes_of(&self.app.board)
    }
}

/// The messages the settings window shows. Text that came from a repository is escaped (see `printable_lines`), so a
/// skill or file name cannot reorder or hide what is shown.
fn notes_of(board: &Board) -> String {
    let mut lines = Vec::new();
    for problem in board.problem().into_iter().chain(board.failure()) {
        lines.push(format!("Problem: {}", printable(problem)));
    }
    if !board.orphans().is_empty() {
        lines.push(format!(
            "Skills recorded for repositories that are no longer in the settings are left alone: {}. To stop tracking them, run: tss remove ID",
            printable(&board.orphans().join(", "))
        ));
    }
    let messages: Vec<_> = board.messages().collect();
    for message in messages.iter().rev() {
        let level = match message.level {
            Level::Error => "Error: ",
            Level::Attention => "Attention: ",
            Level::Ok => "",
        };
        lines.push(format!("{}  {level}{}", local_time(message.at), printable_lines(&message.text).replace('\n', "\r\n")));
    }
    lines.join("\r\n")
}

/// What the settings window shows.
pub struct Snapshot {
    pub now: UnixSecs,
    pub cfg: Config,
    pub views: Vec<RepoView>,
    pub skills_dir: PathBuf,
    pub work_dir: PathBuf,
    pub notes: String,
    pub status: String,
}

pub fn snapshot() -> Option<Snapshot> {
    with_shell(|s| {
        let now = now_secs();
        Snapshot {
            now,
            cfg: s.app.cfg().clone(),
            views: s.app.board.views(s.app.cfg(), now),
            skills_dir: s.app.skills_dir().clone(),
            work_dir: s.paths().work_dir(),
            notes: s.notes(),
            status: s.status(now),
        }
    })
}

/// The user asked for a job. Whatever they were told about the last one gives way to it.
pub fn want(job: Job) -> bool {
    let queued = with_shell(|s| {
        s.feedback = None;
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

/// Changes the settings in `config.json` as it is now on disk, and uses them. `change` returns whether it changed
/// anything; nothing is saved when it did not, or when it fails. A failure is also shown on the status line.
pub fn edit_config(change: impl FnOnce(&mut Config) -> Result<bool, String>) -> Result<bool, String> {
    let result = with_shell(|s| {
        let file = s.cfg_file();
        let mut cfg = Config::load(&file)?;
        if !change(&mut cfg)? {
            return Ok(false);
        }
        cfg.save(&file)?;
        s.cfg_stamp = stamp_of(&file);
        s.cfg_error = false;
        s.feedback = None;
        s.apply_cfg(cfg, false);
        Ok(true)
    })
    .unwrap_or_else(|| Err("the app is busy; try again".to_string()));
    match &result {
        Err(e) => tell(sentence(e)),
        Ok(_) => kick(),
    }
    result
}

pub fn is_idle() -> bool {
    with_shell(|s| s.app.is_idle()).unwrap_or(false)
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
    settings::refresh();
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
    [dialogs::active(), settings::window()]
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

/// Quitting: the settings window closes, the job that runs gets two seconds, what waits to be saved is saved, and
/// the icon goes. Safe to call more than once.
fn shutdown() {
    if SHUT_DOWN.with(|s| s.replace(true)) {
        return;
    }
    bench::mark("quit", None);
    settings::close();
    with_shell(|s| {
        s.cancel.store(true, Ordering::Relaxed);
        s.app.quit();
        let deadline = Instant::now() + QUIT_WAIT;
        loop {
            if s.app.busy().is_none() {
                let Some(order) = s.app.next_order() else { break };
                if s.orders.send(order).is_err() {
                    break;
                }
            }
            match s.done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(done) => s.on_done(done),
                Err(_) => break,
            }
        }
        unsafe { CancelWaitableTimer(s.timer) };
    });
    tray::remove();
}

fn show_menu(hwnd: HWND, x: i32, y: i32) {
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, CMD_SETTINGS, wide("Open settings").as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_CHECK, wide("Check now").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, CMD_QUIT, wide("Quit").as_ptr());
        SetMenuDefaultItem(menu, CMD_SETTINGS as u32, 0);
        // Without this the menu does not close when the user clicks elsewhere.
        SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, x, y, 0, hwnd, null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
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
        settings::open();
    }
}

fn on_command(hwnd: HWND, id: usize) {
    let ask = dialogs::active();
    match id {
        CMD_SETTINGS if !ask.is_null() => unsafe {
            SetForegroundWindow(ask);
        },
        CMD_SETTINGS => settings::open(),
        CMD_CHECK => {
            want(Job::Check(None));
        }
        // The question is answered "cancel" first; then quitting is asked for again.
        CMD_QUIT if !ask.is_null() => unsafe {
            PostMessageW(ask, WM_COMMAND, IDCANCEL as usize, 0);
            PostMessageW(hwnd, WM_COMMAND, CMD_QUIT, 0);
        },
        CMD_QUIT => {
            shutdown();
            unsafe { DestroyWindow(hwnd) };
        }
        _ => {}
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_KICK => on_kick(),
        WM_WORKER => {
            let drained = with_shell(|s| {
                while let Ok(done) = s.done.try_recv() {
                    s.on_done(done);
                }
            });
            if drained.is_none() {
                unsafe { PostMessageW(hwnd, WM_WORKER, 0, 0) };
            }
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
        WM_POWERBROADCAST => {
            if wp == PBT_APMRESUMEAUTOMATIC as usize {
                with_shell(|s| s.app.resumed(now_secs()));
                kick();
            }
            return 1;
        }
        WM_TIMECHANGE => kick(),
        WM_QUERYENDSESSION => return 1,
        WM_ENDSESSION => {
            if wp != 0 {
                shutdown();
            }
        }
        WM_CLOSE => {
            shutdown();
            unsafe { DestroyWindow(hwnd) };
        }
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        _ if msg != 0 && msg == TASKBAR_CREATED.with(Cell::get) => tray::readd(),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_copy_per_app_folder_whatever_its_case() {
        let tag = instance_tag(Path::new(r"C:\Users\Someone\AppData\Local\tiny-skill-sync"));
        assert_eq!(tag, instance_tag(Path::new(r"c:\users\someone\appdata\local\TINY-SKILL-SYNC")));
        assert_ne!(tag, instance_tag(Path::new(r"C:\Temp\tss-home")));
        assert_eq!(tag.len(), 16);
    }

    #[test]
    fn the_messages_never_show_a_control_character_from_a_repository() {
        let mut board = Board::new();
        board.say(1_700_000_000, Level::Attention, "fx: a\u{1b}[2K\rb\u{202e}exe.pdf\nsecond line".to_string());
        board.say(1_700_000_001, Level::Ok, "fx: plain".to_string());
        let notes = notes_of(&board);
        assert!(notes.contains("a\\u{1b}[2K\\u{d}b\\u{202e}exe.pdf\r\nsecond line"), "{notes:?}");
        assert!(notes.contains("fx: plain"), "{notes:?}");
        // What is left is the line break the edit box needs, written out as CR LF, and nothing else.
        assert!(notes.chars().all(|c| matches!(c, '\r' | '\n') || !c.is_control()), "{notes:?}");
        assert!(!notes.contains('\u{202e}'));
    }
}
