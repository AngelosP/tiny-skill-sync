//! Start with Windows, without Windows: what the startup entry says and what it means.
//!
//! The app is started at sign-in by a value under the user's `Run` key. Windows reads it once the desktop is up, at a
//! low priority and after a delay of its own, so nothing here can slow sign-in down. Task Manager's Startup apps page
//! and Settings > Apps > Startup switch an entry off with a mark of their own under `StartupApproved\Run`, which is
//! respected and shown. [`crate::win::autorun`] reads and writes the registry; this module decides what to write and
//! what was read means.

use std::borrow::Cow;
use std::path::Path;

use crate::config::{Paths, instance_tag};
use crate::pathsafe::printable;

/// The value's name, which Task Manager and Settings show.
pub const VALUE_NAME: &str = "Tiny Skill Sync";
/// The option the entry starts the app with: no window, and its first look and check wait a little.
pub const BACKGROUND: &str = "--background";
/// Windows does not start a longer command from the `Run` key.
pub const MAX_COMMAND: usize = 260;

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
/// Where a copy with an app folder of its own (`TSS_HOME`: the scripts and tests) keeps its values. Windows never reads
/// them.
pub const TEST_ROOT: &str = r"Software\tiny-skill-sync-test";

/// The two keys under `HKEY_CURRENT_USER`: the one with the entry, and the one with Windows' mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keys {
    pub run: String,
    pub approved: String,
}

impl Keys {
    /// The keys Windows reads.
    pub fn windows() -> Keys {
        Keys { run: RUN.to_string(), approved: APPROVED.to_string() }
    }

    /// Windows' keys, or for a copy with an app folder of its own, keys under [`TEST_ROOT`] that only it uses.
    pub fn choose(own_home: Option<&Path>) -> Keys {
        match own_home {
            None => Keys::windows(),
            Some(home) => {
                let tag = instance_tag(home);
                Keys { run: format!(r"{TEST_ROOT}\{tag}\Run"), approved: format!(r"{TEST_ROOT}\{tag}\StartupApproved\Run") }
            }
        }
    }

    pub fn current() -> Keys {
        Keys::choose(Paths::own_home().as_deref())
    }

    /// Whether these keys may be written: both under [`TEST_ROOT`], or (when `windows` allows it) exactly Windows' own.
    pub fn allowed(&self, windows: bool) -> bool {
        let own = Keys::windows();
        let is_windows = self.run.eq_ignore_ascii_case(&own.run) && self.approved.eq_ignore_ascii_case(&own.approved);
        (windows && is_windows) || (under_test_root(&self.run) && under_test_root(&self.approved))
    }
}

fn under_test_root(key: &str) -> bool {
    let n = TEST_ROOT.len();
    key.get(..n).is_some_and(|head| head.eq_ignore_ascii_case(TEST_ROOT)) && key.get(n..).is_some_and(|rest| rest.len() > 1 && rest.starts_with('\\'))
}

/// This program's path, in the two forms an entry may name it by: the long one, and the short (8.3) one when the
/// volume keeps short names and it is different.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exe {
    pub long: String,
    pub short: Option<String>,
    aliases: Vec<String>,
}

impl Exe {
    pub fn new(long: &str, short: Option<&str>) -> Exe {
        let long = plain(long).into_owned();
        let short = short.map(|s| plain(s).into_owned()).filter(|s| !s.is_empty() && !same_path(s, &long));
        Exe { long, short, aliases: Vec::new() }
    }

    /// Also recognize a previous path spelling, without using it for a new startup entry.
    pub(crate) fn with_alias(mut self, path: &str) -> Exe {
        let alias = plain(path).into_owned();
        if !self.is(&alias) {
            self.aliases.push(alias);
        }
        self
    }

    /// What the entry says: the long path when the command fits in [`MAX_COMMAND`], else the short one. `None` when
    /// neither fits.
    pub fn command(&self) -> Option<String> {
        std::iter::once(&self.long).chain(&self.short).map(|path| format!("\"{path}\" {BACKGROUND}")).find(|c| c.encode_utf16().count() <= MAX_COMMAND)
    }

    /// Whether a path names this program, in either form.
    pub fn is(&self, path: &str) -> bool {
        let path = plain(path);
        same_path(&path, &self.long)
            || self.short.as_deref().is_some_and(|short| same_path(&path, short))
            || self.aliases.iter().any(|alias| same_path(&path, alias))
    }
}

/// Windows compares file names without case.
fn same_path(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// A path without the `\\?\` prefix that lifts the length limit: the entry is read by a program that does not take it.
pub fn plain(path: &str) -> Cow<'_, str> {
    const UNC: &str = r"\\?\UNC\";
    if path.get(..UNC.len()).is_some_and(|head| head.eq_ignore_ascii_case(UNC)) {
        return Cow::Owned(format!(r"\\{}", &path[UNC.len()..]));
    }
    Cow::Borrowed(path.strip_prefix(r"\\?\").unwrap_or(path))
}

/// Prefer a resolved UNC path, but keep the current path for every other result.
pub fn prefer_unc(current: &str, final_path: Option<&str>) -> String {
    const UNC: &str = r"\\?\UNC\";
    match final_path.filter(|path| path.get(..UNC.len()).is_some_and(|head| head.eq_ignore_ascii_case(UNC))) {
        Some(path) => plain(path).into_owned(),
        None => current.to_string(),
    }
}

/// The program a command line starts, found the way Windows finds it: the text in quotes, or else the text up to the
/// first `.exe` that ends a word, or else the first word.
pub fn program(command: &str) -> &str {
    let command = command.trim_start();
    if let Some(quoted) = command.strip_prefix('"') {
        return quoted.split('"').next().unwrap_or(quoted);
    }
    // ASCII lowercase keeps every byte where it was.
    let lower = command.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(".exe") {
        let end = from + at + ".exe".len();
        if lower[end..].chars().next().is_none_or(char::is_whitespace) {
            return &command[..end];
        }
        from = end;
    }
    command.split_whitespace().next().unwrap_or("")
}

/// Windows' mark says the entry is switched off. The first byte is odd then (3 where 2 is on); the rest is when.
pub fn switched_off(mark: Option<&[u8]>) -> bool {
    mark.and_then(|m| m.first()).is_some_and(|b| b & 1 == 1)
}

/// Where a note is read, which decides how it says to turn the switch on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// Under the switch in the window.
    Window,
    /// From `tss startup`.
    Tss,
}

/// Whether Windows starts this copy of the app when the user signs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Startup {
    Off,
    On,
    /// This copy's entry is there, but switched off in Task Manager or Settings.
    OffInWindows,
    /// The entry starts the program at this path: another copy of the app.
    OtherCopy(String),
    /// This copy's path is too long for an entry, even in its short form.
    TooLong,
}

impl Startup {
    pub fn decide(entry: Option<&str>, mark: Option<&[u8]>, exe: &Exe) -> Startup {
        let entry = entry.map(str::trim).filter(|e| !e.is_empty());
        if entry.is_some_and(|e| exe.is(program(e))) {
            return if switched_off(mark) { Startup::OffInWindows } else { Startup::On };
        }
        if exe.command().is_none() {
            return Startup::TooLong;
        }
        match entry {
            Some(e) => Startup::OtherCopy(program(e).to_string()),
            None => Startup::Off,
        }
    }

    pub fn is_on(&self) -> bool {
        *self == Startup::On
    }

    /// The switch can be turned on or off: it cannot when the path is too long.
    pub fn can_change(&self) -> bool {
        *self != Startup::TooLong
    }

    /// What it is, in a few words: for `tss startup`.
    pub fn summary(&self) -> &'static str {
        match self {
            Startup::On => "on",
            Startup::Off | Startup::OffInWindows | Startup::OtherCopy(_) => "off",
            Startup::TooLong => "not possible",
        }
    }

    /// One plain sentence or two, under the switch and from `tss startup`. Never starts with a path, and ASCII only but
    /// for another copy's path, which is escaped like any text from outside the app.
    pub fn note(&self, via: Via) -> String {
        let turn_on = match via {
            Via::Window => "Turn it on here",
            Via::Tss => "Run 'tss startup on'",
        };
        match self {
            Startup::Off | Startup::On => "Starts in the tray when you sign in to Windows, without opening its window.".to_string(),
            Startup::OffInWindows => format!("It is turned off in Windows' startup apps (Task Manager or Settings). {turn_on} to start it again."),
            Startup::OtherCopy(path) => format!("Windows starts another copy when you sign in: {}. {turn_on} to start this copy instead.", printable(path)),
            Startup::TooLong => "This copy is in a folder whose path is too long for Windows to start it. Move it to a shorter folder to use this.".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: &str = r"C:\Users\someone\AppData\Local\Programs\tiny-skill-sync\tiny-skill-sync.exe";
    const SHORT: &str = r"C:\Users\someone\AppData\Local\Programs\TINY-S~1\TINY-S~1.EXE";

    /// A path of exactly `n` characters.
    fn path_of(n: usize) -> String {
        let tail = r"\tiny-skill-sync.exe";
        format!(r"C:\{}{tail}", "d".repeat(n - 3 - tail.len()))
    }

    #[test]
    fn the_entry_starts_the_long_path_when_it_fits_and_the_short_one_when_only_that_does() {
        let fits = Exe::new(&path_of(245), Some(r"C:\DDDDDD~1\TINY-S~1.EXE"));
        assert_eq!(fits.command().map(|c| c.encode_utf16().count()), Some(MAX_COMMAND), "the longest that fits");
        assert_eq!(fits.command(), Some(format!("\"{}\" --background", path_of(245))));

        let over = Exe::new(&path_of(246), Some(r"C:\DDDDDD~1\TINY-S~1.EXE"));
        assert_eq!(over.command().as_deref(), Some(r#""C:\DDDDDD~1\TINY-S~1.EXE" --background"#));
        assert_eq!(Exe::new(&path_of(246), None).command(), None, "no short name: the volume keeps none");
        assert_eq!(Exe::new(&path_of(300), Some(&path_of(250))).command(), None, "the short one is too long as well");

        // Counted in UTF-16 units, as Windows counts.
        let wide = format!(r"C:\{}\a.exe", "\u{1F600}".repeat(120));
        assert_eq!(wide.chars().count(), 129);
        assert_eq!(Exe::new(&wide, None).command(), None);
    }

    #[test]
    fn a_path_is_this_program_in_either_form_whatever_its_case_or_prefix() {
        let exe = Exe::new(&format!(r"\\?\{LONG}"), Some(SHORT));
        assert_eq!(exe.long, LONG, "the prefix is not written to the registry");
        assert!(exe.is(LONG) && exe.is(SHORT));
        assert!(exe.is(&LONG.to_uppercase()) && exe.is(&SHORT.to_lowercase()));
        assert!(exe.is(&format!(r"\\?\{SHORT}")));
        assert!(!exe.is(r"C:\Users\someone\Downloads\tiny-skill-sync.exe"));
        assert!(!exe.is(""));

        let accents = Exe::new(r"C:\Users\Ángel\tss\tiny-skill-sync.exe", None);
        assert!(accents.is(r"c:\users\ángel\TSS\tiny-skill-sync.exe"));

        let share = Exe::new(r"\\?\UNC\server\share\tss\tiny-skill-sync.exe", None);
        assert_eq!(share.long, r"\\server\share\tss\tiny-skill-sync.exe");
        assert_eq!(Exe::new(LONG, Some(&LONG.to_uppercase())).short, None, "a short form that is the long one is no short form");
    }

    #[test]
    fn a_rewritten_unc_path_still_recognizes_the_old_entry() {
        let unc = r"\\server\share\tss\tiny-skill-sync.exe";
        let old = r"Z:\tss\tiny-skill-sync.exe";
        let short = r"Z:\TSS\TINY-S~1.EXE";
        let exe = Exe::new(unc, Some(short)).with_alias(old);
        assert!(exe.is(unc) && exe.is(old) && exe.is(short));
        assert_eq!(Startup::decide(Some(&format!("\"{old}\" {BACKGROUND}")), None, &exe), Startup::On);
        assert_eq!(exe.command().as_deref(), Some(r#""\\server\share\tss\tiny-skill-sync.exe" --background"#));
    }

    #[test]
    fn only_a_resolved_unc_final_path_replaces_the_current_path() {
        let current = r"Z:\apps\tiny-skill-sync.exe";
        assert_eq!(prefer_unc(current, Some(r"\\?\UNC\server\share\apps\tiny-skill-sync.exe")), r"\\server\share\apps\tiny-skill-sync.exe");
        assert_eq!(
            prefer_unc(current, Some(r"\\?\unc\server\share\apps\tiny-skill-sync.exe")),
            r"\\server\share\apps\tiny-skill-sync.exe",
            "the prefix is case-insensitive"
        );
        assert_eq!(prefer_unc(current, None), current, "a resolution failure keeps the current path");
        assert_eq!(prefer_unc(current, Some(r"\\?\C:\apps\tiny-skill-sync.exe")), current, "a local path is unchanged");
        assert_eq!(prefer_unc(current, Some(r"\\?\Volume{abc}\apps\tiny-skill-sync.exe")), current, "a volume path is unchanged");
        assert_eq!(prefer_unc(current, Some(r"\\server\share\apps\tiny-skill-sync.exe")), current, "only a final path is accepted");
    }

    #[test]
    fn the_program_of_a_command_is_found_the_way_windows_finds_it() {
        assert_eq!(program(r#""C:\Program Files\x\a.exe" --background"#), r"C:\Program Files\x\a.exe");
        assert_eq!(program(r#"  "C:\x\a.exe""#), r"C:\x\a.exe");
        assert_eq!(program(r#""C:\no end"#), r"C:\no end");
        assert_eq!(program(r"C:\Program Files\x\a.exe --background"), r"C:\Program Files\x\a.exe");
        assert_eq!(program(r"C:\tools\a.EXE"), r"C:\tools\a.EXE");
        assert_eq!(program(r"C:\a.executor\b.exe -x"), r"C:\a.executor\b.exe", "an .exe that does not end a word");
        assert_eq!(program("C:\\\u{00dc}n\u{00ef}code\\a.exe\t/x"), "C:\\\u{00dc}n\u{00ef}code\\a.exe");
        assert_eq!(program("rundll32 shell32,Control_RunDLL"), "rundll32");
        assert_eq!(program("   "), "");
    }

    #[test]
    fn an_odd_first_byte_in_windows_mark_means_switched_off() {
        let at = [0u8; 8];
        let mark = |first: u8| [&[first, 0, 0, 0][..], &at[..]].concat();
        assert!(!switched_off(None));
        assert!(!switched_off(Some(&[])));
        assert!(!switched_off(Some(&mark(2))));
        assert!(switched_off(Some(&mark(3))));
        assert!(!switched_off(Some(&mark(6))));
        assert!(switched_off(Some(&mark(7))));
    }

    #[test]
    fn what_the_entry_and_the_mark_mean() {
        let exe = Exe::new(LONG, Some(SHORT));
        let on = format!("\"{LONG}\" --background");
        let off = [3u8, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(Startup::decide(None, None, &exe), Startup::Off);
        assert_eq!(Startup::decide(Some("  "), None, &exe), Startup::Off, "an empty entry starts nothing");
        assert_eq!(Startup::decide(Some(&on), None, &exe), Startup::On);
        assert_eq!(Startup::decide(Some(&format!("\"{SHORT}\" --background")), None, &exe), Startup::On, "by its short name");
        assert_eq!(Startup::decide(Some(LONG), None, &exe), Startup::On, "without the option, as someone may have written it");
        assert_eq!(Startup::decide(Some(&on), Some(&off), &exe), Startup::OffInWindows);
        assert_eq!(Startup::decide(None, Some(&off), &exe), Startup::Off, "a mark left behind with no entry");
        let other = r"D:\old\tiny-skill-sync.exe";
        assert_eq!(Startup::decide(Some(&format!("\"{other}\" --background")), None, &exe), Startup::OtherCopy(other.to_string()));

        let deep = Exe::new(&path_of(246), None);
        assert_eq!(Startup::decide(None, None, &deep), Startup::TooLong);
        assert_eq!(Startup::decide(Some(&format!("\"{other}\"")), None, &deep), Startup::TooLong, "it could not take the other's place");
        assert!(!Startup::TooLong.can_change() && Startup::OffInWindows.can_change());
    }

    #[test]
    fn every_note_is_plain_ascii_and_none_starts_with_a_path() {
        let all = [Startup::Off, Startup::On, Startup::OffInWindows, Startup::OtherCopy(r"D:\old\tiny-skill-sync.exe".to_string()), Startup::TooLong];
        for state in all {
            for via in [Via::Window, Via::Tss] {
                let note = state.note(via);
                assert!(note.is_ascii() && note.starts_with(|c: char| c.is_ascii_uppercase()) && note.ends_with('.'), "{note}");
            }
            assert!(state.summary().is_ascii());
        }
        // It says how to turn it on the way that is done where it is read.
        for state in [Startup::OffInWindows, Startup::OtherCopy(r"D:\old\tiny-skill-sync.exe".to_string())] {
            let (window, tss) = (state.note(Via::Window), state.note(Via::Tss));
            assert!(window.contains("Turn it on here to start") && !window.contains("tss"), "{window}");
            assert!(tss.contains("Run 'tss startup on' to start") && !tss.contains("here"), "{tss}");
        }
        // Another program may have written the entry: its path is shown escaped, so it cannot reorder or hide text.
        let hidden = Startup::OtherCopy("D:\\\u{202e}exe.txt\\tiny-skill-sync.exe".to_string()).note(Via::Window);
        assert!(hidden.contains(r"D:\\u{202e}exe.txt") && !hidden.contains('\u{202e}'), "{hidden}");
    }

    #[test]
    fn only_windows_keys_or_keys_under_the_test_root_are_ever_written() {
        let windows = Keys::windows();
        assert!(windows.allowed(true));
        assert!(!windows.allowed(false), "not by the tests or a copy with an app folder of its own");
        let mine = Keys::choose(Some(Path::new(r"C:\t\home")));
        assert_eq!(mine, Keys::choose(Some(Path::new(r"c:\T\HOME"))), "the same folder whatever its case");
        assert!(mine.run.starts_with(r"Software\tiny-skill-sync-test\") && mine.run.ends_with(r"\Run"));
        assert!(mine.approved.ends_with(r"\StartupApproved\Run"));
        assert!(mine.allowed(false) && mine.allowed(true));
        assert_ne!(mine, Keys::choose(Some(Path::new(r"C:\t\other"))));
        assert_eq!(Keys::choose(None), windows);

        let keys = |run: &str, approved: &str| Keys { run: run.to_string(), approved: approved.to_string() };
        let refused = [
            keys(r"Software\tiny-skill-sync-testX\a\Run", r"Software\tiny-skill-sync-test\a\StartupApproved\Run"),
            keys(r"Software\tiny-skill-sync-test\", r"Software\tiny-skill-sync-test\a"),
            keys(r"Software\tiny-skill-sync-test", r"Software\tiny-skill-sync-test\a"),
            keys(&windows.run, &mine.approved),
            keys(&mine.run, &windows.approved),
            keys(r"Software\Microsoft\Windows\CurrentVersion\RunOnce", &windows.approved),
            keys("", ""),
        ];
        for k in refused {
            assert!(!k.allowed(true), "{k:?}");
        }
        assert!(keys(&windows.run.to_uppercase(), &windows.approved.to_lowercase()).allowed(true), "registry keys have no case");
    }
}
