//! What the main window shows, built as [`Surface`]s from what the app knows. Nothing here calls Windows: text is
//! measured through a [`Fit`] the window passes in, so every layout and every sentence is tested without a screen.
//!
//! Text that came from a repository (names, paths, messages) is shown through `printable`, so it cannot reorder or
//! hide what is around it. Which repository, skill or file a control is for is said by its id (see [`hit`]), never by
//! its text.

use std::collections::{HashMap, HashSet};

use windows_sys::Win32::Graphics::Gdi::{DT_CENTER, DT_EDITCONTROL, DT_END_ELLIPSIS, DT_PATH_ELLIPSIS, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK};

use super::control::{Deco, Item, Kind, Look, Mark, RowCmd, Surface, two_lines};
use super::paint::{Font, Rect, Tone, glyph, hill};
use super::sentence;
use super::theme::Role;
use crate::app::{DraftStage, SourceDraft, duplicate_source_message, preview_syncs_for_settings};
use crate::autostart::{Startup, Via};
use crate::board::{self, Action, Board, KindView, Level, Row, SourceView, ago, repo_name};
use crate::config::{Config, RepoConfig, UpdateInterval, UpdatePrefs, UpdateTiming};
use crate::diff::{Body, FileChange, Presence, Row as Line, TextDiff};
use crate::engine::DiffReport;
use crate::interval::{Interval, UnixSecs};
use crate::location::{Location, Target};
use crate::pathsafe::{printable, printable_lines};
use crate::source::SourceKind;
use crate::sources::{GlobalSettings, SourceConfig, validate_ignore_names};
use crate::updater::Action as AppAction;

/// Measures text in DIPs: (width, height), on one line when the width given is 0, else wrapped at it.
pub type Fit<'a> = &'a dyn Fn(&str, Font, i32) -> (i32, i32);

pub const NAV_W: i32 = 248;
pub const MIN_LIST_W: i32 = 160;
const MIN_PAGE_W: i32 = 360;
const SPLITTER_W: i32 = 12;
pub const HERO_H: i32 = 72;
pub const SYNC_NOW: &str = "Sync now";
pub const SYNC_TIP: &str = "Checks every enabled source now. Items you edited or deleted are never replaced without asking.";
pub const CUSTOM: &str = "Custom…";
pub const PRESETS: &[&str] = &["30 minutes", "1 hour", "6 hours", "12 hours", "1 day", "1 week", "1 month"];
const PAD_X: i32 = 24;
const PAD_TOP: i32 = 18;
const GAP: i32 = 12;
const ONE_LINE: u32 = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS;
const WRAP: u32 = DT_WORDBREAK | DT_EDITCONTROL;
const COMBO_W: i32 = 180;
const TOGGLE_W: i32 = 92;
const ROW_H: i32 = 42;
const LINE_H: i32 = 20;
/// More lines of one file than this are not shown: the diff is for deciding, not for reading a whole file.
const MAX_LINES: usize = 2_000;
/// A line is cut after this many characters; the window is never wider.
const MAX_LINE_CHARS: usize = 400;

/// The ids of the controls. Ranges hold one id per repository, skill, note, file or folded run of lines.
pub mod id {
    pub const SYNC: i32 = 200;
    pub const HERO_TITLE: i32 = 201;
    pub const HERO_SUB: i32 = 202;
    pub const NAV_ADD: i32 = 300;
    pub const NAV_ACTIVITY: i32 = 301;
    pub const NAV_SETTINGS: i32 = 302;
    pub const NAV_HEAD: i32 = 303;
    pub const SPLITTER: i32 = 304;
    pub const WELCOME_ADD: i32 = 305;
    pub const MORE: i32 = 310;
    pub const EVERY: i32 = 311;
    pub const ENABLED: i32 = 312;
    pub const RETRY: i32 = 313;
    pub const OPEN_SETTINGS: i32 = 314;
    pub const CONFIRM_SOURCE: i32 = 315;
    pub const CANCEL_SOURCE: i32 = 316;
    pub const RETRY_SOURCE: i32 = 317;
    pub const EDIT_SOURCE: i32 = 318;
    pub const APPROVE_SOURCE: i32 = 319;
    pub const DEFAULT_EVERY: i32 = 320;
    pub const OPEN_SKILLS: i32 = 321;
    pub const OPEN_BACKUPS: i32 = 322;
    pub const SAVE_TOKEN: i32 = 323;
    pub const DELETE_TOKEN: i32 = 324;
    pub const STARTUP: i32 = 325;
    /// Plus the index of the type (skills, agents, instructions, plugins): a source's "What to sync" choice.
    pub const SOURCE_TYPE: i32 = 326;
    pub const BACK: i32 = 330;
    /// Plus the index of the action in the skill's row.
    pub const DIFF_ACTION: i32 = 333;
    pub const TITLE: i32 = 400;
    pub const PRIVATE: i32 = 401;
    pub const SUB: i32 = 402;
    pub const INFO_LEAD: i32 = 403;
    pub const INFO_TEXT: i32 = 404;
    pub const REPO_ITEMS: i32 = 405;
    pub const FOLDER_RULES: i32 = 406;
    pub const FOLDER_RULES_TEXT: i32 = 407;
    pub const SKILLS_DIR: i32 = 410;
    pub const TOKEN_TEXT: i32 = 411;
    pub const EMPTY: i32 = 412;
    pub const STARTUP_TEXT: i32 = 413;
    /// Plus the index of the type: whether it syncs by default, in Settings.
    pub const GLOBAL_TYPE: i32 = 414;
    pub const GLOBAL_REPO_ITEMS: i32 = 418;
    pub const IGNORED_EDIT: i32 = 419;
    pub const IGNORED_TEXT: i32 = 429;
    pub const SOURCES_HEADING: i32 = 430;
    pub const APP_HEADING: i32 = 420;
    pub const APP_STATUS: i32 = 421;
    pub const APP_CHECK: i32 = 422;
    pub const APP_INSTALL: i32 = 423;
    pub const APP_AUTO: i32 = 424;
    pub const APP_EVERY: i32 = 425;
    pub const APP_WHEN: i32 = 426;
    pub const APP_TIMING_TEXT: i32 = 427;
    pub const APP_STATUS_HELP: i32 = 428;
    pub const PREVIEW_NOTE_TEXT: i32 = 429;
    pub const SECTION_WHAT: i32 = 470;
    pub const SECTION_KIND: i32 = 471;
    pub const SECTION_NEVER: i32 = 475;
    /// Plus the index of the repository in the settings.
    pub const NAV_REPO: i32 = 1_000;
    pub const NOTE: i32 = 100_000;
    pub const FILE: i32 = 200_000;
    pub const GAP: i32 = 300_000;
    /// Plus [`STRIDE`] per skill. Child controls: 0 row; in an open repair card, 2 Show
    /// changes and 4 to 7 choices. Open folder (3), Preview (9), Copy path (10), Copy GitHub link (11) and Open
    /// on GitHub (12) retain their part numbers but are inline commands, not separate child windows.
    pub const SKILL: i32 = 1_000_000;
    pub const STRIDE: i32 = 16;
}

/// A part of a skill's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Row,
    Compare,
    Open,
    Action(usize),
    Preview,
    CopyPath,
    CopyLink,
    OnGitHub,
}

/// What a control's id stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Repo(usize),
    Skill(usize, Part),
    File(usize),
    Gap(usize),
    DiffAction(usize),
    AppUpdate(AppAction),
    Other(i32),
}

pub fn hit(n: i32) -> Hit {
    let index = |base: i32| (n - base) as usize;
    match n {
        id::SKILL.. => {
            let part = match (n - id::SKILL) % id::STRIDE {
                0 => Part::Row,
                2 => Part::Compare,
                3 => Part::Open,
                k @ 4..=7 => Part::Action(k as usize - 4),
                9 => Part::Preview,
                10 => Part::CopyPath,
                11 => Part::CopyLink,
                12 => Part::OnGitHub,
                _ => return Hit::Other(n),
            };
            Hit::Skill(index(id::SKILL) / id::STRIDE as usize, part)
        }
        id::GAP.. => Hit::Gap(index(id::GAP)),
        id::FILE.. => Hit::File(index(id::FILE)),
        id::NOTE.. => Hit::Other(n),
        id::NAV_REPO.. => Hit::Repo(index(id::NAV_REPO)),
        id::DIFF_ACTION..id::TITLE => Hit::DiffAction(index(id::DIFF_ACTION)),
        id::APP_CHECK => Hit::AppUpdate(AppAction::Check),
        id::APP_INSTALL => Hit::AppUpdate(AppAction::Install),
        _ => Hit::Other(n),
    }
}

/// The id of part `k` of the skill at `index` (see [`id::SKILL`]).
pub fn skill_id(index: usize, k: i32) -> i32 {
    id::SKILL + id::STRIDE * index as i32 + k
}

/// Which page the right side shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    Repo(String),
    Add,
    Activity,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SectionKey {
    WhatToSync,
    Kind(SourceKind),
    NeverSynced,
}

impl SectionKey {
    pub fn is_open(self, choices: &HashMap<Self, bool>, preview: bool) -> bool {
        let default = match self {
            Self::WhatToSync => preview,
            Self::Kind(_) | Self::NeverSynced => !preview,
        };
        choices.get(&self).copied().unwrap_or(default)
    }
}

/// The page to show now: the one shown while it is still there, else the first source that needs the user, else the
/// first source. `None` is the welcome page, for when there are none. A lane ID (from a check, the tray or a source
/// just added) shows its source.
pub fn settle(shown: Option<Page>, sources: &[SourceView], cfg: &Config) -> Option<Page> {
    let source_of = |id: &str| cfg.find_source(id).map(|s| s.id.as_str()).filter(|sid| sources.iter().any(|v| v.id == *sid));
    match shown {
        Some(Page::Repo(id)) if source_of(&id).is_some() => source_of(&id).map(|sid| Page::Repo(sid.to_string())),
        Some(page @ (Page::Activity | Page::Settings | Page::Add)) => Some(page),
        _ => sources.iter().find(|v| v.enabled && v.level >= Level::Attention).or(sources.first()).map(|v| Page::Repo(v.id.clone())),
    }
}

fn line_height(font: Font) -> i32 {
    match font {
        Font::Title => 28,
        Font::Caption | Font::CaptionStrong => 16,
        _ => LINE_H,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "Next check in 5 minutes" reads as "next check in 5 minutes" after a "·".
fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_lowercase().chain(chars).collect()).unwrap_or_default()
}

struct Btn {
    id: i32,
    text: String,
    kind: Kind,
    glyph: Option<char>,
    enabled: bool,
}

fn btn(id: i32, text: &str, kind: Kind, glyph: Option<char>) -> Btn {
    Btn { id, text: text.to_string(), kind, glyph, enabled: true }
}

/// Builds a surface: controls and what is drawn behind them.
struct Out<'a> {
    s: Surface,
    fit: Fit<'a>,
}

impl<'a> Out<'a> {
    fn new(fit: Fit<'a>) -> Self {
        Out { s: Surface::default(), fit }
    }

    fn item(&mut self, id: i32, at: Rect, text: impl Into<String>, look: Look, enabled: bool) {
        self.s.items.push(Item { id, at, text: text.into(), look, enabled });
    }

    /// One line that ends in "…" when it does not fit.
    fn label(&mut self, id: i32, at: Rect, text: impl Into<String>, font: Font, ink: Role) {
        self.item(id, at, text, Look::Text { font, ink, flags: ONE_LINE }, true);
    }

    /// Wrapped text at x, y, w wide. Returns its height.
    fn para(&mut self, id: i32, [x, y, w]: [i32; 3], text: impl Into<String>, font: Font, ink: Role) -> i32 {
        let text = text.into();
        let h = (self.fit)(&text, font, w.max(1)).1.max(line_height(font));
        self.item(id, [x, y, w, h], text, Look::Text { font, ink, flags: WRAP }, true);
        h
    }

    fn text(&mut self, at: Rect, text: impl Into<String>, font: Font, ink: Role, flags: u32) {
        self.s.decos.push(Deco::Text { at, text: text.into(), font, ink, flags });
    }

    fn fill(&mut self, at: Rect, radius: i32, fill: Role, border: Role) {
        self.s.decos.push(Deco::Fill { at, radius, fill, border });
    }

    fn glyph(&mut self, at: Rect, ch: char, size: u8, ink: Role) {
        self.s.decos.push(Deco::Glyph { at, ch, size, ink });
    }

    /// A card behind what was added since `mark`.
    fn card(&mut self, mark: usize, at: Rect) {
        self.s.decos.insert(mark, Deco::Fill { at, radius: 6, fill: Role::Card, border: Role::CardBorder });
    }

    fn width(&self, text: &str, font: Font) -> i32 {
        (self.fit)(text, font, 0).0
    }

    fn button_width(&self, text: &str, glyph: Option<char>) -> i32 {
        self.width(text, Font::Body) + 24 + if glyph.is_some() { 22 } else { 0 }
    }

    fn button(&mut self, b: Btn, x: i32, y: i32, h: i32) -> i32 {
        let w = self.button_width(&b.text, b.glyph);
        self.item(b.id, [x, y, w, h], b.text, Look::Button { kind: b.kind, glyph: b.glyph }, b.enabled);
        w
    }

    /// Buttons in a row from `x`, `h` high, 8 apart, going on a new row at `right`. Returns the bottom.
    fn flow(&mut self, x: i32, y: i32, right: i32, h: i32, buttons: Vec<Btn>) -> i32 {
        let (mut cx, mut cy) = (x, y);
        for b in buttons {
            let w = self.button_width(&b.text, b.glyph);
            if cx > x && cx + w > right {
                (cx, cy) = (x, cy + h + 8);
            }
            cx += self.button(b, cx, cy, h) + 8;
        }
        cy + h
    }

    /// A page title with a line under it. Returns where what follows starts.
    fn head(&mut self, w: i32, title: &str, sub: &str) -> i32 {
        let width = w - 2 * PAD_X;
        self.label(id::TITLE, [PAD_X, PAD_TOP, width, 28], title, Font::Title, Role::Text);
        let h = self.para(id::SUB, [PAD_X, PAD_TOP + 28, width], sub, Font::Caption, Role::Text2);
        PAD_TOP + 28 + h + GAP + 4
    }

    fn done(mut self, bottom: i32) -> Surface {
        self.s.height = bottom;
        self.s
    }
}

/// The band across the top: how everything is, in one sentence, and Sync now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hero {
    pub tone: Tone,
    pub glyph: char,
    pub title: String,
    pub sub: String,
}

/// `status` is what the app is doing or will do next; `news` says it is worth more than the summary (the app is busy,
/// or the user was just told something), so it replaces it. Everything is counted by source: a source's level is its
/// own, and its items are those of its types that sync.
pub fn hero_of(sources: &[SourceView], problem: Option<&str>, status: &str, news: bool, now: UnixSecs) -> Hero {
    fn kinds(v: &SourceView) -> Vec<&KindView> {
        v.kinds.iter().filter(|k| k.on).collect()
    }
    let none = sources.is_empty();
    let on: Vec<&SourceView> = sources.iter().filter(|v| v.enabled).collect();
    let checked = on.iter().filter_map(|v| v.last_check).max().map(|at| format!("checked {}", ago(now, at)));
    let sub = |head: String| -> String {
        if news {
            return status.to_string();
        }
        let next = (!status.is_empty()).then(|| lower_first(status));
        [Some(head), checked.clone(), next].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
    };
    let calm = |text: &str| if news { status.to_string() } else { text.to_string() };
    let hero = |tone, glyph, title: String, sub: String| Hero { tone, glyph, title, sub };
    let sources = |n| plural(n, "source", "sources");
    let place = |views: &[&SourceView]| match views {
        [one] => format!("In {}", printable(repo_name(&one.label))),
        many => format!("In {}", sources(many.len())),
    };
    if let Some(problem) = problem {
        return hero(Tone::Err, glyph::ERR, "Tiny Skill Sync has a problem".to_string(), calm(&sentence(&printable(problem))));
    }
    if none {
        return hero(Tone::Off, glyph::INFO, "Add a source to start".to_string(), calm("Preview skills, agents, instructions or plugins from GitHub."));
    }
    if on.is_empty() {
        return hero(Tone::Off, glyph::PAUSE, "Every repository is paused".to_string(), calm("Turn on Keep in sync for one to check it again."));
    }
    let unreadable: Vec<&SourceView> = on.iter().copied().filter(|v| v.level == Level::Error && v.detail.is_some()).collect();
    if !unreadable.is_empty() {
        let title = format!("{} can’t be read", sources(unreadable.len()));
        return hero(Tone::Err, glyph::ERR, title, sub(place(&unreadable)));
    }
    let needs = |row: &Row| row.level >= Level::Attention;
    let needy: Vec<&KindView> = on.iter().flat_map(|v| kinds(v)).filter(|k| k.rows.iter().any(needs)).collect();
    let count: usize = needy.iter().map(|k| k.rows.iter().filter(|r| needs(r)).count()).sum();
    if count > 0 {
        let with: Vec<&SourceView> = on.iter().copied().filter(|v| kinds(v).iter().any(|k| k.rows.iter().any(needs))).collect();
        let failed = needy.iter().any(|k| k.rows.iter().any(|r| r.level == Level::Error));
        let (one, many) = board::item_nouns(needy.iter().map(|k| k.kind));
        let title = if count == 1 { format!("1 {one} needs your attention") } else { format!("{count} {many} need your attention") };
        let (tone, mark) = if failed { (Tone::Err, glyph::ERR) } else { (Tone::Warn, glyph::WARN) };
        return hero(tone, mark, title, sub(place(&with)));
    }
    let empty: Vec<&SourceView> = on.iter().copied().filter(|v| v.level >= Level::Attention).collect();
    if !empty.is_empty() {
        let title = match empty.len() {
            1 => "1 source needs your attention".to_string(),
            n => format!("{n} sources need your attention"),
        };
        return hero(Tone::Warn, glyph::WARN, title, sub(place(&empty)));
    }
    let pending = on.iter().filter(|v| v.last_check.is_none()).count();
    if pending > 0 {
        let title = if pending == on.len() { "Waiting for the first check".to_string() } else { format!("Waiting to check {}", sources(pending)) };
        return hero(Tone::Off, glyph::SYNC, title, sub(String::new()));
    }
    let present: Vec<&KindView> = on.iter().flat_map(|v| kinds(v)).collect();
    if present.is_empty() && on.iter().any(|v| !v.kinds.is_empty()) {
        return hero(Tone::Off, glyph::PAUSE, "Nothing is being synced".to_string(), sub("Sync is off for every type found".to_string()));
    }
    let skills: usize = present.iter().map(|k| k.rows.len()).sum();
    let (one, many) = board::item_nouns(present.iter().map(|k| k.kind));
    let head = format!("{} from {}", plural(skills, one, many), sources(on.len()));
    hero(Tone::Ok, glyph::OK, format!("All {many} are up to date"), sub(head))
}

/// The top of something `h` high, centred in the band.
fn in_band(h: i32) -> i32 {
    (HERO_H - h) / 2
}

pub fn hero(h: &Hero, can_sync: bool, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    // A status is marked as in the list: the same dot, with a ring that sets it off from the band. The calm states
    // (Off) keep their own pair of colours.
    let (fill, ink) = if h.tone == Tone::Off { (h.tone.bg(), h.tone.ink()) } else { (h.tone.dot(), h.tone.dot_ink()) };
    let mark = [24, in_band(40), 40, 40];
    o.fill(mark, 20, fill, Role::BandDotRing);
    o.glyph(mark, h.glyph, 18, ink);
    let bw = o.button_width(SYNC_NOW, Some(glyph::SYNC));
    let bx = w - PAD_X - bw;
    o.button(Btn { enabled: can_sync, ..btn(id::SYNC, SYNC_NOW, Kind::BandAccent, Some(glyph::SYNC)) }, bx, in_band(32), 32);
    let (tw, top) = ((bx - 16 - 78).max(0), in_band(28 + 16));
    let taken = [[78, top, o.width(&h.title, Font::Title).min(tw), 28], [78, top + 28, o.width(&h.sub, Font::Caption).min(tw), 16], [bx, in_band(32), bw, 32]];
    o.s.decos.extend(stations(w, &taken));
    o.label(id::HERO_TITLE, [78, top, tw, 28], &h.title, Font::Title, Role::BandText);
    o.label(id::HERO_SUB, [78, top + 28, tw, 16], &h.sub, Font::Caption, Role::BandCaption);
    o.done(HERO_H)
}

fn source_mark(v: &SourceView) -> Mark {
    match v.level {
        _ if !v.enabled => Mark::Dot(Tone::Off),
        Level::Error => Mark::Dot(Tone::Err),
        Level::Attention => Mark::Dot(Tone::Warn),
        Level::Ok if v.last_check.is_none() => Mark::Glyph(glyph::SYNC, Role::Text2),
        Level::Ok => Mark::Dot(Tone::Ok),
    }
}

/// The icon of a type, in the list and on a source's page.
pub fn kind_glyph(kind: SourceKind) -> char {
    match kind {
        SourceKind::Skills => glyph::SKILL,
        SourceKind::Agents => glyph::AGENT,
        SourceKind::Instructions => glyph::INSTRUCTION,
        SourceKind::Plugins => glyph::PLUGIN,
    }
}

/// The branch the last listing read, else the one set, else "default branch".
fn branch_text(v: &SourceView) -> String {
    let branch = v.seen_branch.as_deref().filter(|b| !b.is_empty()).unwrap_or(&v.branch);
    if branch.is_empty() { "default branch".to_string() } else { printable(branch).into_owned() }
}

fn folder_text(v: &SourceView) -> String {
    if v.folder.is_empty() { "whole repository".to_string() } else { printable(&v.folder).into_owned() }
}

/// What the list shows beside a type's icon ("3", or "off" when it does not sync), and whether it needs the user.
pub fn kind_badge(k: &KindView) -> (String, bool) {
    if !k.on {
        return ("off".to_string(), false);
    }
    let n = k.found.map_or(k.rows.len(), |n| n as usize);
    let warn = k.rows.iter().any(|r| r.level >= Level::Attention || r.actions.contains(&Action::ApprovePlugin));
    (n.to_string(), warn)
}

/// The window text of a source's entry, which a screen reader reads: "owner/repo, default branch, whole repository.
/// Skills 3, Instructions: Sync off. Up to date".
pub fn nav_name(v: &SourceView) -> String {
    let mut text = format!("{}, {}, {}.", printable(&v.repo), branch_text(v), folder_text(v));
    if !v.kinds.is_empty() {
        text.push(' ');
        text.push_str(&v.kinds.iter().map(KindView::summary).collect::<Vec<_>>().join(", "));
        text.push('.');
    }
    text.push(' ');
    text.push_str(&printable(&v.status));
    text
}

/// Where the text of a source's entry starts, from its left edge, and how far from the right edge it stops.
const ENTRY_LEFT: i32 = 40;
const ENTRY_RIGHT: i32 = 8;
/// The one height of every source's entry: the repository, and under it the types and the status.
const ENTRY_H: i32 = 56;
const TITLE_TOP: i32 = 5;
const TITLE_H: i32 = 20;
const CHIP_TOP: i32 = 27;
const CHIP_H: i32 = 20;
const CHIP_GAP: i32 = 4;
const CHIP_PAD: i32 = 6;
const CHIP_ICON: i32 = 12;
/// The least room, with the space after it, that a shortened owner has; with less the owner goes and the repository
/// gets the room.
const MIN_OWNER: i32 = 48;

/// How a type's chip looks on the second line of an entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChipLook {
    On,
    /// Something in this type needs the user.
    Warn,
    /// The type does not sync: no fill, and fainter.
    Off,
}

/// One type on the second line: its icon and count, or the "+2" that stands for the ones with no room.
struct Chip {
    icon: Option<char>,
    label: String,
    label_w: i32,
    look: ChipLook,
    width: i32,
}

impl Chip {
    fn new(icon: Option<char>, label: String, look: ChipLook, fit: Fit) -> Self {
        let label_w = fit(&label, Font::Caption, 0).0 + 2;
        let width = 2 * CHIP_PAD + icon.map_or(0, |_| CHIP_ICON + 3) + label_w + if look == ChipLook::Warn { 3 + CHIP_ICON } else { 0 };
        Self { icon, label, label_w, look, width }
    }

    fn draw(self, x: i32, parts: &mut Vec<Deco>) {
        let (fill, border, icon_ink, ink) = match self.look {
            ChipLook::On => (Role::Ctl, Role::CtlBorder, Role::Text2, Role::Text),
            ChipLook::Warn => (Role::WarnBg, Role::CtlBorder, Role::Warn, Role::Warn),
            ChipLook::Off => (Role::None, Role::None, Role::Text3, Role::Text3),
        };
        if fill != Role::None {
            parts.push(Deco::Fill { at: [x, CHIP_TOP, self.width, CHIP_H], radius: CHIP_H / 2, fill, border });
        }
        let mut left = x + CHIP_PAD;
        if let Some(ch) = self.icon {
            parts.push(Deco::Glyph { at: [left, CHIP_TOP, CHIP_ICON, CHIP_H], ch, size: 12, ink: icon_ink });
            left += CHIP_ICON + 3;
        }
        parts.push(Deco::Text { at: [left, CHIP_TOP, self.label_w, CHIP_H], text: self.label, font: Font::Caption, ink, flags: ONE_LINE });
        if self.look == ChipLook::Warn {
            parts.push(Deco::Glyph { at: [left + self.label_w + 3, CHIP_TOP, CHIP_ICON, CHIP_H], ch: glyph::WARN, size: 12, ink });
        }
    }
}

/// The first line of an entry: the repository in strong type, and in front of it its owner, dimmed. The owner gives way
/// first: it is cut to the start of it that leaves the repository its room and one space, and ends in an ellipsis, then
/// it goes; the repository is cut last. The owner is cut here, not by the painter, so the space after the letters that
/// are drawn is the only gap there is.
fn entry_title(parts: &mut Vec<Deco>, repo: &str, top: i32, room: i32, fit: Fit) {
    let line = |x, width, text: &str, font, ink| Deco::Text { at: [ENTRY_LEFT + x, top, width, TITLE_H], text: text.to_string(), font, ink, flags: ONE_LINE };
    let Some((owner, name)) = repo.split_once('/').filter(|(owner, name)| !owner.is_empty() && !name.is_empty()) else {
        parts.push(line(0, room, repo, Font::Strong, Role::Text));
        return;
    };
    let name_w = fit(name, Font::Strong, 0).0 + 1;
    let slashed = format!("{owner}/");
    let slashed_w = fit(&slashed, Font::Body, 0).0 + 1;
    if slashed_w + name_w <= room {
        parts.push(line(0, slashed_w, &slashed, Font::Body, Role::Text2));
        parts.push(line(slashed_w, room - slashed_w, name, Font::Strong, Role::Text));
        return;
    }
    let space = fit(" ", Font::Body, 0).0;
    let owner_room = room - name_w;
    let cut = if owner_room >= MIN_OWNER { shortened(owner, owner_room - space, fit) } else { None };
    match cut {
        Some((cut, cut_w)) => {
            parts.push(line(0, cut_w + 1, &cut, Font::Body, Role::Text2));
            parts.push(line(cut_w + space, room - cut_w - space, name, Font::Strong, Role::Text));
        }
        None => parts.push(line(0, room, name, Font::Strong, Role::Text)),
    }
}

/// The longest start of an owner's name that, with an ellipsis after it, is at most `width` wide, and how wide that is.
/// None when no start of it fits.
fn shortened(owner: &str, width: i32, fit: Fit) -> Option<(String, i32)> {
    let starts: Vec<&str> = owner.char_indices().skip(1).map(|(end, _)| &owner[..end]).collect();
    let dotted = |start: &str| format!("{start}…");
    let fitting = starts.partition_point(|&start| fit(&dotted(start), Font::Body, 0).0 <= width);
    let cut = dotted(starts[fitting.checked_sub(1)?]);
    let cut_w = fit(&cut, Font::Body, 0).0;
    Some((cut, cut_w))
}

/// The second line of an entry: a chip for each type, as many as fit, and the status at the right. The status keeps its
/// whole width and the chips give way to it: first those that are off, then those as they should be, last those that
/// need the user. A "+2" chip stands for the types with no room (it warns when one of them does).
fn entry_detail(parts: &mut Vec<Deco>, v: &SourceView, status: Option<(String, Role)>, room: i32, fit: Fit) {
    let chips: Vec<Chip> = v
        .kinds
        .iter()
        .map(|k| {
            let (label, warn) = kind_badge(k);
            let look = match (k.on, warn) {
                (false, _) => ChipLook::Off,
                (true, true) => ChipLook::Warn,
                (true, false) => ChipLook::On,
            };
            Chip::new(Some(kind_glyph(k.kind)), label, look, fit)
        })
        .collect();
    let mut order: Vec<usize> = (0..chips.len()).collect();
    order.sort_by_key(|&i| match chips[i].look {
        ChipLook::Warn => 0,
        ChipLook::On => 1,
        ChipLook::Off => 2,
    });
    let status_w = status.as_ref().map_or(0, |(text, _)| (fit(text, Font::Caption, 0).0 + 2).min(room));
    let limit = if status_w > 0 { room - status_w - CHIP_GAP } else { room };
    let more = |kept: usize| {
        let look = if order[kept..].iter().any(|&i| chips[i].look == ChipLook::Warn) { ChipLook::Warn } else { ChipLook::On };
        Chip::new(None, format!("+{}", order.len() - kept), look, fit)
    };
    let width = |kept: usize| {
        let shown = order[..kept].iter().map(|&i| chips[i].width).sum::<i32>() + CHIP_GAP * (kept as i32 - 1).max(0);
        match kept {
            0 => more(0).width,
            k if k == order.len() => shown,
            _ => shown + CHIP_GAP + more(kept).width,
        }
    };
    let kept = if chips.is_empty() { None } else { (0..=chips.len()).rev().find(|&kept| width(kept) <= limit) };
    if let Some(kept) = kept {
        let rest = (kept < chips.len()).then(|| more(kept));
        let shown = &order[..kept];
        let mut x = ENTRY_LEFT;
        for chip in chips.into_iter().enumerate().filter(|(i, _)| shown.contains(i)).map(|(_, chip)| chip).chain(rest) {
            let next = x + chip.width + CHIP_GAP;
            chip.draw(x, parts);
            x = next;
        }
    }
    if let Some((text, ink)) = status {
        let (at, flags) = if kept.is_some() {
            ([ENTRY_LEFT + room - status_w, CHIP_TOP, status_w, CHIP_H], ONE_LINE | DT_RIGHT)
        } else {
            ([ENTRY_LEFT, CHIP_TOP, room, CHIP_H], ONE_LINE)
        };
        parts.push(Deco::Text { at, text, font: Font::Caption, ink, flags });
    }
}

/// Two lines: the repository, then a chip for each type and, unless the source is simply up to date, its status (that
/// is what the dot is for). Overflow and the location stay in `nav_name`.
pub fn source_entry(v: &SourceView, w: i32, fit: Fit) -> (i32, Vec<Deco>) {
    let room = (w - ENTRY_LEFT - ENTRY_RIGHT).max(0);
    let status = (v.status != board::UP_TO_DATE_TAG).then(|| {
        let ink = match v.level {
            Level::Ok => Role::Text2,
            Level::Attention => Role::Warn,
            Level::Error => Role::Err,
        };
        (printable(&v.status).into_owned(), ink)
    });
    let mut parts = Vec::new();
    let top = if status.is_some() || !v.kinds.is_empty() { TITLE_TOP } else { (ENTRY_H - TITLE_H) / 2 };
    entry_title(&mut parts, &printable(&v.repo), top, room, fit);
    entry_detail(&mut parts, v, status, room, fit);
    (ENTRY_H, parts)
}

/// The list on the left: the sources, Add source, and at the bottom Activity and Settings.
pub fn nav(sources: &[SourceView], page: &Page, w: i32, h: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let iw = w - 16;
    o.label(id::NAV_HEAD, [16, 4, w - 32, 20], "Sources", Font::Strong, Role::Text);
    let mut y = 30;
    for (i, v) in sources.iter().enumerate() {
        let selected = *page == Page::Repo(v.id.clone());
        let (eh, parts) = source_entry(v, iw, fit);
        let look = Look::Source { mark: source_mark(v), selected, parts };
        o.item(id::NAV_REPO + i as i32, [8, y + 1, iw, eh], nav_name(v), look, true);
        y += eh + 2;
    }
    let plain = |ch, ink, selected| Look::Nav { mark: Mark::Glyph(ch, ink), ink, selected };
    o.item(id::NAV_ADD, [8, y + 1, iw, 36], "Add source", plain(glyph::ADD, Role::AccentText, *page == Page::Add), true);
    let bottom = (y + 38 + 12).max(h - 82);
    o.item(id::NAV_ACTIVITY, [8, bottom + 1, iw, 36], "Activity", plain(glyph::HISTORY, Role::Text, *page == Page::Activity), true);
    o.item(id::NAV_SETTINGS, [8, bottom + 39, iw, 36], "Settings", plain(glyph::SETTINGS, Role::Text, *page == Page::Settings), true);
    o.done(bottom + 82)
}

/// The page when no source is set up yet.
pub fn welcome(w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let sub =
        "Give a GitHub repository, branch or folder. The app finds the skills, agents, instructions and plugins in it and shows you before anything is saved.";
    let y = o.head(w, "Add your first source", sub);
    let bottom = o.flow(PAD_X, y, w - PAD_X, 32, vec![btn(id::WELCOME_ADD, "Add source", Kind::Accent, Some(glyph::ADD))]);
    o.done(bottom + 16)
}

/// "Public · the skills folder on main · checked 2 minutes ago".
pub fn source_sub(view: &SourceView, now: UnixSecs) -> String {
    let folder = if view.folder.is_empty() { "the whole repository".to_string() } else { format!("the {} folder", printable(&view.folder)) };
    let branch = match branch_text(view) {
        b if b == "default branch" => "the default branch".to_string(),
        b => b,
    };
    let checked = match view.last_check {
        Some(at) => format!("checked {}", ago(now, at)),
        None if view.level == Level::Error => "not checked successfully yet".to_string(),
        None => "not checked yet".to_string(),
    };
    if view.private {
        format!("{} on {branch} · read with your GitHub sign-in · {checked}", sentence(&folder))
    } else if view.last_check.is_some() {
        format!("Public · {folder} on {branch} · {checked}")
    } else {
        format!("{} on {branch} · {checked}", sentence(&folder))
    }
}

fn preview_sub(view: &SourceView, source: &SourceConfig) -> String {
    let folder = if source.path.is_empty() { "the whole repository".to_string() } else { format!("the {} folder", printable(&source.path)) };
    let branch = match branch_text(view) {
        b if b == "default branch" => "the default branch".to_string(),
        b => b,
    };
    if source.use_token {
        format!("{} on {branch} · read with your GitHub sign-in · not checked yet", sentence(&folder))
    } else {
        format!("{} on {branch} · not checked yet", sentence(&folder))
    }
}

/// The forms of location the Add source question takes.
pub const LOCATION_FORMS: &str = "Enter owner/name, the GitHub address of a repository, or the address of a branch or folder in it (…/tree/branch/folder). The app finds the skills, agents, instruction files and plugins in it.";

/// What the typed location means, or why it cannot be used.
pub fn location_hint(text: &str) -> Result<String, String> {
    let location = Location::parse(text).map_err(|e| sentence(&printable(&e)))?;
    let repo = printable(&format!("{}/{}", location.owner, location.name)).into_owned();
    Ok(match location.target {
        Target::Bare => format!("{repo} · the whole repository on the default branch"),
        Target::Tree(parts) => format!("{repo} · {} (GitHub tells where the branch name ends)", printable(&parts.join("/"))),
    })
}

/// Inputs for rendering an unsaved location in the same page as saved sources.
pub struct PreviewPage<'a> {
    pub draft: &'a SourceDraft,
    pub global: &'a GlobalSettings,
    pub default: Interval,
    pub now: UnixSecs,
    pub toggled: &'a HashSet<String>,
    pub sections: &'a HashMap<SectionKey, bool>,
}

/// Render an unsaved location. The draft's settings are authoritative; its preview contributes only remote facts and GitHub paths.
pub fn source_preview(p: &PreviewPage, w: i32, fit: Fit) -> Surface {
    let draft = p.draft;
    let source = &draft.settings;
    let snapshot = match &draft.stage {
        DraftStage::Reviewed(preview) => Some(preview),
        _ => draft.previous_preview.as_ref(),
    };
    let empty_view = SourceView {
        id: source.id.clone(),
        label: String::new(),
        repo: String::new(),
        branch: source.branch.clone(),
        seen_branch: None,
        folder: String::new(),
        enabled: source.enabled,
        status: String::new(),
        detail: None,
        level: Level::Ok,
        last_check: None,
        private: source.use_token,
        detected: None,
        kinds: Vec::new(),
        notes: Vec::new(),
        more_notes: 0,
    };
    let view = snapshot.map_or(&empty_view, |snapshot| snapshot.view());
    source_page(
        &SourcePage { view, source, global: p.global, default: p.default, now: p.now, toggled: p.toggled, draft: Some(draft), sections: p.sections },
        w,
        fit,
    )
}

const KINDS: [SourceKind; 4] = [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions, SourceKind::Plugins];

/// The entries of a Check every list, and the one shown. With a `default`, the first entry is "Default (…)" and stands
/// for no interval of its own.
pub fn every_choices(current: Option<Interval>, default: Option<Interval>) -> (Vec<String>, String) {
    let mut items: Vec<String> = default.map(|d| format!("Default ({})", d.describe())).into_iter().collect();
    items.extend(PRESETS.iter().map(|s| s.to_string()));
    let shown = current.map_or_else(|| items[0].clone(), Interval::describe);
    if !items.contains(&shown) {
        items.push(shown.clone());
    }
    items.push(CUSTOM.to_string());
    (items, shown)
}

/// What a Check every entry means: `None` is the default.
pub fn parse_every(text: &str) -> Result<Option<Interval>, String> {
    let text = text.trim();
    if text.is_empty() || text.to_ascii_lowercase().starts_with("default") {
        return Ok(None);
    }
    Interval::parse(text)?.validate_poll().map(Some)
}

/// The colour and mark of a skill's label.
pub fn pill_of(row: &Row) -> (Tone, Option<char>) {
    match (row.level, row.tag) {
        (_, board::EDITED_TAG) => (Tone::Warn, Some(glyph::EDIT)),
        (_, board::DELETED_TAG) => (Tone::Warn, Some(glyph::DELETE)),
        (Level::Ok, board::UP_TO_DATE_TAG | board::INSTALLED_TAG) => (Tone::Ok, Some(glyph::OK)),
        (Level::Ok, _) => (Tone::Off, None),
        (Level::Attention, _) => (Tone::Warn, Some(glyph::WARN)),
        (Level::Error, _) => (Tone::Err, Some(glyph::ERR)),
    }
}

/// Plugin choices such as Reinstall are available even when the row only reports what the user did.
pub fn expandable(row: &Row) -> bool {
    (row.plugin.is_some() || row.level >= Level::Attention) && row.actions.iter().any(|a| *a != Action::ManageAgain)
}

pub fn is_open(row: &Row, lane: &str, toggled: &HashSet<String>) -> bool {
    expandable(row) && !toggled.contains(&toggle_key(lane, &row.name))
}

/// Commands shared by the inline icons and the row's menu, in display order.
pub fn row_commands(row: &Row, kind: SourceKind, linkable: bool) -> Vec<RowCmd> {
    if kind == SourceKind::Plugins {
        return if row.listed && linkable { vec![RowCmd::CopyLink, RowCmd::OnGitHub] } else { Vec::new() };
    }
    let mut commands = Vec::new();
    if row.has_folder() {
        if kind == SourceKind::Skills {
            commands.push(RowCmd::Open);
        }
        if row.facts.is_some() {
            commands.push(RowCmd::Preview);
        }
        if kind == SourceKind::Skills {
            commands.push(RowCmd::CopyPath);
        }
    }
    if row.listed && linkable && kind == SourceKind::Skills {
        commands.extend([RowCmd::CopyLink, RowCmd::OnGitHub]);
    }
    if !expandable(row) {
        if row.can_compare() {
            commands.push(RowCmd::Compare);
        }
        if row.actions.contains(&Action::ManageAgain) {
            commands.push(RowCmd::ManageAgain);
        }
    }
    commands
}

fn action_glyph(action: Action) -> Option<char> {
    match action {
        Action::Restore => Some(glyph::UNDO),
        Action::RemoveThem => Some(glyph::DELETE),
        _ => None,
    }
}

/// The settings of one lane of a source, as the engine runs it (the same whether its type is on or off).
pub fn lane_config(source: &SourceConfig, kind: SourceKind, lane: &str) -> RepoConfig {
    RepoConfig {
        id: lane.to_string(),
        repo: source.repo.clone(),
        branch: source.branch.clone(),
        path: source.path.clone(),
        interval: source.interval,
        use_token: source.use_token,
        enabled: source.enabled,
        kind,
        source: source.id.clone(),
    }
}

/// The type and row a source's page shows at `index`: rows are numbered across its sections, in order.
pub fn row_at(view: &SourceView, index: usize) -> Option<(&KindView, &Row)> {
    view.kinds.iter().flat_map(|k| k.rows.iter().map(move |r| (k, r))).nth(index)
}

/// The index on its source's page of the row named `name` of `lane`.
pub fn row_index(view: &SourceView, lane: &str, name: &str) -> Option<usize> {
    view.kinds.iter().flat_map(|k| k.rows.iter().map(move |r| (k, r))).position(|(k, r)| k.lane == lane && r.name == name)
}

/// What remembers that a row was opened or closed: its lane and name, so rows of two types never share it.
pub fn toggle_key(lane: &str, name: &str) -> String {
    format!("{lane}\n{name}")
}

/// The commands of a row of a type. A type that is off only offers what reads the files already there.
pub fn kind_commands(row: &Row, k: &KindView, linkable: bool) -> Vec<RowCmd> {
    let mut commands = row_commands(row, k.kind, linkable);
    if !k.on {
        commands.retain(|c| matches!(c, RowCmd::Open | RowCmd::CopyPath | RowCmd::CopyLink | RowCmd::OnGitHub));
    }
    commands
}

fn on_off(on: bool) -> &'static str {
    if on { "On" } else { "Off" }
}

/// The entries of a type's "What to sync" list, and the one shown.
pub fn type_choices(value: Option<bool>, global: bool) -> (Vec<String>, String) {
    let items = vec![format!("Default ({})", on_off(global)), "On".to_string(), "Off".to_string()];
    let shown = match value {
        None => items[0].clone(),
        Some(on) => on_off(on).to_string(),
    };
    (items, shown)
}

/// The entries of the repository-specific items list, and the one shown.
pub fn repo_items_choices(value: Option<bool>, global: bool) -> (Vec<String>, String) {
    let items = vec![format!("Default ({})", if global { "include" } else { "skip" }), "Include".to_string(), "Skip".to_string()];
    let shown = match value {
        None => items[0].clone(),
        Some(true) => items[1].clone(),
        Some(false) => items[2].clone(),
    };
    (items, shown)
}

/// A change to one source's settings, chosen on its page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceEdit {
    /// `None` follows the global setting again.
    Type(SourceKind, Option<bool>),
    RepoItems(Option<bool>),
    FolderRules {
        add: Vec<String>,
        allow: Vec<String>,
    },
}

impl SourceEdit {
    pub fn apply(self, s: &mut SourceConfig) {
        match self {
            SourceEdit::Type(kind, value) => s.set_type(kind, value),
            SourceEdit::RepoItems(value) => s.set_repo_items(value),
            SourceEdit::FolderRules { add, allow } => {
                s.ignore_add = add;
                s.ignore_allow = allow;
            }
        }
    }
}

/// What choosing `value` in the "What to sync" list with id `n` changes.
pub fn source_choice(n: i32, value: &str) -> Option<SourceEdit> {
    let choice = match value {
        "On" | "Include" => Some(true),
        "Off" | "Skip" => Some(false),
        v if v.starts_with("Default") => None,
        _ => return None,
    };
    match n {
        id::REPO_ITEMS => Some(SourceEdit::RepoItems(choice)),
        _ => {
            let kind = *KINDS.get(usize::try_from(n - id::SOURCE_TYPE).ok()?)?;
            value.starts_with(['O', 'D']).then_some(SourceEdit::Type(kind, choice))
        }
    }
}

/// Folder names typed with commas between them, checked the way the settings are.
pub fn parse_folders(text: &str) -> Result<Vec<String>, String> {
    let names: Vec<String> = text.split(',').map(str::trim).filter(|n| !n.is_empty()).map(str::to_string).collect();
    validate_ignore_names(&names).map_err(|why| sentence(&why))?;
    Ok(names)
}

/// A source's folder rules from what was typed: the names it also skips and the ones it never skips.
pub fn folder_rules(add: &str, allow: &str) -> Result<SourceEdit, String> {
    let (add, allow) = (parse_folders(add)?, parse_folders(allow)?);
    if let Some(both) = add.iter().find(|a| allow.iter().any(|b| crate::pathsafe::fold(a) == crate::pathsafe::fold(b))) {
        return Err(format!("Folder '{}' cannot be both skipped and never skipped.", printable(both)));
    }
    Ok(SourceEdit::FolderRules { add, allow })
}

/// A change to the settings every source follows, chosen in Settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlobalEdit {
    Type(SourceKind, bool),
    RepoItems(bool),
    Ignore(Vec<String>),
}

impl GlobalEdit {
    pub fn apply(self, g: &mut GlobalSettings) {
        match self {
            GlobalEdit::Type(kind, on) => {
                g.set_type(kind, on);
            }
            GlobalEdit::RepoItems(on) => g.set_repo_items(on),
            GlobalEdit::Ignore(names) => g.ignore_folders = names,
        }
    }
}

/// What switching the Settings switch with id `n` to `on` changes.
pub fn global_switch(n: i32, on: bool) -> Option<GlobalEdit> {
    match n {
        id::GLOBAL_REPO_ITEMS => Some(GlobalEdit::RepoItems(on)),
        _ => KINDS.get(usize::try_from(n - id::GLOBAL_TYPE).ok()?).map(|&kind| GlobalEdit::Type(kind, on)),
    }
}

/// "Also skips: a, b. Never skips: c." for a source's folder rules.
pub fn folder_rules_text(source: &SourceConfig) -> String {
    let list = |names: &[String]| printable(&names.join(", ")).into_owned();
    match (source.ignore_add.is_empty(), source.ignore_allow.is_empty()) {
        (true, true) => "Skips the ignored folders set in Settings.".to_string(),
        (false, true) => format!("Also skips: {}.", list(&source.ignore_add)),
        (true, false) => format!("Never skips: {}.", list(&source.ignore_allow)),
        (false, false) => format!("Also skips: {}. Never skips: {}.", list(&source.ignore_add), list(&source.ignore_allow)),
    }
}

pub struct SourcePage<'a> {
    pub view: &'a SourceView,
    pub source: &'a SourceConfig,
    pub global: &'a GlobalSettings,
    pub default: Interval,
    pub now: UnixSecs,
    /// The rows the user opened or closed, by [`toggle_key`].
    pub toggled: &'a HashSet<String>,
    /// An unsaved draft uses this page's shared renderer and has only GitHub-safe row commands.
    pub draft: Option<&'a SourceDraft>,
    /// Manual expansion choices, stored separately from source settings.
    pub sections: &'a HashMap<SectionKey, bool>,
}

pub fn source_page(p: &SourcePage, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let (x, cw) = (PAD_X, w - 2 * PAD_X);
    let right = x + cw;
    let mut y = PAD_TOP;
    let preview = p.draft.is_some();
    if !preview {
        o.item(id::MORE, [right - 32, y, 32, 32], "More", Look::Button { kind: Kind::Icon, glyph: Some(glyph::MORE) }, true);
    }
    let room = right - if preview { 0 } else { 32 + 8 } - x;
    let title =
        p.draft.map_or_else(|| p.view.repo.clone(), |draft| if p.source.repo.is_empty() { draft_repo(&draft.input.text) } else { p.source.repo.clone() });
    let private = p.draft.map_or(p.view.private, |_| p.source.use_token);
    let pill_w = o.width("Private", Font::CaptionStrong) + 18 + 16;
    let title = printable(&title).into_owned();
    let title_w = (o.width(&title, Font::Title) + 2).min(room - if private { pill_w + 10 } else { 0 }).max(0);
    o.label(id::TITLE, [x, y, title_w, 28], title, Font::Title, Role::Text);
    if private {
        o.item(id::PRIVATE, [x + title_w + 10, y, pill_w, 28], "Private", Look::Pill { tone: Tone::Off, glyph: Some(glyph::LOCK) }, true);
    }
    let subtitle = p.draft.map_or_else(|| source_sub(p.view, p.now), |_| preview_sub(p.view, p.source));
    let h = o.para(id::SUB, [x, y + 28, room], subtitle, Font::Caption, Role::Text2);
    y = (y + 28 + h).max(y + 32) + GAP + 4;
    if let Some(draft) = p.draft {
        y = preview_banner(&mut o, draft, p.global, x, y, cw) + GAP;
        if matches!(draft.stage, DraftStage::AlreadySetUp(_)) {
            return o.done(y + 16);
        }
    }
    if let Some(detail) = &p.view.detail {
        y = if preview { preview_detail(&mut o, detail, x, y, cw) } else { infobar(&mut o, p.view, detail, x, y, cw) } + GAP;
    }
    y = interval_card(&mut o, p, x, y, cw) + GAP;
    y = what_to_sync(&mut o, p, x, y, cw) + GAP;
    y = sections(&mut o, p, x, y, cw);
    y = never_synced(&mut o, p, x, y, cw);
    o.done(y + 16)
}

fn draft_repo(text: &str) -> String {
    crate::location::Location::parse(text)
        .map(|location| format!("{}/{}", location.owner, location.name))
        .unwrap_or_else(|_| printable(text.trim()).into_owned())
}

/// Why a source can't be read, or why it lists nothing, with what can be done about it.
fn infobar(o: &mut Out, view: &SourceView, detail: &str, x: i32, y: i32, cw: i32) -> i32 {
    let error = view.level == Level::Error;
    let (tone, mark, lead) = if error {
        let mut failed = view.kinds.iter().filter(|k| k.level == Level::Error).peekable();
        let plugins = failed.peek().is_some() && failed.all(|k| k.kind == SourceKind::Plugins);
        let lead = if plugins { "Could not check this plugin source." } else { "Can’t read this repository." };
        (Tone::Err, glyph::ERROR_BADGE, lead.to_string())
    } else {
        (Tone::Warn, glyph::WARN, format!("{}.", printable(&view.status)))
    };
    let mark_at = o.s.decos.len();
    o.glyph([x + 16, y + 12, 16, 20], mark, 16, tone.ink());
    let (tx, tw) = (x + 44, cw - 44 - 16);
    o.label(id::INFO_LEAD, [tx, y + 12, tw, 20], lead, Font::Strong, Role::Text);
    let h = o.para(id::INFO_TEXT, [tx, y + 32, tw], printable_lines(detail), Font::Body, Role::Text);
    let mut buttons = Vec::new();
    if error {
        buttons.push(btn(id::OPEN_SETTINGS, "Open Settings", Kind::Standard, Some(glyph::SETTINGS)));
    }
    buttons.push(btn(id::RETRY, "Try again", Kind::Standard, Some(glyph::REFRESH)));
    let bottom = o.flow(tx, y + 32 + h + 10, tx + tw, 28, buttons) + 12;
    o.s.decos.insert(mark_at, Deco::Fill { at: [x, y, cw, bottom - y], radius: 6, fill: tone.bg(), border: Role::CardBorder });
    bottom
}

fn preview_detail(o: &mut Out, detail: &str, x: i32, y: i32, cw: i32) -> i32 {
    let mark = o.s.decos.len();
    o.glyph([x + 16, y + 12, 16, 20], glyph::WARN, 16, Role::Warn);
    let (tx, tw) = (x + 44, cw - 44 - 16);
    o.label(id::INFO_LEAD, [tx, y + 12, tw, 20], "Preview note", Font::Strong, Role::Text);
    let h = o.para(id::PREVIEW_NOTE_TEXT, [tx, y + 32, tw], printable_lines(detail), Font::Body, Role::Text);
    let bottom = y + 32 + h + 12;
    o.s.decos.insert(mark, Deco::Fill { at: [x, y, cw, bottom - y], radius: 6, fill: Role::WarnBg, border: Role::CardBorder });
    bottom
}

/// Check every and Keep in sync, for the whole source, on one row when they fit, else on two.
fn interval_card(o: &mut Out, p: &SourcePage, x: i32, y: i32, cw: i32) -> i32 {
    let editable = p.draft.is_none_or(|draft| !matches!(draft.stage, DraftStage::Loading | DraftStage::Saving | DraftStage::AlreadySetUp(_)));
    let (items, shown) = every_choices(p.source.interval, Some(p.default));
    let every = "Check every";
    let keep_label = "Keep in sync";
    let keep_text = if p.source.enabled { "Keep in sync: On" } else { "Keep in sync: Off" };
    let shown_text = format!("{every}\n{shown}");
    let labels = o.width(every, Font::Body).max(o.width(keep_label, Font::Body));
    let fixed = 16 + 20 + 16 + 16 + COMBO_W + 16 + 1 + 16 + 16 + TOGGLE_W + 16;
    let mark = o.s.decos.len();
    let toggle = Look::Toggle { on: p.source.enabled };
    let h = if (cw - fixed) / 2 >= labels {
        let flex = (cw - fixed) / 2;
        o.glyph([x + 16, y, 20, 56], glyph::TIMER, 16, Role::Text);
        o.text([x + 52, y, flex, 56], every, Font::Body, Role::Text, ONE_LINE);
        let cx = x + 52 + flex + 16;
        o.item(id::EVERY, [cx, y + 12, COMBO_W, 32], shown_text.clone(), Look::Combo { items }, editable);
        let sx = cx + COMBO_W + 16;
        o.fill([sx, y + 12, 1, 32], 0, Role::Divider, Role::None);
        o.text([sx + 17, y, flex, 56], keep_label, Font::Body, Role::Text, ONE_LINE);
        o.item(id::ENABLED, [x + cw - 16 - TOGGLE_W, y + 12, TOGGLE_W, 32], keep_text, toggle, editable);
        56
    } else {
        let combo = COMBO_W.min(cw - 52 - 16 - 8 - labels).max(120);
        let lw = cw - 52 - 16 - combo - 8;
        o.glyph([x + 16, y, 20, 52], glyph::TIMER, 16, Role::Text);
        o.text([x + 52, y, lw, 52], every, Font::Body, Role::Text, ONE_LINE);
        o.item(id::EVERY, [x + cw - 16 - combo, y + 10, combo, 32], shown_text, Look::Combo { items }, editable);
        o.fill([x + 1, y + 52, cw - 2, 1], 0, Role::Divider, Role::None);
        o.text([x + 52, y + 53, cw - 52 - 16 - TOGGLE_W - 8, 52], keep_label, Font::Body, Role::Text, ONE_LINE);
        o.item(id::ENABLED, [x + cw - 16 - TOGGLE_W, y + 63, TOGGLE_W, 32], keep_text, toggle, editable);
        105
    };
    o.card(mark, [x, y, cw, h]);
    y + h
}

fn preview_banner(o: &mut Out, draft: &SourceDraft, global: &GlobalSettings, x: i32, y: i32, cw: i32) -> i32 {
    let (heading, detail, actions) = match &draft.stage {
        DraftStage::Loading => {
            let refreshing = draft.previous_preview.is_some();
            (
                if refreshing { "Refreshing preview" } else { "Preview" },
                if refreshing {
                    "The previous preview stays visible while the updated folder rules are checked. Nothing has been saved or installed.".to_string()
                } else {
                    "Reading the source. Nothing has been saved or installed.".to_string()
                },
                vec![btn(id::CANCEL_SOURCE, "Cancel", Kind::Standard, None)],
            )
        }
        DraftStage::AlreadySetUp(id) => (
            "Already added",
            duplicate_source_message(id),
            vec![btn(id::EDIT_SOURCE, "Edit location", Kind::Accent, None), btn(id::CANCEL_SOURCE, "Cancel", Kind::Standard, None)],
        ),
        DraftStage::Failed(why) => (
            "Preview unavailable",
            if draft.previous_preview.is_some() {
                format!("The previous preview is shown below. The updated source could not be read: {}", printable(why))
            } else {
                format!("The source could not be read: {}", printable(why))
            },
            vec![
                btn(id::RETRY_SOURCE, "Retry", Kind::Accent, None),
                btn(id::EDIT_SOURCE, "Edit location", Kind::Standard, None),
                btn(id::CANCEL_SOURCE, "Cancel", Kind::Standard, None),
            ],
        ),
        DraftStage::Saving => ("Adding source", "Checking the preview again and saving the source…".to_string(), Vec::new()),
        DraftStage::Reviewed(preview) => {
            let syncs = KINDS.iter().any(|&kind| preview_syncs_for_settings(preview, &draft.settings, global, kind));
            let plugins = preview_syncs_for_settings(preview, &draft.settings, global, SourceKind::Plugins);
            let any_type_on = KINDS.iter().any(|&kind| type_enabled_settings(&draft.settings, global, kind));
            let detail = if !syncs {
                if any_type_on {
                    "No syncable items were found. Change the location or folder rules before adding this source.".to_string()
                } else {
                    "All item types are turned off. Turn on a type before adding this source.".to_string()
                }
            } else if plugins {
                "Preview only. Nothing has been saved or installed. Plugin installation requires the separate approval action.".to_string()
            } else {
                "Preview only. Nothing has been saved or installed. Add this source to save it and start its first check.".to_string()
            };
            let actions = if syncs {
                let mut actions = vec![btn(id::CONFIRM_SOURCE, "Add and keep in sync", Kind::Accent, None)];
                if plugins {
                    actions.push(btn(id::APPROVE_SOURCE, "Add and approve plugins", Kind::Standard, None));
                }
                actions.push(btn(id::CANCEL_SOURCE, "Cancel", Kind::Standard, None));
                actions
            } else {
                vec![
                    btn(id::EDIT_SOURCE, "Edit location", Kind::Accent, None),
                    btn(id::RETRY_SOURCE, "Retry", Kind::Standard, None),
                    btn(id::CANCEL_SOURCE, "Cancel", Kind::Standard, None),
                ]
            };
            ("Preview", detail, actions)
        }
    };
    let mark = o.s.decos.len();
    o.text([x + 16, y + 12, cw - 32, 24], heading, Font::Strong, Role::Text, ONE_LINE);
    let buttons_y = y + 12 + 24 + 6;
    let bottom = o.flow(x + 16, buttons_y, x + cw - 16, 32, actions);
    let body_y = bottom + 8;
    let body_h = o.para(id::INFO_TEXT, [x + 16, body_y, cw - 32], printable(&detail).into_owned(), Font::Caption, Role::Text2);
    let bottom = body_y + body_h + 12;
    o.card(mark, [x, y, cw, bottom - y]);
    bottom
}

/// Which types the source syncs, whether it takes repository-specific items, and its folder rules.
fn what_to_sync(o: &mut Out, p: &SourcePage, x: i32, y: i32, cw: i32) -> i32 {
    let open = SectionKey::WhatToSync.is_open(p.sections, p.draft.is_some());
    let marks = KINDS.into_iter().map(|kind| (kind_glyph(kind), if type_enabled(p, kind) { Role::Text } else { Role::Text3 })).collect::<Vec<_>>();
    let states =
        KINDS.into_iter().map(|kind| format!("{} {}", kind.heading(), if type_enabled(p, kind) { "on" } else { "off" })).collect::<Vec<_>>().join("; ");
    let look = Look::Section { title: "What to sync".to_string(), icon: glyph::SYNC, open, marks, warn: false };
    o.item(id::SECTION_WHAT, [x, y, cw, 28], format!("What to sync, {}. {states}", expanded_text(open)), look, true);
    if !open {
        return y + 32;
    }
    let (mark, top) = (o.s.decos.len(), y + 32);
    let mut ly = top + 8;
    let mut rows = KINDS
        .into_iter()
        .enumerate()
        .map(|(i, kind)| {
            let choices = type_choices(p.source.types.get(kind), p.global.types.enabled(kind));
            (kind_glyph(kind), kind.heading(), id::SOURCE_TYPE + i as i32, choices, format!("Sync {}", kind.nouns().1))
        })
        .collect::<Vec<_>>();
    let title = "Repository-specific skills and agents";
    rows.push((glyph::FOLDER, title, id::REPO_ITEMS, repo_items_choices(p.source.include_repo_items, p.global.include_repo_items), title.to_string()));
    // One layout for every list: wide enough for its longest choice (a combo paints its value in its width less 44),
    // beside the titles when all of them fit, else under each title.
    let room = cw - 52 - 16;
    let longest = rows.iter().flat_map(|r| r.3.0.iter()).map(|c| o.width(c, Font::Body) + 44).max().unwrap_or(0);
    let combo = longest.max(COMBO_W).min(room).max(0);
    let titles = rows.iter().map(|r| o.width(r.1, Font::Body) + 2).max().unwrap_or(0);
    let beside = room - combo - 8 >= titles;
    for (kind_mark, title, n, (items, shown), purpose) in rows {
        let text = two_lines(&purpose, &shown);
        if beside {
            o.glyph([x + 16, ly, 20, 44], kind_mark, 16, Role::Text);
            o.text([x + 52, ly, room - combo - 8, 44], title, Font::Body, Role::Text, ONE_LINE);
            o.item(
                n,
                [x + cw - 16 - combo, ly + 6, combo, 32],
                text,
                Look::Combo { items },
                p.draft.is_none_or(|d| !matches!(d.stage, DraftStage::Loading | DraftStage::Saving | DraftStage::AlreadySetUp(_))),
            );
            ly += 44;
        } else {
            let one = o.width(title, Font::Body) + 2 <= room;
            let th = if one { line_height(Font::Body) } else { (o.fit)(title, Font::Body, room.max(1)).1.max(line_height(Font::Body)) };
            o.glyph([x + 16, ly + 8, 20, line_height(Font::Body)], kind_mark, 16, Role::Text);
            o.text([x + 52, ly + 8, room, th], title, Font::Body, Role::Text, if one { ONE_LINE } else { WRAP });
            o.item(
                n,
                [x + 52, ly + 8 + th + 4, combo, 32],
                text,
                Look::Combo { items },
                p.draft.is_none_or(|d| !matches!(d.stage, DraftStage::Loading | DraftStage::Saving | DraftStage::AlreadySetUp(_))),
            );
            ly += 8 + th + 4 + 32 + 4;
        }
    }
    if !beside {
        ly += 4;
    }
    let b = btn(id::FOLDER_RULES, "Folder rules…", Kind::Standard, None);
    let bw = o.button_width(&b.text, b.glyph);
    let beside = cw - 52 - 16 - bw - 8;
    let tw = if beside >= 120 { beside } else { cw - 52 - 16 };
    o.glyph([x + 16, ly + 8, 20, 20], glyph::FILE, 16, Role::Text);
    o.text([x + 52, ly + 8, tw, 20], "Folder rules", Font::Body, Role::Text, ONE_LINE);
    let dh = o.para(id::FOLDER_RULES_TEXT, [x + 52, ly + 28, tw], folder_rules_text(p.source), Font::Caption, Role::Text2);
    let bottom = if beside >= 120 {
        o.button(b, x + cw - 16 - bw, ly + 8, 32);
        (ly + 28 + dh).max(ly + 40) + 12
    } else {
        o.button(b, x + 52, ly + 28 + dh + 8, 32);
        ly + 28 + dh + 8 + 32 + 12
    };
    o.card(mark, [x, top, cw, bottom - top]);
    bottom
}

/// The items of a source, by type in the order skills, agents, instructions, plugins: each with its icon, count and
/// rows, then what its folder rules skipped and what is never synced, with the reason.
fn sections(o: &mut Out, p: &SourcePage, x: i32, mut y: i32, cw: i32) -> i32 {
    let v = p.view;
    if v.kinds.is_empty() {
        if p.draft.is_some() {
            return y - GAP;
        }
        let text = if !v.enabled {
            "Nothing from this source is kept up to date while Keep in sync is off."
        } else if v.detail.is_some() {
            return y - GAP;
        } else if v.last_check.is_none() {
            "What it holds is listed here after the first check."
        } else {
            "Nothing to sync was found here."
        };
        return y + o.para(id::EMPTY, [x, y, cw], text, Font::Body, Role::Text2);
    }
    let mut index = 0;
    for k in &v.kinds {
        let open = SectionKey::Kind(k.kind).is_open(p.sections, p.draft.is_some());
        let detected = v.detected.map(|found| found[kind_index(k.kind)]).unwrap_or(k.found.unwrap_or(k.rows.len() as u32));
        let on = kind_is_on(p, k);
        let count = if on { detected.to_string() } else { format!("{detected} · Sync off") };
        let heading = format!("{} ({count})", k.kind.heading());
        let warn = !open && on && kind_badge(k).1;
        let text = if warn { format!("{heading}, collapsed. Needs your attention.") } else { format!("{heading}, {}.", expanded_text(open)) };
        let look = Look::Section { title: heading, icon: kind_glyph(k.kind), open, marks: Vec::new(), warn };
        o.item(id::SECTION_KIND + kind_index(k.kind) as i32, [x, y, cw, 28], text, look, true);
        y += 32;
        let first = index;
        index += k.rows.len();
        if !open {
            y += GAP;
            continue;
        }
        if !k.rows.is_empty() {
            y = rows_card(o, p, k, first, x, y, cw) + 8;
        } else if on && k.notes.is_empty() && k.filtered == 0 {
            let text = format!("Its {} are listed here after the next check.", k.kind.nouns().1);
            y += o.para(0, [x, y, cw], text, Font::Caption, Role::Text2) + 8;
        }
        if k.filtered > 0 {
            y += o.para(0, [x, y, cw], format!("{} skipped by folder rules", k.filtered), Font::Caption, Role::Text2) + 4;
        }
        for note in &k.notes {
            let text = format!("Not synced: {}: {}", printable(&note.path), printable(&note.reason));
            y += o.para(0, [x, y, cw], wrap_report(text, cw, Font::Caption, o.fit), Font::Caption, Role::Text2) + 4;
        }
        y += GAP;
    }
    y - GAP
}

/// Files the listing had that are never synced, such as AGENTS.md.
fn never_synced(o: &mut Out, p: &SourcePage, x: i32, mut y: i32, cw: i32) -> i32 {
    let v = p.view;
    if v.notes.is_empty() {
        return y;
    }
    y += GAP;
    let open = SectionKey::NeverSynced.is_open(p.sections, p.draft.is_some());
    let count = v.notes.len() + v.more_notes as usize;
    let heading = format!("Never synced ({count})");
    let text = format!("{heading}, {}.", expanded_text(open));
    o.item(id::SECTION_NEVER, [x, y, cw, 28], text, Look::Section { title: heading, icon: glyph::INFO, open, marks: Vec::new(), warn: false }, true);
    y += 32;
    if !open {
        return y;
    }
    for note in &v.notes {
        let text = format!("Never synced: {}: {}", printable(&note.path), printable(&note.reason));
        y += o.para(0, [x, y, cw], wrap_report(text, cw, Font::Caption, o.fit), Font::Caption, Role::Text2) + 4;
    }
    if v.more_notes > 0 {
        y += o.para(0, [x, y, cw], format!("and {} more", v.more_notes), Font::Caption, Role::Text2) + 4;
    }
    y
}

fn expanded_text(open: bool) -> &'static str {
    if open { "expanded" } else { "collapsed" }
}

fn type_enabled(p: &SourcePage, kind: SourceKind) -> bool {
    type_enabled_settings(p.source, p.global, kind)
}

fn kind_is_on(p: &SourcePage, kind: &KindView) -> bool {
    p.draft.map_or(kind.on, |_| type_enabled(p, kind.kind))
}

fn type_enabled_settings(source: &SourceConfig, global: &GlobalSettings, kind: SourceKind) -> bool {
    source.types.get(kind).unwrap_or_else(|| global.types.enabled(kind))
}

fn kind_index(kind: SourceKind) -> usize {
    KINDS.iter().position(|&item| item == kind).expect("all source kinds are listed in KINDS")
}

/// The rows of one type, each with its status and, once opened, what can be done about it. `first` is the index of
/// its first row on the page.
fn rows_card(o: &mut Out, p: &SourcePage, k: &KindView, first: usize, x: i32, mut y: i32, cw: i32) -> i32 {
    let (mark, top) = (o.s.decos.len(), y);
    for (j, row) in k.rows.iter().enumerate() {
        if j > 0 {
            o.fill([x + 1, y, cw - 2, 1], 0, Role::Divider, Role::None);
        }
        let base = skill_id(first + j, 0);
        let (tone, pill) = pill_of(row);
        let open = is_open(row, &k.lane, p.toggled);
        let look = Look::Row { tone, glyph: pill, commands: source_row_commands(p, k, row), disclosure: expandable(row).then_some(open) };
        let facts = match &row.plugin {
            Some(plugin) => printable(&plugin.facts()).into_owned(),
            None => row.facts.as_ref().map(|f| printable(&board::facts_text(f, p.now)).into_owned()).unwrap_or_default(),
        };
        let explanation = if !row.plain() || row.facts.is_none() { printable_lines(&row.text) } else { "".into() };
        let text = format!("{}\n{}\n{facts}\n{explanation}", printable(&row.name), row.tag);
        o.item(base, [x + 1, y, cw - 2, ROW_H], text, look, true);
        y += ROW_H;
        let (tx, tw) = (x + 44, cw - 44 - 16);
        if row.plugin.is_some() {
            let mut details = facts;
            if row.tag != row.text {
                details.push('\n');
                details.push_str(&explanation);
            }
            y += o.para(base + 8, [tx, y, tw], wrap_report(details, tw, Font::Caption, o.fit), Font::Caption, Role::Text2) + 6;
        }
        if open {
            y = skill_card(o, row, base, [tx, y, tw]) + 6;
        }
        y += 4;
    }
    o.card(mark, [x, top, cw, y - top]);
    y
}

fn source_row_commands(p: &SourcePage, k: &KindView, row: &Row) -> Vec<RowCmd> {
    let lane = lane_config(p.source, k.kind, &k.lane);
    if p.draft.is_some() {
        return row
            .remote_path
            .as_deref()
            .filter(|path| crate::github::web_url_path(&lane, path).is_some())
            .map(|_| vec![RowCmd::CopyLink, RowCmd::OnGitHub])
            .unwrap_or_default();
    }
    kind_commands(row, k, crate::github::web_url(&lane, None).is_some())
}

/// An open card contains just the comparison and repair choices. Returns the bottom.
fn skill_card(o: &mut Out, row: &Row, base: i32, [x, y, w]: [i32; 3]) -> i32 {
    let mut choices = Vec::new();
    if row.can_compare() {
        let kind = if row.actions.contains(&Action::Overwrite) { Kind::Accent } else { Kind::Standard };
        choices.push(btn(base + 2, "Show changes", kind, Some(glyph::FILE)));
    }
    for (k, action) in row.actions.iter().enumerate() {
        choices.push(btn(base + 4 + k as i32, action.label(), Kind::Standard, action_glyph(*action)));
    }
    o.flow(x, y, x + w, 28, choices)
}

pub struct SettingsPage<'a> {
    pub default: Interval,
    pub skills_dir: &'a str,
    pub token_saved: bool,
    /// Whether Windows starts this copy when the user signs in, or why that can't be told.
    pub startup: Result<&'a Startup, &'a str>,
    pub running_version: &'a str,
    pub app_update: UpdatePrefs,
    pub app_update_status: &'a str,
    pub global: &'a GlobalSettings,
}

pub const APP_INTERVALS: &[(&str, UpdateInterval)] =
    &[("Daily", UpdateInterval::Daily), ("Every 7 days", UpdateInterval::Weekly), ("Every 30 days", UpdateInterval::Monthly)];
pub const APP_TIMINGS: &[(&str, UpdateTiming)] = &[("Immediate", UpdateTiming::Immediate), ("On Quit", UpdateTiming::OnQuit)];

pub fn selection<T: Copy>(choices: &[(&str, T)], text: &str) -> Option<T> {
    choices.iter().find(|(label, _)| *label == text).map(|&(_, value)| value)
}

enum Ctl {
    Button(Btn),
    Combo {
        id: i32,
        items: Vec<String>,
        shown: String,
    },
    /// An on/off switch; `text` is what it is for.
    Toggle {
        id: i32,
        text: String,
        on: bool,
        enabled: bool,
    },
}

fn app_combo<T: PartialEq>(id: i32, purpose: &str, choices: &[(&str, T)], current: T) -> Ctl {
    let shown = choices.iter().find(|(_, value)| *value == current).expect("all app update preferences have a choice").0;
    Ctl::Combo { id, items: choices.iter().map(|(label, _)| (*label).to_string()).collect(), shown: two_lines(purpose, shown) }
}

enum Desc {
    Text(i32, String),
    Path(i32, String),
}

/// One setting: an icon, what it is, and its controls at the right, or under it when the window is narrow.
#[allow(clippy::too_many_arguments)]
fn setting(o: &mut Out, x: i32, y: i32, cw: i32, mark: char, title: &str, desc: Desc, ctls: Vec<Ctl>) -> i32 {
    let widths: Vec<i32> = ctls
        .iter()
        .map(|c| match c {
            Ctl::Button(b) => o.button_width(&b.text, b.glyph),
            Ctl::Combo { .. } => COMBO_W,
            Ctl::Toggle { .. } => TOGGLE_W,
        })
        .collect();
    let all = widths.iter().sum::<i32>() + 8 * (widths.len() as i32 - 1).max(0);
    let tx = x + 52;
    let beside = cw - 52 - 16 - if all > 0 { all + 16 } else { 0 };
    let stacked = all > 0 && beside < 160;
    let tw = if stacked { cw - 52 - 16 } else { beside };
    let desc_h = match &desc {
        Desc::Text(_, text) => (o.fit)(text, Font::Caption, tw.max(1)).1.max(16),
        Desc::Path(..) => 18,
    };
    let text_h = 20 + desc_h;
    let h = if stacked { 12 + text_h + 10 + 32 + 12 } else { (text_h + 24).max(56) };
    let ty = if stacked { y + 12 } else { y + (h - text_h) / 2 };
    let deco_at = o.s.decos.len();
    o.glyph([x + 16, ty, 20, 20], mark, 16, Role::Text);
    o.text([tx, ty, tw, 20], title, Font::Body, Role::Text, ONE_LINE);
    match desc {
        Desc::Text(n, text) => {
            o.para(n, [tx, ty + 20, tw], text, Font::Caption, Role::Text2);
        }
        Desc::Path(n, path) => {
            let flags = DT_SINGLELINE | DT_VCENTER | DT_PATH_ELLIPSIS;
            o.item(n, [tx, ty + 20, tw, 18], path, Look::Text { font: Font::Mono, ink: Role::Text2, flags }, true);
        }
    }
    let (mut cx, cy) = if stacked { (tx, y + 12 + text_h + 10) } else { (x + cw - 16 - all, y + (h - 32) / 2) };
    for (ctl, w) in ctls.into_iter().zip(widths) {
        match ctl {
            Ctl::Button(b) => {
                o.button(b, cx, cy, 32);
            }
            Ctl::Combo { id, items, shown } => o.item(id, [cx, cy, w, 32], shown, Look::Combo { items }, true),
            Ctl::Toggle { id, text, on, enabled } => o.item(id, [cx, cy, w, 32], text, Look::Toggle { on }, enabled),
        }
        cx += w + 8;
    }
    o.card(deco_at, [x, y, cw, h]);
    y + h
}

pub fn token_text(saved: bool) -> &'static str {
    if saved {
        "Used automatically when a repository can’t be read without it, or when GitHub limits app update checks. \
         Your token is saved in Windows Credential Manager on this PC."
    } else {
        "Used automatically when a repository can’t be read without it, or when GitHub limits app update checks. \
         No token is saved, so your GitHub CLI sign-in is used if you have one."
    }
}

pub fn settings_page(p: &SettingsPage, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let (x, cw) = (PAD_X, w - 2 * PAD_X);
    let mut y = o.head(w, "Settings", "Changes are saved as soon as you make them.");
    let (items, shown) = every_choices(Some(p.default), None);
    let every = Ctl::Combo { id: id::DEFAULT_EVERY, items, shown };
    let desc = Desc::Text(0, "Used by every source set to Default".to_string());
    y = setting(&mut o, x, y, cw, glyph::TIMER, "Check for updates every", desc, vec![every]) + GAP;
    y = sources_settings(&mut o, p.global, x, y, cw);
    let (on, enabled, note) = match p.startup {
        Ok(state) => (state.is_on(), state.can_change(), state.note(Via::Window)),
        Err(e) => (false, false, format!("Whether Windows starts it can’t be read: {e}")),
    };
    let switch = Ctl::Toggle { id: id::STARTUP, text: "Start with Windows".to_string(), on, enabled };
    y = setting(&mut o, x, y, cw, glyph::POWER, "Start with Windows", Desc::Text(id::STARTUP_TEXT, note), vec![switch]) + GAP;
    let open = |n| Ctl::Button(btn(n, "Open", Kind::Standard, Some(glyph::OPEN)));
    let desc = Desc::Path(id::SKILLS_DIR, printable(p.skills_dir).into_owned());
    y = setting(&mut o, x, y, cw, glyph::FOLDER, "Skills folder", desc, vec![open(id::OPEN_SKILLS)]) + GAP;
    let desc = Desc::Text(0, "Copies the app set aside instead of deleting them. The app never deletes them.".to_string());
    y = setting(&mut o, x, y, cw, glyph::HISTORY, "Backups", desc, vec![open(id::OPEN_BACKUPS)]) + GAP;
    let tokens = if p.token_saved {
        vec![Ctl::Button(btn(id::SAVE_TOKEN, "Replace token…", Kind::Standard, None)), Ctl::Button(btn(id::DELETE_TOKEN, "Delete token", Kind::Standard, None))]
    } else {
        vec![Ctl::Button(btn(id::SAVE_TOKEN, "Save a token…", Kind::Standard, None))]
    };
    let desc = Desc::Text(id::TOKEN_TEXT, token_text(p.token_saved).to_string());
    y = setting(&mut o, x, y, cw, glyph::KEY, "GitHub sign-in for private repositories", desc, tokens) + GAP;
    let about = format!("Tiny Skill Sync {} is running in the notification area. tss.exe is the command line.", p.running_version);
    y = setting(&mut o, x, y, cw, glyph::INFO, "About", Desc::Text(0, about), Vec::new()) + GAP;
    y = app_updates(&mut o, p, x, y, cw);
    o.done(y + 16)
}

/// What each type is, in Settings.
fn kind_text(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Skills => "Folders with a SKILL.md, copied to your skills folder.",
        SourceKind::Agents => "Custom agents (.agent.md files), copied to your agents folder.",
        SourceKind::Instructions => "Instructions (.instructions.md files), copied to your instructions folder.",
        SourceKind::Plugins => "Copilot plugins, installed with GitHub Copilot CLI after you approve them.",
    }
}

/// The ignored folders as Settings lists them.
pub fn ignored_text(names: &[String]) -> String {
    if names.is_empty() {
        "No folder is skipped. Every folder in a source is searched.".to_string()
    } else {
        format!("Folders with these names are skipped in every source: {}.", printable(&names.join(", ")))
    }
}

/// What every source syncs unless it says otherwise: the four types, repository-specific items and ignored folders.
fn sources_settings(o: &mut Out, g: &GlobalSettings, x: i32, mut y: i32, cw: i32) -> i32 {
    o.label(id::SOURCES_HEADING, [x, y, cw, 28], "Sources", Font::Title, Role::Text);
    y += 28 + 8;
    for (i, kind) in KINDS.into_iter().enumerate() {
        let text = format!("Sync {} by default", kind.nouns().1);
        let switch = Ctl::Toggle { id: id::GLOBAL_TYPE + i as i32, text: text.clone(), on: g.types.enabled(kind), enabled: true };
        let desc = Desc::Text(0, format!("{} A source can turn this on or off for itself.", kind_text(kind)));
        y = setting(o, x, y, cw, kind_glyph(kind), &text, desc, vec![switch]) + GAP;
    }
    let title = "Include repo-specific skills and agents";
    let switch = Ctl::Toggle { id: id::GLOBAL_REPO_ITEMS, text: title.to_string(), on: g.include_repo_items, enabled: true };
    let desc =
        "Also sync skills and agents kept for one repository's own tools, in its .github, .agents, .claude, .copilot, .codex, .cursor and .gemini folders.";
    y = setting(o, x, y, cw, glyph::FOLDER, title, Desc::Text(0, desc.to_string()), vec![switch]) + GAP;
    let edit = Ctl::Button(btn(id::IGNORED_EDIT, "Edit…", Kind::Standard, None));
    let desc = Desc::Text(id::IGNORED_TEXT, ignored_text(&g.ignore_folders));
    setting(o, x, y, cw, glyph::FILE, "Ignored folders", desc, vec![edit]) + GAP
}

fn app_updates(o: &mut Out, p: &SettingsPage, x: i32, mut y: i32, cw: i32) -> i32 {
    o.label(id::APP_HEADING, [x, y, cw, 28], "App updates", Font::Title, Role::Text);
    y = app_report(o, p, x, y + 28 + 8, cw) + GAP;
    let auto = Ctl::Toggle { id: id::APP_AUTO, text: "Automatic app updates".into(), on: p.app_update.enabled, enabled: true };
    let desc = "Let the tray check for and install new versions. Even when off, Check only looks without installing, and Install now installs at once and restarts the tray.";
    y = setting(o, x, y, cw, glyph::REFRESH, "Automatic app updates", Desc::Text(0, desc.into()), vec![auto]) + GAP;
    let purpose = "App update check interval";
    let every = app_combo(id::APP_EVERY, purpose, APP_INTERVALS, p.app_update.interval);
    let desc = Desc::Text(0, "How often the tray checks for an app release when automatic updates are on. Skill checks have their own schedule.".into());
    y = setting(o, x, y, cw, glyph::TIMER, purpose, desc, vec![every]) + GAP;
    let purpose = "When to install app updates";
    let when = app_combo(id::APP_WHEN, purpose, APP_TIMINGS, p.app_update.timing);
    let timing = match p.app_update.timing {
        UpdateTiming::Immediate => {
            "Install as soon as an update is found, then restart the tray into it. If the tray cannot restart, it keeps running its current build and says why."
        }
        UpdateTiming::OnQuit => {
            "Install a pending update when you choose Quit from the tray menu, not when you close this window. The tray stays closed: open it again yourself to use the update."
        }
    };
    setting(o, x, y, cw, glyph::POWER, purpose, Desc::Text(id::APP_TIMING_TEXT, timing.into()), vec![when])
}

fn app_report(o: &mut Out, p: &SettingsPage, x: i32, top: i32, cw: i32) -> i32 {
    let (mark, tx, width) = (o.s.decos.len(), x + 16, (cw - 32).max(1));
    let status = format!("Running tray build: {}.\n{}", p.running_version, printable_lines(p.app_update_status));
    let status = wrap_report(status, width, Font::Caption, o.fit);
    let mut y = top + 12;
    y += o.para(id::APP_STATUS, [tx, y, width], status, Font::Caption, Role::Text) + 8;
    let help = "The last result describes a past check or install. An offered release may differ from the version on disk. Only a pending update is ready to install on Quit.";
    y += o.para(id::APP_STATUS_HELP, [tx, y, width], help, Font::Caption, Role::Text2) + 10;
    let buttons = vec![btn(id::APP_CHECK, "Check only", Kind::Standard, None), btn(id::APP_INSTALL, "Install now", Kind::Standard, None)];
    y = o.flow(tx, y, tx + width, 32, buttons) + 12;
    o.card(mark, [x, top, cw, y - top]);
    y
}

/// DrawText with word wrapping can ask for more width than the card if, for example, an error names a long path.
/// Only break such words; keep the worker's line breaks and every character of its message.
fn wrap_report(text: String, width: i32, font: Font, fit: Fit) -> String {
    if fit(&text, font, width).0 <= width {
        return text;
    }
    fn push_word(out: &mut String, word: &str, width: i32, font: Font, fit: Fit) {
        match word.char_indices().nth(word.chars().count() / 2) {
            Some((cut, _)) if cut > 0 && fit(word, font, 0).0 > width => {
                push_word(out, &word[..cut], width, font, fit);
                out.push('\n');
                push_word(out, &word[cut..], width, font, fit);
            }
            _ => out.push_str(word),
        }
    }
    let mut out = String::with_capacity(text.len());
    for part in text.split_inclusive(char::is_whitespace) {
        let word = part.trim_end_matches(char::is_whitespace);
        push_word(&mut out, word, width, font, fit);
        out.push_str(&part[word.len()..]);
    }
    out
}

/// One entry of the Activity page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub glyph: char,
    pub ink: Role,
    pub at: Option<UnixSecs>,
    pub text: String,
}

/// What the Activity page lists: a problem first, then the newest message first. Text that came from a repository is
/// escaped (see `printable_lines`), so a skill or file name cannot reorder or hide what is shown.
pub fn activity_of(board: &Board) -> Vec<Note> {
    let mut notes = Vec::new();
    for problem in board.problem().into_iter().chain(board.failure()) {
        notes.push(Note { glyph: glyph::ERROR_BADGE, ink: Role::Err, at: None, text: sentence(&printable(problem)) });
    }
    if !board.orphans().is_empty() {
        let text = format!(
            "Items recorded for sources that are no longer in the settings are left alone: {}. To stop tracking them, run: tss remove ID",
            printable(&board.orphans().join(", "))
        );
        notes.push(Note { glyph: glyph::INFO, ink: Role::Text2, at: None, text });
    }
    let messages: Vec<_> = board.messages().collect();
    for message in messages.iter().rev() {
        let (glyph, ink) = match message.level {
            Level::Ok => (glyph::OK, Role::Ok),
            Level::Attention => (glyph::WARN, Role::Warn),
            Level::Error => (glyph::ERR, Role::Err),
        };
        notes.push(Note { glyph, ink, at: Some(message.at), text: printable_lines(&message.text).into_owned() });
    }
    notes
}

/// `when` writes a time the way the user reads it.
pub fn activity_page(notes: &[Note], when: &dyn Fn(UnixSecs) -> String, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let (x, cw) = (PAD_X, w - 2 * PAD_X);
    let mut y = o.head(w, "Activity", "What the app did recently, newest first.");
    let (mark, top) = (o.s.decos.len(), y);
    if notes.is_empty() {
        y += 10 + o.para(id::EMPTY, [x + 16, y + 10, cw - 32], "Nothing to show yet.", Font::Body, Role::Text2) + 10;
    }
    for (k, note) in notes.iter().enumerate() {
        if k > 0 {
            o.fill([x + 1, y, cw - 2, 1], 0, Role::Divider, Role::None);
        }
        let time = note.at.map(when).unwrap_or_default();
        let time_w = if time.is_empty() { 0 } else { o.width(&time, Font::Caption) + 2 };
        let tw = cw - 48 - 16 - if time_w > 0 { time_w + 14 } else { 0 };
        o.glyph([x + 16, y + 10, 18, 20], note.glyph, 14, note.ink);
        let h = o.para(id::NOTE + k as i32, [x + 48, y + 10, tw], &note.text, Font::Body, Role::Text);
        if time_w > 0 {
            o.text([x + cw - 16 - time_w, y + 10, time_w, 20], time, Font::Caption, Role::Text2, ONE_LINE | DT_RIGHT);
        }
        y += 10 + h + 10;
    }
    o.card(mark, [x, top, cw, y - top]);
    o.done(y + 16)
}

/// The displayed list width is bounded, but the caller retains the preferred width.
pub fn list_width(window_w: i32, preferred: i32) -> i32 {
    preferred.clamp(MIN_LIST_W, (window_w.saturating_sub(SPLITTER_W + MIN_PAGE_W)).max(MIN_LIST_W))
}

#[derive(Clone, Copy)]
pub enum SplitKey {
    Left,
    Right,
    Home,
    End,
}

pub fn width_after_key(window_w: i32, preferred: i32, key: SplitKey, shift: bool) -> i32 {
    let shown = list_width(window_w, preferred);
    let step = if shift { 32 } else { 8 };
    let wanted = match key {
        SplitKey::Left => shown - step,
        SplitKey::Right => shown + step,
        SplitKey::Home => MIN_LIST_W,
        SplitKey::End => i32::MAX,
    };
    list_width(window_w, wanted)
}

pub struct SplitLayout {
    pub list: Rect,
    pub splitter: Rect,
    pub page: Rect,
}

/// The same partition is used below either header, with a strip of its own for the splitter.
pub fn split_layout(w: i32, top: i32, height: i32, preferred: i32) -> SplitLayout {
    let list = list_width(w, preferred);
    let strip = (w - list).clamp(0, SPLITTER_W);
    let right = list + strip;
    SplitLayout { list: [0, top, list, height], splitter: [list, top, strip, height], page: [right, top, (w - right).max(0), height] }
}

pub fn splitter(w: i32, h: i32) -> Surface {
    // The strip is fixed, not a scrolling surface.
    let item = Item { id: id::SPLITTER, at: [0, 0, w, h], text: "Resize list".into(), look: Look::Splitter, enabled: true };
    Surface { items: vec![item], ..Surface::default() }
}

/// The sky, the hills in front of it and the line under the band.
pub fn band(w: i32) -> [Deco; 3] {
    [
        Deco::Fill { at: [0, 0, w, HERO_H], radius: 0, fill: Role::Band, border: Role::None },
        Deco::BandPrint { at: [0, 0, w, HERO_H] },
        Deco::Fill { at: [0, HERO_H - 1, w, 1], radius: 0, fill: Role::BandEdge, border: Role::None },
    ]
}

/// The four types, in the list's order, climbing the middle hill: each one's distance from the band's right edge.
const STATIONS: [(f32, SourceKind); 4] =
    [(304.0, SourceKind::Skills), (270.0, SourceKind::Agents), (236.0, SourceKind::Instructions), (202.0, SourceKind::Plugins)];
const STATION_R: i32 = 10;

fn overlaps([x, y, w, h]: Rect, [ox, oy, ow, oh]: Rect) -> bool {
    x < ox + ow && ox < x + w && y < oy + oh && oy < y + h
}

/// Each type's icon in a disc of sky set into the middle hill, just under its top. All four or none: every disc keeps
/// 12 DIPs to either side clear of what the header shows on the band (`taken`, text as wide as it is drawn, and
/// buttons).
pub fn stations(w: i32, taken: &[Rect]) -> Vec<Deco> {
    let mut discs: Vec<Rect> = Vec::new();
    for (v, _) in STATIONS {
        let (x, y) = ((w as f32 - v).round() as i32, (HERO_H as f32 - hill(2, v) + 2.0).round() as i32);
        let at = [x - STATION_R, y - STATION_R, 2 * STATION_R, 2 * STATION_R];
        let air = [at[0] - 12, at[1] - 2, at[2] + 24, at[3] + 4];
        if taken.iter().any(|t| overlaps(air, *t)) || discs.iter().any(|d| overlaps([at[0] - 2, at[1], at[2] + 4, at[3]], *d)) {
            return Vec::new();
        }
        discs.push(at);
    }
    let disc = |at| Deco::Fill { at, radius: STATION_R, fill: Role::Band, border: Role::None };
    discs.into_iter().zip(STATIONS).flat_map(|(at, (_, kind))| [disc(at), Deco::Glyph { at, ch: kind_glyph(kind), size: 12, ink: Role::BandMark }]).collect()
}

const LAYER_RADIUS: i32 = 8;

/// The raised layer to the right of the splitter, then the band. The layer starts under the band, so the band hides
/// its rounded top corner and its top border.
pub fn backdrop(page_x: i32, w: i32, h: i32) -> Vec<Deco> {
    let top = HERO_H - 2 * LAYER_RADIUS;
    let layer = Deco::Fill { at: [page_x, top, w - page_x + 20, h - top + 20], radius: LAYER_RADIUS, fill: Role::Layer, border: Role::LayerBorder };
    let mut out = vec![layer];
    out.extend(band(w));
    out
}

/// The diff header and the list and comparison below it, each with a card that starts in the header.
pub struct DiffLayout {
    pub head: Rect,
    pub files_card: Rect,
    pub code_card: Rect,
    pub split: SplitLayout,
}

const DIFF_TOP: i32 = HERO_H + 12;
const CARD_HEAD: i32 = 41;

pub fn diff_layout(w: i32, h: i32, preferred: i32) -> DiffLayout {
    let top = DIFF_TOP + CARD_HEAD;
    let body = (h - top - 17).max(0);
    let split = split_layout(w, top, body, preferred);
    let card = |[x, _, width, _]: Rect| [x, DIFF_TOP, width, CARD_HEAD + body + 1];
    DiffLayout { head: [0, 0, w, top], files_card: card(split.list), code_card: card(split.page), split }
}

pub fn diff_backdrop(l: &DiffLayout, w: i32, h: i32) -> Vec<Deco> {
    let mut out = vec![Deco::Fill { at: [-20, 0, w + 40, h + 20], radius: 0, fill: Role::Layer, border: Role::None }];
    out.extend(band(w));
    for card in [l.files_card, l.code_card] {
        out.push(Deco::Fill { at: card, radius: 6, fill: Role::Card, border: Role::CardBorder });
        out.push(Deco::Fill { at: [card[0] + 1, DIFF_TOP + CARD_HEAD - 1, card[2] - 2, 1], radius: 0, fill: Role::Divider, border: Role::None });
    }
    out
}

/// What "Show changes" or "Preview" found, or that it is still looking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    Loading,
    Failed(String),
    Ready(DiffReport),
}

/// The diff view of one skill, which also shows a Preview: the files GitHub put there, whole, as they are on this PC.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffView {
    pub source: RepoConfig,
    pub skill: String,
    /// A Preview rather than a comparison: no choices, no legend, and one column of line numbers.
    pub preview: bool,
    /// The skill's choices when it was opened.
    pub actions: Vec<Action>,
    pub report: Report,
    /// The file shown.
    pub file: usize,
    /// The folded runs of lines the user opened, by row.
    pub gaps: Vec<usize>,
}

impl DiffView {
    pub fn files(&self) -> &[FileChange] {
        match &self.report {
            Report::Ready(report) => &report.files,
            _ => &[],
        }
    }

    fn shown(&self) -> Option<&FileChange> {
        self.files().get(self.file)
    }

    /// The mark, its colour and the second line of a file in the list.
    pub fn summary(&self, change: &FileChange) -> (char, Role, String) {
        if !self.preview {
            return file_summary(change);
        }
        match &change.body {
            Body::Text(t) if t.new.is_empty() => (glyph::FILE, Role::Text2, "Empty".to_string()),
            Body::Text(t) if t.new.len() == 1 => (glyph::FILE, Role::Text2, "1 line".to_string()),
            Body::Text(t) => (glyph::FILE, Role::Text2, format!("{} lines", t.new.len())),
            Body::NotText => (glyph::BINARY, Role::Text2, "Not text".to_string()),
            Body::Hidden(_) => (glyph::INFO, Role::Text2, "Not shown".to_string()),
        }
    }

    fn reading(&self) -> &'static str {
        if self.preview { "Reading the files…" } else { "Reading both copies…" }
    }
}

/// The sentence under the skill's name.
pub fn diff_sub(v: &DiffView) -> String {
    match &v.report {
        Report::Loading => v.reading().to_string(),
        Report::Failed(why) => sentence(&printable(why)),
        Report::Ready(_) if v.preview && v.source.kind != SourceKind::Skills => "The installed file, as it is on this PC. Nothing is changed.".to_string(),
        Report::Ready(_) if v.preview => "The files GitHub put in this skill, as they are on this PC. Files you added are not shown.".to_string(),
        Report::Ready(r) if r.files.is_empty() => "The copy on this PC is the same as GitHub.".to_string(),
        Report::Ready(r) if !r.here && v.source.kind != SourceKind::Skills => "The file is missing on this PC.".to_string(),
        Report::Ready(r) if !r.here => "Its folder is not on this PC, so every file shows as only on GitHub.".to_string(),
        Report::Ready(r) if !r.on_github => "GitHub no longer lists it, so every file shows as only on this PC.".to_string(),
        Report::Ready(_) => "How the copy on this PC would change to match GitHub. Nothing changes until you choose.".to_string(),
    }
}

/// The header: Back, which skill, its choices, and the headers of the two cards.
pub fn diff_head(v: &DiffView, l: &DiffLayout, fit: Fit) -> Surface {
    let w = l.head[2];
    let mut o = Out::new(fit);
    // From the right: the first choice, which takes GitHub's version, ends the row. A Preview changes nothing, so it
    // offers none.
    let actions: &[Action] = if !v.preview && matches!(v.report, Report::Ready(_)) { &v.actions } else { &[] };
    let mut right = w - 20;
    let mut choices = Vec::new();
    for (k, action) in actions.iter().enumerate() {
        if *action == Action::RemoveThem {
            continue;
        }
        let kind = if matches!(action, Action::Overwrite | Action::Restore) { Kind::BandAccent } else { Kind::Standard };
        let b = btn(id::DIFF_ACTION + k as i32, action.label(), kind, action_glyph(*action));
        let bw = o.button_width(&b.text, b.glyph);
        if right - bw < 56 + 120 {
            break;
        }
        right -= bw;
        choices.push((b, right));
        right -= 8;
    }
    let room = (right - 8 - 56).max(0);
    let sub = diff_sub(v);
    // The title and the sentence under it sit centred together beside the buttons. A sentence too long for that space
    // goes under the buttons instead, across the band, and the row above it moves up.
    let (row, top, sub_at) = if o.width(&sub, Font::Caption) <= room {
        let top = in_band(28 + 16);
        (in_band(32), top, [56, top + 28, room, 16])
    } else {
        (8, 10, [56, 44, (w - 80).max(0), 16])
    };
    o.item(id::BACK, [12, row, 32, 32], "Back", Look::Button { kind: Kind::Icon, glyph: Some(glyph::BACK) }, true);
    let mut taken = vec![[12, row, 32, 32]];
    for (b, x) in choices {
        let bw = o.button(b, x, row, 32);
        taken.push([x, row, bw, 32]);
    }
    let repo = format!("{}: {} [{}]", v.source.kind.heading(), printable(&v.source.label()), printable(&v.source.id));
    let skill = printable(&v.skill).into_owned();
    let repo_w = (o.width(&repo, Font::Title) + 2).min(room / 2);
    o.text([56, top, repo_w, 28], repo, Font::Title, Role::BandCaption, ONE_LINE);
    o.glyph([56 + repo_w + 8, top, 12, 28], glyph::CHEVRON_RIGHT, 12, Role::BandCaption);
    let sx = 56 + repo_w + 28;
    let title_w = (right - 8 - sx).max(0);
    taken.push([56, top, sx + o.width(&skill, Font::Title).min(title_w) - 56, 28]);
    taken.push([sub_at[0], sub_at[1], o.width(&sub, Font::Caption).min(sub_at[2]), sub_at[3]]);
    o.s.decos.extend(stations(w, &taken));
    o.label(id::TITLE, [sx, top, title_w, 28], skill, Font::Title, Role::BandText);
    o.label(id::SUB, sub_at, sub, Font::Caption, Role::BandCaption);
    let files = v.files();
    if !files.is_empty() {
        let count = match (v.preview, files.len()) {
            (true, 1) => "1 file".to_string(),
            (true, n) => format!("{n} files"),
            (false, 1) => "1 file is different".to_string(),
            (false, n) => format!("{n} files are different"),
        };
        o.text([l.files_card[0] + 12, DIFF_TOP, l.files_card[2] - 24, CARD_HEAD - 1], count, Font::Caption, Role::Text2, ONE_LINE);
    }
    if let Some(change) = v.shown() {
        let [cx, _, cw, _] = l.code_card;
        let mut lx = cx + cw - 14;
        if !v.preview && matches!(change.body, Body::Text(_)) {
            for (sign, ink, text, bg) in [("+", Role::Ok, "Only on GitHub", Role::AddBg), ("−", Role::Err, "Only on this PC", Role::DelBg)] {
                let lw = 8 + 8 + 6 + o.width(text, Font::Caption) + 2 + 8;
                if lx - lw < cx + 14 {
                    break;
                }
                lx -= lw;
                o.fill([lx, DIFF_TOP + 9, lw, 22], 4, bg, Role::None);
                o.text([lx + 8, DIFF_TOP + 9, 8, 22], sign, Font::CaptionStrong, ink, ONE_LINE);
                o.text([lx + 22, DIFF_TOP + 9, lw - 30, 22], text, Font::Caption, Role::Text, ONE_LINE);
                lx -= 10;
            }
        }
        o.text([cx + 14, DIFF_TOP, (lx - cx - 14 - 8).max(0), CARD_HEAD - 1], printable(&change.path), Font::Strong, Role::Text, ONE_LINE);
    }
    o.done(DIFF_TOP + CARD_HEAD)
}

/// The mark, its colour and the second line of a file in the list of differences.
pub fn file_summary(change: &FileChange) -> (char, Role, String) {
    match (change.presence, &change.body) {
        (Presence::OnlyHere, _) => (glyph::REMOVE, Role::Err, "Only on this PC".to_string()),
        (Presence::OnlyOnGitHub, _) => (glyph::ADD, Role::Ok, "Only on GitHub".to_string()),
        (Presence::Both, Body::Text(t)) if t.only_invisible() => (glyph::EDIT, Role::Warn, "Only invisible changes".to_string()),
        (Presence::Both, Body::Text(t)) => (glyph::EDIT, Role::Warn, format!("Changed · {} added, {} removed", t.added, t.removed)),
        (Presence::Both, Body::NotText) => (glyph::BINARY, Role::Text2, "Different (not text)".to_string()),
        (Presence::Both, Body::Hidden(_)) => (glyph::INFO, Role::Text2, "Not shown".to_string()),
    }
}

/// The files that are different, or a Preview's files, in the left card.
pub fn diff_files(v: &DiffView, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let mut y = 4;
    for (i, change) in v.files().iter().enumerate() {
        let (mark, ink, sub) = v.summary(change);
        let look = Look::Nav { mark: Mark::Glyph(mark, ink), ink: Role::Text, selected: i == v.file };
        o.item(id::FILE + i as i32, [4, y, w - 8, 46], two_lines(&printable(&change.path), &sub), look, true);
        y += 48;
    }
    o.done(y + 4)
}

/// A line of a file as it is shown: tabs as four spaces, anything that could hide or reorder text written out, and
/// cut when it is longer than any window.
fn code_line(text: &str) -> String {
    let text = text.replace('\t', "    ");
    let text = printable(&text);
    match text.char_indices().nth(MAX_LINE_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.into_owned(),
    }
}

/// The lines of the file shown, in the right card.
pub fn diff_code(v: &DiffView, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let message = |o: &mut Out, text: &str| 12 + o.para(id::EMPTY, [16, 12, (w - 32).max(1)], text, Font::Body, Role::Text2) + 12;
    let bottom = match (&v.report, v.shown()) {
        (Report::Loading, _) => message(&mut o, v.reading()),
        (Report::Ready(_), None) | (Report::Failed(_), _) => 0,
        // A Preview's file is whole, so it has no change to describe.
        (Report::Ready(_), Some(change)) if v.preview => match &change.body {
            Body::NotText => message(&mut o, "This file is not text, so it is not shown."),
            Body::Hidden(why) => message(&mut o, &format!("This file is not shown: {}.", printable(why))),
            Body::Text(t) if t.new.is_empty() => message(&mut o, "This file is empty."),
            Body::Text(t) => lines(&mut o, t, &[], w, true),
        },
        (Report::Ready(_), Some(change)) => match &change.body {
            Body::NotText => message(&mut o, "This file is not text, so only the fact that it is different is shown."),
            Body::Hidden(why) => message(&mut o, &format!("This file is not shown: {}.", printable(why))),
            Body::Text(t) if t.only_invisible() && change.presence != Presence::Both => message(&mut o, "This file is empty, or holds only blank lines."),
            Body::Text(t) if t.only_invisible() => message(
                &mut o,
                "Every line is the same. Only a mark that can't be seen at the start of the file (a byte order mark) is \
                 different. Line breaks and blank lines don't count.",
            ),
            Body::Text(t) => lines(&mut o, t, &v.gaps, w, false),
        },
    };
    o.done(bottom)
}

/// Draws the rows of `t`. `one` shows a single column of numbers, the line's own, and leaves room for no sign: a
/// Preview's lines are all the same.
fn lines(o: &mut Out, t: &TextDiff, open: &[usize], w: i32, one: bool) -> i32 {
    let most = t.old.len().max(t.new.len()).max(1);
    let gw = 38.max(o.width(&"0".repeat(most.to_string().len()), Font::Mono) + 16);
    let sx = if one { gw } else { 2 * gw };
    let tx = if one { sx + 8 } else { sx + 18 };
    let mut y = 4;
    let mut shown = 0;
    let line = |o: &mut Out, y: i32, old: Option<usize>, new: Option<usize>| {
        let (sign, ink, text, bg, gut) = match (old, new) {
            (Some(n), None) => ("−", Role::Err, t.old.get(n.wrapping_sub(1)), Role::DelBg, Role::DelGut),
            (None, Some(n)) => ("+", Role::Ok, t.new.get(n.wrapping_sub(1)), Role::AddBg, Role::AddGut),
            (Some(_), Some(n)) => ("", Role::Text2, t.new.get(n.wrapping_sub(1)), Role::None, Role::None),
            (None, None) => return,
        };
        let Some(text) = text else { return };
        if bg != Role::None {
            o.fill([0, y, w, LINE_H], 0, bg, Role::None);
            o.fill([0, y, sx, LINE_H], 0, gut, Role::None);
        }
        let numbers = if one { [new, None] } else { [old, new] };
        for (k, number) in numbers.into_iter().enumerate() {
            if let Some(n) = number {
                o.text([k as i32 * gw, y, gw - 8, LINE_H], n.to_string(), Font::Mono, Role::Text3, ONE_LINE | DT_RIGHT);
            }
        }
        if !sign.is_empty() {
            o.text([sx, y, 18, LINE_H], sign, Font::Mono, ink, ONE_LINE | DT_CENTER);
        }
        o.text([tx, y, (w - tx - 8).max(0), LINE_H], code_line(text), Font::Mono, Role::Text, ONE_LINE);
    };
    for (r, row) in t.rows.iter().enumerate() {
        if shown >= MAX_LINES {
            break;
        }
        match *row {
            Line::Same { old, new } => line(o, y, Some(old), Some(new)),
            Line::Removed { old } => line(o, y, Some(old), None),
            Line::Added { new } => line(o, y, None, Some(new)),
            Line::Folded { old, new, count } if open.contains(&r) => {
                for (old, new) in t.folded(old, new, count).take(MAX_LINES - shown) {
                    line(o, y, Some(old), Some(new));
                    y += LINE_H;
                    shown += 1;
                }
                continue;
            }
            Line::Folded { count, .. } => {
                o.fill([0, y, w, LINE_H], 0, Role::GapBg, Role::None);
                let text = if count == 1 { "1 line is the same".to_string() } else { format!("{count} lines are the same") };
                let bw = o.width(&text, Font::Caption) + 2 + 12 + 8 + 16;
                let look = Look::Button { kind: Kind::Link, glyph: Some(glyph::CHEVRON_DOWN) };
                o.item(id::GAP + r as i32, [tx - 8, y, bw, LINE_H], text, look, true);
            }
        }
        y += LINE_H;
        shown += 1;
    }
    if shown >= MAX_LINES {
        y += 8 + o.para(id::EMPTY, [tx, y + 8, (w - tx - 16).max(1)], "The rest of this file is not shown.", Font::Body, Role::Text2) + 8;
    }
    y + 4
}

/// The controls of a question. OK and Cancel have the ids Enter and Escape send (`IDOK`, `IDCANCEL`).
pub mod ask_id {
    pub const OK: i32 = 1;
    pub const CANCEL: i32 = 2;
    pub const EDIT: i32 = 10;
    pub const HEADING: i32 = 11;
    pub const TEXT: i32 = 12;
    pub const LABEL: i32 = 13;
    pub const HINT: i32 = 14;
    pub const COMBO_LABEL: i32 = 15;
    pub const COMBO: i32 = 16;
    pub const NOTE: i32 = 17;
    /// One each, from here.
    pub const CHOICE: i32 = 20;
}

/// The width of a question, in DIPs.
pub const ASK_W: i32 = 468;
const FOOT_H: i32 = 72;

/// The line under a text box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hint {
    Plain(String),
    /// What the text was read as.
    Ok(String),
    /// What is wrong with it.
    Err(String),
}

/// A question: a heading, some text, and optionally a text box, a list, choices and a note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub heading: String,
    pub text: String,
    /// A text box's label and the line under it. The box itself is a native edit control at the returned rect.
    pub field: Option<(String, Hint)>,
    /// Whether the text box has the keyboard focus.
    pub focused: bool,
    /// A list's label, its entries and the one shown.
    pub combo: Option<(String, Vec<String>, String)>,
    pub choices: Vec<String>,
    pub choice: usize,
    pub note: Option<(char, String)>,
    pub ok: String,
    /// The Cancel button's label; "Cancel" when empty.
    pub cancel: String,
}

/// Lays out a question [`ASK_W`] wide. Also returns where the text box's edit control goes.
pub fn ask(q: &Question, fit: Fit) -> (Surface, Option<Rect>) {
    let mut o = Out::new(fit);
    let (x, cw) = (PAD_X, ASK_W - 2 * PAD_X);
    let mut y = PAD_X;
    y += o.para(ask_id::HEADING, [x, y, cw], q.heading.as_str(), Font::Title, Role::Text);
    if !q.text.is_empty() {
        y += 4 + o.para(ask_id::TEXT, [x, y + 4, cw], q.text.as_str(), Font::Body, Role::Text2);
    }
    let mut edit = None;
    if let Some((label, hint)) = &q.field {
        o.label(ask_id::LABEL, [x, y + 16, cw, LINE_H], label.as_str(), Font::Body, Role::Text);
        y += 16 + LINE_H + 6;
        if q.focused {
            o.fill([x, y, cw, 32], 4, Role::Card, Role::CtlBorder);
            o.fill([x + 1, y + 30, cw - 2, 2], 0, Role::Accent, Role::None);
        } else {
            o.fill([x, y, cw, 32], 4, Role::Ctl, Role::CtlBorder);
            o.fill([x + 3, y + 31, cw - 6, 1], 0, Role::CtlBottom, Role::None);
        }
        edit = Some([x + 11, y + 6, cw - 22, 20]);
        y += 32 + 8;
        let (mark, text, ink) = match hint {
            Hint::Plain(t) => (None, t, Role::Text2),
            Hint::Ok(t) => (Some((glyph::OK, Role::Ok)), t, Role::Text2),
            Hint::Err(t) => (Some((glyph::ERROR_BADGE, Role::Err)), t, Role::Err),
        };
        let tx = if mark.is_some() { x + 20 } else { x };
        if let Some((ch, ink)) = mark {
            o.glyph([x, y, 12, 16], ch, 12, ink);
        }
        y += o.para(ask_id::HINT, [tx, y, x + cw - tx], text.as_str(), Font::Caption, ink);
    }
    if let Some((label, items, shown)) = &q.combo {
        o.label(ask_id::COMBO_LABEL, [x, y + 16, cw, LINE_H], label.as_str(), Font::Body, Role::Text);
        y += 16 + LINE_H + 6;
        o.item(ask_id::COMBO, [x, y, 200, 32], shown.as_str(), Look::Combo { items: items.clone() }, true);
        y += 32;
    }
    if !q.choices.is_empty() {
        y += 12;
        for (i, choice) in q.choices.iter().enumerate() {
            o.item(ask_id::CHOICE + i as i32, [x - 6, y, cw + 12, 32], choice.as_str(), Look::Radio { on: i == q.choice }, true);
            y += 36;
        }
        y -= 4;
    }
    if let Some((ch, text)) = &q.note {
        y += 18;
        let tw = cw - 24 - 24;
        let h = (o.fit)(text, Font::Caption, tw).1.max(line_height(Font::Caption));
        o.fill([x, y, cw, h + 20], 6, Role::OffBg, Role::None);
        o.glyph([x + 12, y + 10, 14, 16], *ch, 14, Role::Text);
        o.para(ask_id::NOTE, [x + 36, y + 10, tw], text.as_str(), Font::Caption, Role::Text2);
        y += h + 20;
    }
    y += 20;
    o.fill([0, y, ASK_W, FOOT_H], 0, Role::DialogFoot, Role::None);
    o.fill([0, y, ASK_W, 1], 0, Role::Divider, Role::None);
    let bw = (cw - 8) / 2;
    o.item(ask_id::OK, [x, y + 20, bw, 32], q.ok.as_str(), Look::Button { kind: Kind::Accent, glyph: None }, true);
    let cancel = if q.cancel.is_empty() { "Cancel" } else { q.cancel.as_str() };
    o.item(ask_id::CANCEL, [x + bw + 8, y + 20, cw - bw - 8, 32], cancel, Look::Button { kind: Kind::Standard, glyph: None }, true);
    (o.done(y + FOOT_H), edit)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::config::DEFAULT_INTERVAL;
    use crate::diff::{Content, compare, text_diff, whole};
    use crate::engine::SkillFacts;
    use crate::interval::Unit;

    const NOW: UnixSecs = 1_700_000_000;

    /// 7 DIPs a character, wrapped by characters.
    pub(in crate::win::ui) fn fit(text: &str, font: Font, width: i32) -> (i32, i32) {
        fit_by(7, text, font, width)
    }

    /// Like `fit`, but a caption is 6 DIPs a character. Measured in its real font (Segoe UI Variable Text at 12 px),
    /// the longest sentence under the diff view's title is 497 DIPs for 87 characters, under 5.8 a character; 7 would
    /// make every such sentence look too long for the window.
    fn caption_fit(text: &str, font: Font, width: i32) -> (i32, i32) {
        fit_by(if font == Font::Caption { 6 } else { 7 }, text, font, width)
    }

    fn fit_by(per: i32, text: &str, font: Font, width: i32) -> (i32, i32) {
        let w = text.chars().count() as i32 * per;
        let lh = line_height(font);
        if width <= 0 { (w, lh) } else { (w.min(width), ((w + width - 1) / width).max(1) * lh) }
    }

    fn row(name: &str, level: Level, tag: &'static str, actions: Vec<Action>) -> Row {
        Row {
            name: name.to_string(),
            text: format!("about {name}"),
            tag,
            level,
            actions,
            facts: None,
            listed: true,
            remote_path: None,
            folder_missing: false,
            plugin: None,
        }
    }

    fn ok(name: &str) -> Row {
        row(name, Level::Ok, board::UP_TO_DATE_TAG, Vec::new())
    }

    fn edited(name: &str) -> Row {
        row(name, Level::Attention, board::EDITED_TAG, vec![Action::Overwrite, Action::KeepMine])
    }

    fn repo(name: &str) -> RepoConfig {
        RepoConfig { id: name.to_string(), repo: name.to_string(), path: "skills".to_string(), branch: "main".to_string(), ..RepoConfig::default() }
    }

    fn kind_view(kind: SourceKind, lane: &str, rows: Vec<Row>) -> KindView {
        let level = rows.iter().map(|r| r.level).max().unwrap_or(Level::Ok);
        KindView { kind, lane: lane.to_string(), on: true, found: None, filtered: 0, notes: Vec::new(), rows, status: "Up to date".to_string(), level }
    }

    fn source_view(name: &str, kinds: Vec<KindView>) -> SourceView {
        let level = kinds.iter().map(|k| k.level).max().unwrap_or(Level::Ok);
        SourceView {
            id: name.to_string(),
            label: name.to_string(),
            repo: name.to_string(),
            branch: "main".to_string(),
            seen_branch: None,
            folder: "skills".to_string(),
            enabled: true,
            status: "Up to date".to_string(),
            detail: None,
            level,
            last_check: Some(NOW - 120),
            private: false,
            detected: None,
            kinds,
            notes: Vec::new(),
            more_notes: 0,
        }
    }

    /// A source with only skills, whose skills lane has the source's ID.
    fn skills_view(name: &str, rows: Vec<Row>) -> SourceView {
        source_view(name, vec![kind_view(SourceKind::Skills, name, rows)])
    }

    fn unreadable_source(name: &str) -> SourceView {
        let failed = KindView { level: Level::Error, status: "Not found".to_string(), ..kind_view(SourceKind::Skills, name, Vec::new()) };
        SourceView {
            level: Level::Error,
            status: "Not found".to_string(),
            detail: Some("GitHub answered not found.".to_string()),
            ..source_view(name, vec![failed])
        }
    }

    fn source_cfg(name: &str) -> SourceConfig {
        let lanes = crate::sources::Lanes::default().with(SourceKind::Skills, name.to_string());
        SourceConfig { id: name.to_string(), repo: name.to_string(), path: "skills".to_string(), branch: "main".to_string(), lanes, ..SourceConfig::default() }
    }

    fn page_of(v: &SourceView, s: &SourceConfig, toggled: &HashSet<String>, w: i32) -> Surface {
        let sections = [
            (SectionKey::WhatToSync, true),
            (SectionKey::Kind(SourceKind::Skills), true),
            (SectionKey::Kind(SourceKind::Agents), true),
            (SectionKey::Kind(SourceKind::Instructions), true),
            (SectionKey::Kind(SourceKind::Plugins), true),
            (SectionKey::NeverSynced, true),
        ]
        .into();
        page_with(v, s, toggled, &sections, w)
    }

    /// A source's page with only the sections in `sections` opened or closed; the others keep their defaults.
    fn page_with(v: &SourceView, s: &SourceConfig, toggled: &HashSet<String>, sections: &HashMap<SectionKey, bool>, w: i32) -> Surface {
        let global = GlobalSettings::default();
        source_page(&SourcePage { view: v, source: s, global: &global, default: DEFAULT_INTERVAL, now: NOW, toggled, draft: None, sections }, w, &fit)
    }

    fn preview_page(draft: &SourceDraft, w: i32, measure: Fit) -> Surface {
        let global = GlobalSettings::default();
        let toggled = HashSet::new();
        let sections = HashMap::new();
        source_preview(&PreviewPage { draft, global: &global, default: DEFAULT_INTERVAL, now: NOW, toggled: &toggled, sections: &sections }, w, measure)
    }

    fn settings<'a>(startup: Result<&'a Startup, &'a str>) -> SettingsPage<'a> {
        SettingsPage {
            default: DEFAULT_INTERVAL,
            skills_dir: r"C:\skills",
            token_saved: false,
            startup,
            running_version: "0.2.0",
            app_update: UpdatePrefs::default(),
            app_update_status: "Not checked in this session.",
            global: Box::leak(Box::new(GlobalSettings::default())),
        }
    }

    fn texts(s: &Surface) -> Vec<(i32, String)> {
        s.items.iter().map(|i| (i.id, i.text.clone())).collect()
    }

    fn item(s: &Surface, n: i32) -> &Item {
        s.items.iter().find(|i| i.id == n).unwrap_or_else(|| panic!("no control {n} in {:?}", texts(s)))
    }

    /// Every control is inside 0..w from left to right, and none is over another: controls are sibling windows that do
    /// not clip each other, so one that paints again by itself draws over any part of another that it covers.
    fn fits_in(s: &Surface, w: i32) {
        let mut over = Vec::new();
        for (k, i) in s.items.iter().enumerate() {
            let [x, y, iw, ih] = i.at;
            assert!(x >= 0 && iw >= 0 && x + iw <= w, "{} ({:?}) is outside 0..{w}: {:?}", i.id, i.text, i.at);
            for j in &s.items[k + 1..] {
                let [jx, jy, jw, jh] = j.at;
                let empty = iw <= 0 || ih <= 0 || jw <= 0 || jh <= 0;
                if !empty && x < jx + jw && jx < x + iw && y < jy + jh && jy < y + ih {
                    over.push(format!("{} ({:?}) at {:?} is over {} ({:?}) at {:?}", i.id, i.text, i.at, j.id, j.text, j.at));
                }
            }
        }
        assert!(over.is_empty(), "controls overlap:\n{}", over.join("\n"));
    }

    #[test]
    fn the_hero_says_the_most_important_thing_first() {
        let status = "Next check in 1 hour";
        let hero = |views: &[SourceView], problem: Option<&str>| hero_of(views, problem, status, false, NOW);
        let fine = skills_view("o/a", vec![ok("x"), ok("y")]);
        let other = skills_view("o/b", vec![ok("z")]);

        let got = hero(&[fine.clone(), other.clone()], None);
        assert_eq!((got.tone, got.title.as_str()), (Tone::Ok, "All skills are up to date"));
        assert_eq!(got.sub, "3 skills from 2 sources · checked 2 minutes ago · next check in 1 hour");

        let needy = skills_view("o/\u{202e}b@main : skills", vec![edited("e"), ok("z")]);
        let got = hero(&[fine.clone(), needy.clone()], None);
        assert_eq!((got.tone, got.title.as_str()), (Tone::Warn, "1 skill needs your attention"));
        assert_eq!(got.sub, "In o/\\u{202e}b · checked 2 minutes ago · next check in 1 hour", "a name from the settings is escaped");

        let got = hero(&[needy.clone(), unreadable_source("o/c"), unreadable_source("o/d")], None);
        assert_eq!(
            (got.tone, got.title.as_str(), got.sub.as_str()),
            (Tone::Err, "2 sources can’t be read", "In 2 sources · checked 2 minutes ago · next check in 1 hour")
        );

        let got = hero(std::slice::from_ref(&fine), Some("the skills folder is a file"));
        assert_eq!((got.title.as_str(), got.sub.as_str()), ("Tiny Skill Sync has a problem", "The skills folder is a file"));

        assert_eq!(hero(&[], None).title, "Add a source to start");
        assert_eq!(hero(&[SourceView { enabled: false, ..fine.clone() }], None).title, "Every repository is paused");
        let empty =
            SourceView { level: Level::Attention, status: "No skills found".to_string(), detail: Some("…".to_string()), ..skills_view("o/e", Vec::new()) };
        assert_eq!(hero(&[fine.clone(), empty], None).title, "1 source needs your attention");
        let fresh = SourceView { last_check: None, ..skills_view("o/f", Vec::new()) };
        assert_eq!(
            hero(&[fresh], None),
            Hero { tone: Tone::Off, glyph: glyph::SYNC, title: "Waiting for the first check".to_string(), sub: "next check in 1 hour".to_string() }
        );

        let busy = hero_of(&[fine, needy], None, "Checking for updates…", true, NOW);
        assert_eq!(
            (busy.title.as_str(), busy.sub.as_str()),
            ("1 skill needs your attention", "Checking for updates…"),
            "news replaces the summary, not the title"
        );
    }

    #[test]
    fn issue7_dashboard_waits_for_both_same_repo_sources() {
        let mut sources = [
            source_view("team/tools@main : agents", vec![kind_view(SourceKind::Agents, "agents", vec![ok("review.agent.md")])]),
            source_view("team/tools@main : rules", vec![kind_view(SourceKind::Instructions, "rules", vec![ok("style.instructions.md")])]),
        ];
        let complete = hero_of(&sources, None, "", false, NOW);
        assert_eq!(complete.tone, Tone::Ok, "both sources checked: {complete:?}");

        sources[1].last_check = None;
        sources[1].status = "Not checked yet".into();
        sources[1].kinds[0].rows.clear();
        let partial = hero_of(&sources, None, "", false, NOW);
        assert!(partial.tone != Tone::Ok && partial.title != complete.title, "one source checked: {partial:?}; both checked: {complete:?}");
    }

    /// Source `o/fixtures` with a lane for every type, all on.
    fn fixtures_cfg() -> Config {
        let mut cfg = Config::default();
        let (id, lanes) = crate::sources::allocate_source_ids("fixtures", &mut cfg.taken_ids());
        cfg.add_source_config(SourceConfig { id, repo: "o/fixtures".into(), path: String::new(), branch: "main".into(), lanes, ..SourceConfig::default() })
            .unwrap();
        cfg
    }

    /// A board after one check of every lane of `o/fixtures` that ended with `outcome`. A check that got an answer read
    /// a listing with these skills and agents in it, all up to date, and nothing else.
    fn fixtures_board(cfg: &Config, outcome: crate::engine::Outcome, skills: &[&str], agents: &[&str]) -> Board {
        use crate::engine::{ApplyResult, Outcome, RepoReport, SkillReport, SyncReport};
        let answered = !matches!(outcome, Outcome::Failed(_));
        let lane = |id: &str, names: &[&str]| RepoReport {
            repo_id: id.to_string(),
            label: "o/fixtures@main".to_string(),
            outcome: outcome.clone(),
            skills: names
                .iter()
                .map(|n| SkillReport { name: n.to_string(), verdict: crate::reconcile::Verdict::InSync, result: ApplyResult::NotApplicable })
                .collect(),
            plugins: Vec::new(),
            rejected: Vec::new(),
            stats: crate::verify::Stats::default(),
            downloaded: 0,
            finished_at: NOW - 120,
            lists_nothing: answered && names.is_empty(),
        };
        let repos = vec![lane("fixtures", skills), lane("fixtures-agents", agents), lane("fixtures-instructions", &[]), lane("fixtures-plugins", &[])];
        let found = [skills.len() as u32, agents.len() as u32, 0, 0];
        let summary = crate::inventory::Summary { found, installable: found, ..Default::default() };
        let seen = if answered { vec![("fixtures".to_string(), crate::seen::Seen { summary, ..Default::default() })] } else { Vec::new() };
        let mut board = Board::new();
        board.note_source_sync(&SyncReport { repos, notes: Vec::new(), warnings: Vec::new(), seen }, NOW, cfg);
        board
    }

    #[test]
    fn issue8_the_hero_counts_one_skills_only_source_as_one_source() {
        let cfg = fixtures_cfg();
        let board = fixtures_board(&cfg, crate::engine::Outcome::Unchanged, &["alpha", "beta", "gamma"], &[]);
        let got = hero_of(&board.source_views(&cfg, NOW), None, "Next check in 1 hour", false, NOW);
        assert_eq!((got.tone, got.title.as_str()), (Tone::Ok, "All skills are up to date"), "{got:?}");
        assert_eq!(got.sub.split(" · ").next(), Some("3 skills from 1 source"), "{got:?}");
    }

    #[test]
    fn issue8_the_hero_counts_one_mixed_source_as_one_source_with_all_its_items() {
        let cfg = fixtures_cfg();
        let board = fixtures_board(&cfg, crate::engine::Outcome::Unchanged, &["alpha", "beta", "gamma"], &["review.agent.md"]);
        let got = hero_of(&board.source_views(&cfg, NOW), None, "Next check in 1 hour", false, NOW);
        assert_eq!((got.tone, got.title.as_str()), (Tone::Ok, "All items are up to date"), "types that list nothing need no attention: {got:?}");
        assert_eq!(got.sub.split(" · ").next(), Some("4 items from 1 source"), "{got:?}");
    }

    #[test]
    fn issue8_the_hero_counts_one_source_that_cannot_be_read_as_one_source() {
        let cfg = fixtures_cfg();
        let failure = crate::engine::Failure { kind: crate::github::FailureKind::Network, message: "no network".to_string(), retry_at: None };
        let board = fixtures_board(&cfg, crate::engine::Outcome::Failed(failure), &[], &[]);
        let got = hero_of(&board.source_views(&cfg, NOW), None, "Next check in 1 hour", false, NOW);
        assert_eq!((got.tone, got.title.as_str()), (Tone::Err, "1 source can’t be read"), "{got:?}");
    }

    #[test]
    fn issue8_the_hero_does_not_call_types_that_are_off_up_to_date() {
        let status = "Next check in 1 hour";
        let off = |v: SourceView| SourceView { kinds: v.kinds.into_iter().map(|k| KindView { on: false, ..k }).collect(), ..v };
        let skills_off = off(skills_view("o/a", vec![ok("x"), ok("y"), ok("z")]));
        let got = hero_of(std::slice::from_ref(&skills_off), None, status, false, NOW);
        assert_eq!((got.tone, got.glyph, got.title.as_str()), (Tone::Off, glyph::PAUSE, "Nothing is being synced"), "{got:?}");
        assert_eq!(got.sub, "Sync is off for every type found · checked 2 minutes ago · next check in 1 hour");
        let busy = hero_of(std::slice::from_ref(&skills_off), None, "Checking for updates…", true, NOW);
        assert_eq!((busy.title.as_str(), busy.sub.as_str()), ("Nothing is being synced", "Checking for updates…"));

        let agents = source_view("o/b", vec![kind_view(SourceKind::Agents, "o/b", vec![ok("review.agent.md")])]);
        let got = hero_of(&[skills_off, agents], None, status, false, NOW);
        assert_eq!((got.tone, got.title.as_str()), (Tone::Ok, "All agents are up to date"), "only what syncs is counted: {got:?}");
    }

    #[test]
    fn the_top_mark_has_the_colours_of_the_marks_in_the_list_and_a_ring_on_the_band() {
        let mark = [24, 16, 40, 40];
        let top = |tone| hero(&Hero { tone, glyph: glyph::OK, title: "Title".to_string(), sub: String::new() }, true, 800, &fit);
        for tone in [Tone::Ok, Tone::Warn, Tone::Err] {
            let s = top(tone);
            assert!(s.decos.contains(&Deco::Fill { at: mark, radius: 20, fill: tone.dot(), border: Role::BandDotRing }), "{tone:?}: {:?}", s.decos);
            assert!(s.decos.contains(&Deco::Glyph { at: mark, ch: glyph::OK, size: 18, ink: tone.dot_ink() }), "{tone:?}: {:?}", s.decos);
        }
        let calm = top(Tone::Off);
        assert!(calm.decos.contains(&Deco::Fill { at: mark, radius: 20, fill: Tone::Off.bg(), border: Role::BandDotRing }), "{:?}", calm.decos);
        assert!(calm.decos.contains(&Deco::Glyph { at: mark, ch: glyph::OK, size: 18, ink: Tone::Off.ink() }), "{:?}", calm.decos);
    }

    #[test]
    fn the_band_covers_the_top_of_both_views_with_one_line_under_it() {
        for w in [640, 980, 1400] {
            let main = backdrop(split_layout(w, HERO_H, 480 - HERO_H, NAV_W).page[0], w, 480);
            let diff = diff_backdrop(&diff_layout(w, 480, NAV_W), w, 480);
            for (view, decos) in [("main", &main), ("diff", &diff)] {
                let find = |wanted: &Deco| decos.iter().position(|d| d == wanted);
                let paper = find(&Deco::Fill { at: [0, 0, w, HERO_H], radius: 0, fill: Role::Band, border: Role::None });
                let print = find(&Deco::BandPrint { at: [0, 0, w, HERO_H] });
                let line = find(&Deco::Fill { at: [0, HERO_H - 1, w, 1], radius: 0, fill: Role::BandEdge, border: Role::None });
                let layer = decos.iter().position(|d| matches!(d, Deco::Fill { fill: Role::Layer, .. }));
                let (Some(layer), Some(paper), Some(print), Some(line)) = (layer, paper, print, line) else { panic!("{view} at {w}: no band in {decos:?}") };
                assert!(layer < paper && paper < print && print < line, "{view} at {w}: the layer, paper, print and line: {decos:?}");
                assert_eq!(decos.iter().filter(|d| matches!(d, Deco::Fill { fill: Role::BandEdge, .. })).count(), 1, "{view} at {w}");
                for (k, d) in decos.iter().enumerate() {
                    let Deco::Fill { at, radius, border, .. } = d else { continue };
                    if k < paper && (*border != Role::None || *radius > 0) {
                        assert!(at[1] + radius < HERO_H - 1, "{view} at {w}: the band hides the top edge and corners of {d:?}");
                    }
                    if k > line {
                        assert!(at[1] > HERO_H, "{view} at {w}: {d:?} is drawn on the band or against its line");
                    }
                }
            }
        }
    }

    #[test]
    fn the_hero_and_the_diff_header_sit_centred_in_the_band() {
        let in_the_band = |s: &Surface, w: i32, what: &str| {
            fits_in(s, w);
            for i in &s.items {
                assert!(i.at[1] >= 0 && i.at[1] + i.at[3] < HERO_H, "{what} at {w}: {} ({:?}) at {:?} is not in the band", i.id, i.text, i.at);
            }
        };
        let centred = |top: i32, bottom: i32| top == HERO_H - bottom;
        let h = Hero {
            tone: Tone::Warn,
            glyph: glyph::WARN,
            title: "3 skills need your attention".to_string(),
            sub: "In o/a · checked 2 minutes ago · next check in 1 hour".to_string(),
        };
        for w in [640, 980, 1400] {
            let s = hero(&h, true, w, &fit);
            in_the_band(&s, w, "the hero");
            let (title, sub, sync) = (item(&s, id::HERO_TITLE), item(&s, id::HERO_SUB), item(&s, id::SYNC));
            assert_eq!(sync.look, Look::Button { kind: Kind::BandAccent, glyph: Some(glyph::SYNC) });
            assert_eq!([title.at[1], sub.at[1], sub.at[1] + sub.at[3], sync.at[1]], [14, 42, 58, 20], "at {w}");
            assert!(centred(title.at[1], sub.at[1] + sub.at[3]) && centred(sync.at[1], sync.at[1] + sync.at[3]), "at {w}");
            let mark = [24, 16, 40, 40];
            assert!(s.decos.iter().any(|d| matches!(d, Deco::Fill { at, .. } if *at == mark)) && centred(mark[1], mark[1] + mark[3]));

            let l = diff_layout(w, 480, NAV_W);
            assert!(l.files_card[1] > HERO_H && l.code_card[1] > HERO_H, "at {w}: the cards start below the band");
            let choices = diff_of("a\n", "b\n");
            let preview = DiffView { preview: true, ..choices.clone() };
            for (what, v) in [("the diff header", &choices), ("the preview header", &preview)] {
                let s = diff_head(v, &l, &caption_fit);
                in_the_band(&s, w, what);
                let row: Vec<&Item> = s.items.iter().filter(|i| i.id == id::BACK || matches!(hit(i.id), Hit::DiffAction(_))).collect();
                assert_eq!(row.len(), if v.preview { 1 } else { 3 }, "{what} at {w}: Back and the choices");
                let (title, sub) = (item(&s, id::TITLE), item(&s, id::SUB));
                if !v.preview && w == 640 {
                    // The sentence is too long to go beside two choices, so it goes under them.
                    assert!(row.iter().all(|i| [i.at[1], i.at[3]] == [8, 32]), "{what} at {w}: not on one row at the top: {row:?}");
                    assert_eq!((title.at[1], sub.at), (10, [56, 44, w - 80, 16]), "{what} at {w}");
                } else {
                    let on_row = |i: &&Item| [i.at[1], i.at[3]] == [20, 32] && centred(i.at[1], i.at[1] + i.at[3]);
                    assert!(row.iter().all(on_row), "{what} at {w}: not on one centred row: {row:?}");
                    assert_eq!([title.at[1], sub.at[1], sub.at[1] + sub.at[3]], [14, 42, 58], "{what} at {w}");
                    assert!(centred(title.at[1], sub.at[1] + sub.at[3]), "{what} at {w}");
                }
                for d in &s.decos {
                    let [_, y, _, dh] = deco_at(d);
                    let station = matches!(d, Deco::Fill { fill: Role::Band, .. } | Deco::Glyph { ink: Role::BandMark, .. });
                    assert!(
                        station || y == title.at[1] && dh == title.at[3] || y >= DIFF_TOP,
                        "{what} at {w}: {d:?} is beside the title or in a card's header"
                    );
                }
            }
        }
    }

    /// The type icons on the band, each with the disc under it, from left to right.
    fn stations_of(s: &Surface) -> Vec<(Rect, char)> {
        let mut out = Vec::new();
        for (k, d) in s.decos.iter().enumerate() {
            if let Deco::Glyph { at, ch, size: 12, ink: Role::BandMark } = d {
                let disc = Deco::Fill { at: *at, radius: STATION_R, fill: Role::Band, border: Role::None };
                assert_eq!(k.checked_sub(1).map(|p| &s.decos[p]), Some(&disc), "each icon sits on its disc");
                out.push((*at, *ch));
            }
        }
        assert_eq!(s.decos.iter().filter(|d| matches!(d, Deco::Fill { fill: Role::Band, .. })).count(), out.len(), "a disc without its icon");
        out
    }

    /// Each disc keeps 12 DIPs to either side clear of these.
    fn clear_of(got: &[(Rect, char)], taken: &[Rect], what: &str) {
        for ([x, y, dw, dh], _) in got {
            for t in taken {
                assert!(!overlaps([x - 12, *y, dw + 24, *dh], *t), "{what}: the disc at {:?} is too close to {t:?}", [x, y, dw, dh]);
            }
        }
    }

    #[test]
    fn the_type_icons_ride_the_middle_hill_clear_of_the_heading_and_the_button() {
        let order = [SourceKind::Skills, SourceKind::Agents, SourceKind::Instructions, SourceKind::Plugins].map(kind_glyph);
        let h = Hero {
            tone: Tone::Ok,
            glyph: glyph::OK,
            title: "All skills are up to date".to_string(),
            sub: "12 skills from 2 sources · checked 2 minutes ago · next check in 1 hour".to_string(),
        };
        for w in [860, 980, 1400] {
            let s = hero(&h, true, w, &caption_fit);
            let got = stations_of(&s);
            assert_eq!(got.iter().map(|(_, ch)| *ch).collect::<Vec<_>>(), order, "at {w}: the four types, in the list's order");
            let mut left = 0;
            for ([x, y, dw, dh], _) in &got {
                let (cx, cy) = (x + dw / 2, y + dh / 2);
                let top = HERO_H as f32 - hill(2, (w - cx) as f32);
                assert!((cy as f32 - top - 2.0).abs() <= 1.0, "at {w}: the disc at {cx},{cy} is not just under the middle hill's top, {top}");
                assert!(*x > left, "at {w}: the discs are apart, from left to right");
                left = x + dw;
            }
            let (title, sub) = (item(&s, id::HERO_TITLE).at, item(&s, id::HERO_SUB).at);
            let text =
                [[title[0], title[1], caption_fit(&h.title, Font::Title, 0).0, title[3]], [sub[0], sub[1], caption_fit(&h.sub, Font::Caption, 0).0, sub[3]]];
            clear_of(&got, &[text[0], text[1], item(&s, id::SYNC).at], &format!("at {w}"));
        }
    }

    #[test]
    fn a_heading_that_needs_the_room_hides_every_type_icon_never_some() {
        for w in [640, 860, 1400] {
            let mut seen = [false; 2];
            for n in 0..=180 {
                let h = Hero { tone: Tone::Warn, glyph: glyph::WARN, title: "x".repeat(n), sub: String::new() };
                let got = stations_of(&hero(&h, true, w, &fit)).len();
                assert!(got == 0 || got == 4, "at {w}, a title of {n}: {got} icons");
                seen[usize::from(got == 4)] = true;
            }
            assert_eq!(seen, [true; 2], "at {w}: a short title shows the icons and a long one hides them");
        }
    }

    #[test]
    fn a_preview_shows_the_type_icons_and_the_choices_of_show_changes_hide_them() {
        let choices = diff_of("a\n", "b\n");
        let preview = DiffView { preview: true, ..choices.clone() };
        // From the window's first width up; narrower, the preview's long sentence can need the hill's room.
        for w in [980, 1200, 1400] {
            let l = diff_layout(w, 600, NAV_W);
            let s = diff_head(&preview, &l, &caption_fit);
            let got = stations_of(&s);
            assert_eq!(got.len(), 4, "at {w}: a preview offers no choices, so the hill has room");
            let (title, sub) = (item(&s, id::TITLE).at, item(&s, id::SUB).at);
            let mut taken = vec![
                [56, title[1], title[0] + fit(&choices.skill, Font::Title, 0).0 - 56, title[3]],
                [sub[0], sub[1], caption_fit(&diff_sub(&preview), Font::Caption, 0).0, sub[3]],
            ];
            taken.extend(s.items.iter().filter(|i| matches!(i.look, Look::Button { .. })).map(|i| i.at));
            clear_of(&got, &taken, &format!("the preview at {w}"));
            assert!(stations_of(&diff_head(&choices, &l, &caption_fit)).is_empty(), "at {w}: the choices cross the hill");
        }
    }

    // At 640 wide the sentence beside two choices had room for about half of itself, so "Nothing changes until you
    // choose" was cut off.
    #[test]
    fn the_diff_header_shows_its_whole_sentence_beside_or_under_the_choices() {
        let base = diff_of("a\n", "b\n");
        let Report::Ready(found) = &base.report else { unreachable!() };
        let ready = |on_github, here, files: &[FileChange]| Report::Ready(DiffReport { on_github, here, files: files.to_vec() });
        let reports = [
            Report::Loading,
            Report::Failed("GitHub could not be reached".to_string()),
            ready(true, true, &[]),
            ready(true, false, &found.files),
            ready(false, true, &found.files),
            ready(true, true, &found.files),
        ];
        let agents = RepoConfig { kind: SourceKind::Agents, ..base.source.clone() };
        let (mut sentences, mut tops) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
        for report in &reports {
            for source in [&base.source, &agents] {
                for preview in [false, true] {
                    let v = DiffView { source: source.clone(), preview, report: report.clone(), ..base.clone() };
                    for w in [640, 980, 1400] {
                        let s = diff_head(&v, &diff_layout(w, 480, NAV_W), &caption_fit);
                        fits_in(&s, w);
                        for i in &s.items {
                            assert!(i.at[1] >= 0 && i.at[1] + i.at[3] < HERO_H, "{} ({:?}) at {:?} is not in the band at {w}", i.id, i.text, i.at);
                        }
                        let sub = item(&s, id::SUB);
                        let needed = caption_fit(&sub.text, Font::Caption, 0).0;
                        assert!(needed <= sub.at[2], "at {w}, {:?} needs {needed} but has {:?}", sub.text, sub.at);
                        sentences.insert(sub.text.clone());
                        tops.insert(sub.at[1]);
                    }
                }
            }
        }
        assert_eq!(sentences.len(), 10, "every sentence the header can show: {sentences:?}");
        assert_eq!(tops.into_iter().collect::<Vec<_>>(), [42, 44], "both the centred and the stacked header");
    }

    /// A source with all four types: three skills, an agent, instructions that do not sync and a plugin to approve.
    fn four_types() -> SourceView {
        let approve = Row { plugin: Some(board::PluginDetails::default()), ..row("p", Level::Attention, "Needs your approval", vec![Action::ApprovePlugin]) };
        source_view(
            "o/a",
            vec![
                KindView { found: Some(3), ..kind_view(SourceKind::Skills, "o/a", vec![ok("x")]) },
                KindView { found: Some(1), ..kind_view(SourceKind::Agents, "o/a-agents", vec![ok("r.agent.md")]) },
                KindView { on: false, ..kind_view(SourceKind::Instructions, "o/a-instructions", vec![ok("s.instructions.md")]) },
                kind_view(SourceKind::Plugins, "o/a-plugins", vec![approve]),
            ],
        )
    }

    /// A list of one source at width `w`, as `caption_fit` measures it: that source's entry, and what it draws.
    fn entry_in_list(v: &SourceView, w: i32) -> (Item, Vec<Deco>) {
        let s = nav(std::slice::from_ref(v), &Page::Activity, w, 600, &caption_fit);
        let entry = item(&s, id::NAV_REPO).clone();
        let Look::Source { parts, .. } = &entry.look else { panic!("{:?}", entry.look) };
        let parts = parts.clone();
        (entry, parts)
    }

    /// What an entry draws is inside it. The only things that share a place are a chip and what is written on it; no
    /// text or icon touches another, and no chip another chip.
    fn assert_entry_is_tidy(entry: &Item, parts: &[Deco], at: &str) {
        let [_, _, w, h] = entry.at;
        let apart = |a: Rect, b: Rect| a[0] + a[2] <= b[0] || b[0] + b[2] <= a[0] || a[1] + a[3] <= b[1] || b[1] + b[3] <= a[1];
        let within = |a: Rect, b: Rect| a[0] >= b[0] && a[1] >= b[1] && a[0] + a[2] <= b[0] + b[2] && a[1] + a[3] <= b[1] + b[3];
        for (k, d) in parts.iter().enumerate() {
            let a = deco_at(d);
            assert!(a[2] > 0 && a[3] > 0 && within(a, [0, 0, w, h]), "{d:?} is outside the {w}x{h} entry at {at}");
            for e in &parts[k + 1..] {
                let b = deco_at(e);
                let fine = match (d, e) {
                    (Deco::Fill { .. }, Deco::Fill { .. }) => apart(a, b),
                    (Deco::Fill { .. }, _) => apart(a, b) || within(b, a),
                    (_, Deco::Fill { .. }) => apart(a, b) || within(a, b),
                    _ => apart(a, b),
                };
                assert!(fine, "{d:?} and {e:?} collide at {at}");
            }
        }
    }

    /// The first line of an entry: the owner's text when it is shown, and the repository's.
    fn title_of(parts: &[Deco]) -> (Option<&Deco>, &Deco) {
        let find = |font: Font| parts.iter().find(|d| matches!(d, Deco::Text { font: f, .. } if *f == font));
        (find(Font::Body), find(Font::Strong).expect("the repository"))
    }

    #[test]
    fn the_list_has_one_entry_per_source_with_a_chip_per_type() {
        let v = four_types();
        assert_eq!(nav_name(&v), "o/a, main, skills. Skills 3, Agents 1, Instructions: Sync off, Plugins 1 (needs approval). Up to date");
        let page = Page::Repo("o/a".to_string());
        for w in [NAV_W, 400] {
            let s = nav(std::slice::from_ref(&v), &page, w, 600, &caption_fit);
            fits_in(&s, w);
            let entry = item(&s, id::NAV_REPO);
            assert_eq!(entry.at[3], ENTRY_H, "a source uses only two text lines");
            assert_eq!(entry.text, nav_name(&v), "the window text spells out what the chips show");
            let Look::Source { selected: true, parts, .. } = &entry.look else { panic!("{:?}", entry.look) };
            let icons: Vec<(char, Rect)> = parts
                .iter()
                .filter_map(|d| match d {
                    Deco::Glyph { ch, at, .. } if *ch != glyph::WARN => Some((*ch, *at)),
                    _ => None,
                })
                .collect();
            assert_eq!(icons.iter().map(|i| i.0).collect::<Vec<_>>(), [glyph::SKILL, glyph::AGENT, glyph::INSTRUCTION, glyph::PLUGIN]);
            let labels: Vec<&str> = parts
                .iter()
                .filter_map(|d| match d {
                    Deco::Text { text, font: Font::Caption, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(labels, ["3", "1", "off", "1"], "a source that is up to date says nothing else on its second line");
            assert_eq!(parts.iter().filter(|d| matches!(d, Deco::Glyph { ch: glyph::WARN, .. })).count(), 1, "the plugin to approve is marked");
            assert_entry_is_tidy(entry, parts, &w.to_string());
            let lines: HashSet<i32> = icons.iter().map(|i| i.1[1]).collect();
            assert_eq!(lines.len(), 1, "the chips never wrap ({w}): {icons:?}");
            assert!(!parts.iter().any(|d| matches!(d, Deco::Text { text, .. } if text.contains("main") || text.contains("skills"))));
            assert!(item(&s, id::NAV_ADD).at[1] > entry.at[1] + entry.at[3], "rows never overlap");
        }

        let fresh = SourceView {
            last_check: None,
            status: "Not checked yet".to_string(),
            branch: String::new(),
            folder: String::new(),
            ..source_view("o/b", Vec::new())
        };
        assert_eq!(nav_name(&fresh), "o/b, default branch, whole repository. Not checked yet");
        let s = nav(std::slice::from_ref(&fresh), &Page::Activity, NAV_W, 600, &fit);
        let Look::Source { parts, selected: false, .. } = &item(&s, id::NAV_REPO).look else { panic!() };
        assert!(!parts.iter().any(|d| matches!(d, Deco::Glyph { .. } | Deco::Fill { .. })), "no chips before the first listing");
        assert!(parts.iter().any(|d| matches!(d, Deco::Text { text, .. } if text == "Not checked yet")));
        assert_eq!(item(&s, id::NAV_ADD).text, "Add source");
    }

    #[test]
    fn narrow_source_entries_keep_two_lines_and_expose_overflow_in_the_full_label() {
        for wide_counts in [false, true] {
            for status in ["Disabled", "Needs attention (2)"] {
                let mut v = SourceView { status: status.into(), repo: "owner/a-very-long-repository-name".into(), ..four_types() };
                if wide_counts {
                    for k in &mut v.kinds {
                        k.found = Some(u32::MAX);
                    }
                }
                let needed = caption_fit(status, Font::Caption, 0).0;
                for w in MIN_LIST_W..=400 {
                    let (entry, parts) = entry_in_list(&v, w);
                    assert_eq!(entry.at[3], ENTRY_H);
                    assert_eq!(entry.text, nav_name(&v), "no information is lost from the tooltip or accessible name");
                    assert_entry_is_tidy(&entry, &parts, &format!("{w} wide with {status:?}"));
                    let chips = parts.iter().any(|d| matches!(d, Deco::Fill { .. }));
                    let said = parts.iter().find(|d| matches!(d, Deco::Text { text, .. } if text == status)).expect("the status is shown");
                    if chips {
                        assert!(deco_at(said)[2] >= needed, "the chips give way to the status at {w}: {said:?}");
                        assert!(parts.iter().any(|d| matches!(d, Deco::Glyph { ch: glyph::WARN, .. })), "overflow never hides a warning at {w}");
                    }
                    if w == MIN_LIST_W && status == "Disabled" {
                        assert!(parts.iter().any(|d| matches!(d, Deco::Text { text, .. } if text.starts_with('+'))), "hidden types are signposted");
                    }
                }
            }
        }
    }

    #[test]
    fn an_entry_names_the_repository_in_strong_type_and_its_owner_gives_way_first() {
        let named = |repo: &str| skills_view(repo, vec![ok("x")]);
        let (_, parts) = entry_in_list(&named("owner/repository"), 400);
        let (owner, name) = title_of(&parts);
        let owner = owner.expect("an owner");
        assert_eq!((deco_text(owner).as_str(), deco_text(name).as_str()), ("owner/", "repository"));
        assert!(matches!(owner, Deco::Text { ink: Role::Text2, .. }), "the owner is dimmed");
        assert!(matches!(name, Deco::Text { ink: Role::Text, .. }), "the repository has the full ink");
        assert_eq!(deco_at(owner)[0] + deco_at(owner)[2], deco_at(name)[0], "the repository follows its owner");
        assert_eq!(deco_at(owner)[1], deco_at(name)[1]);

        // The room is for the repository first: the owner shortens, then goes, and only then is the repository cut.
        let long = named("organization/a-long-repository-name");
        let name_w = caption_fit("a-long-repository-name", Font::Strong, 0).0;
        let owner_w = caption_fit("organization/", Font::Body, 0).0;
        let mut owners = Vec::new();
        for w in MIN_LIST_W..=400 {
            let (_, parts) = entry_in_list(&long, w);
            let (owner, name) = title_of(&parts);
            let shown = owner.map_or(0, |o| deco_at(o)[2]);
            if owner.is_some() {
                assert!(deco_at(name)[2] >= name_w, "at {w} the repository is cut while its owner is shown");
                assert!(shown >= owner_w || w - 16 - ENTRY_LEFT - ENTRY_RIGHT - name_w >= MIN_OWNER, "at {w} the owner has too little room to be shown");
            } else {
                assert_eq!(deco_text(name), "a-long-repository-name");
                assert_eq!(deco_at(name), [ENTRY_LEFT, TITLE_TOP, w - 16 - ENTRY_LEFT - ENTRY_RIGHT, TITLE_H], "the repository has the whole line");
            }
            owners.push(shown);
        }
        assert!(owners.windows(2).all(|pair| pair[0] <= pair[1]), "a wider list never shows less of the owner: {owners:?}");
        assert_eq!(owners[0], 0, "the narrowest list shows the repository alone");
        assert!(owners.iter().any(|&shown| shown > 0 && shown < owner_w), "the owner shortens before it goes");
        assert!(owners.last().is_some_and(|&shown| shown >= owner_w), "and a wide list shows all of it");

        for plain in ["monorepo", "/x", "o/"] {
            let (_, parts) = entry_in_list(&named(plain), 400);
            let (owner, name) = title_of(&parts);
            assert!(owner.is_none(), "{plain:?} has no owner to show");
            assert_eq!(deco_text(name), plain);
            assert!(matches!(name, Deco::Text { font: Font::Strong, ink: Role::Text, .. }));
        }
        let (_, parts) = entry_in_list(&named("ow\u{202e}ner/re\u{202e}po"), 400);
        assert!(parts.iter().all(|d| !deco_text(d).contains('\u{202e}')), "the text is made printable");
    }

    /// The owner's text is cut here, not by the painter, which would leave a gap of its own size after the letters.
    #[test]
    fn a_shortened_owner_is_cut_by_the_page_and_the_repository_follows_it_by_one_space() {
        let repo = skills_view("organization/a-long-repository-name", vec![ok("x")]);
        for per in [5, 7, 9] {
            // As in a real font, a space is narrower than a letter and a bold letter is wider, so measuring the wrong
            // one shows.
            let by = |text: &str, font: Font, width: i32| {
                let letter = if font == Font::Strong { per + 2 } else { per };
                let w: i32 = text.chars().map(|c| if c == ' ' { per / 2 } else { letter }).sum();
                let lh = line_height(font);
                if width <= 0 { (w, lh) } else { (w.min(width), ((w + width - 1) / width).max(1) * lh) }
            };
            let (space, name_w) = (by(" ", Font::Body, 0).0, by("a-long-repository-name", Font::Strong, 0).0 + 1);
            let (mut whole, mut cut, mut dropped) = (0, 0, 0);
            for w in MIN_LIST_W..=520 {
                let room = w - 16 - ENTRY_LEFT - ENTRY_RIGHT;
                let s = nav(std::slice::from_ref(&repo), &Page::Activity, w, 600, &by);
                let Look::Source { parts, .. } = &item(&s, id::NAV_REPO).look else { panic!() };
                let (owner, name) = title_of(parts);
                let case = format!("{per} DIPs a letter, {w} wide");
                let want = {
                    let width = |text: &str| by(text, Font::Body, 0).0;
                    let owner_room = room - name_w;
                    let fits_whole = width("organization/") + 1 + name_w <= room;
                    let shortened = (1..12).rev().map(|kept| format!("{}…", &"organization"[..kept])).find(|cut| width(cut) <= owner_room - space);
                    if fits_whole {
                        Some("organization/".to_string())
                    } else if owner_room >= MIN_OWNER {
                        shortened
                    } else {
                        None
                    }
                };
                let Some(owner) = owner else {
                    assert_eq!(want, None, "an owner that fits is shown: {case}");
                    assert_eq!(deco_at(name), [ENTRY_LEFT, TITLE_TOP, room, TITLE_H], "the repository has the whole line: {case}");
                    dropped += 1;
                    continue;
                };
                let (text, [x, _, box_w, _]) = (deco_text(owner), deco_at(owner));
                let drawn = by(&text, Font::Body, 0).0;
                assert_eq!(box_w, drawn + 1, "{text:?} needs {drawn} and its box is {box_w}, so the painter cuts it, or leaves a gap, at {case}");
                assert_eq!(Some(&text), want.as_ref(), "{case}");
                let [name_x, _, name_room, _] = deco_at(name);
                assert_eq!(name_x + name_room, ENTRY_LEFT + room, "the repository has what is left of the line: {case}");
                assert!(name_room >= name_w, "the repository is whole: {case}");
                assert!([owner, name].iter().all(|d| matches!(d, Deco::Text { flags, .. } if *flags == ONE_LINE)), "the title is one line: {case}");
                if text.ends_with('/') {
                    whole += 1;
                    assert_eq!(name_x, x + box_w, "the slash is what separates the repository from its owner: {case}");
                } else {
                    cut += 1;
                    assert_eq!(name_x, x + drawn + space, "the repository starts one space after the letters that are drawn: {case}");
                }
            }
            assert!(whole > 0 && cut > 0 && dropped > 0, "every case is met ({per} DIPs a letter): {whole} whole, {cut} cut, {dropped} dropped");
        }
    }

    #[test]
    fn every_source_entry_has_one_height_whatever_it_shows() {
        let fresh = SourceView { last_check: None, status: "Not checked yet".to_string(), ..source_view("o/fresh", Vec::new()) };
        let paused = SourceView { enabled: false, status: "Disabled".to_string(), ..four_types() };
        let long = skills_view("AngelosP/tiny-skill-sync-fixtures-with-a-long-name", vec![ok("x")]);
        let sources = [four_types(), fresh, unreadable_source("o/broken"), paused, skills_view("monorepo", vec![ok("x")]), long];
        for w in [MIN_LIST_W, NAV_W, 400] {
            let s = nav(&sources, &Page::Activity, w, 600, &caption_fit);
            fits_in(&s, w);
            let entries: Vec<&Item> = s.items.iter().filter(|i| matches!(i.look, Look::Source { .. })).collect();
            assert_eq!(entries.len(), sources.len());
            assert!(entries.iter().all(|e| e.at[3] == ENTRY_H), "at {w}: {:?}", entries.iter().map(|e| e.at[3]).collect::<Vec<_>>());
            for (pair, v) in entries.windows(2).zip(&sources) {
                assert_eq!(pair[1].at[1], pair[0].at[1] + ENTRY_H + 2, "{} is followed by the next entry", v.repo);
            }
            let last = entries.last().expect("entries");
            assert!(item(&s, id::NAV_ADD).at[1] >= last.at[1] + last.at[3], "Add source is under the last entry");
        }
    }

    #[test]
    fn a_type_that_needs_the_user_has_a_warm_chip_that_is_kept_before_the_calm_ones() {
        let (_, parts) = entry_in_list(&four_types(), 400);
        let fills: Vec<(Role, Role)> = parts
            .iter()
            .filter_map(|d| match d {
                Deco::Fill { fill, border, .. } => Some((*fill, *border)),
                _ => None,
            })
            .collect();
        assert_eq!(
            fills,
            [(Role::Ctl, Role::CtlBorder), (Role::Ctl, Role::CtlBorder), (Role::WarnBg, Role::CtlBorder)],
            "skills and agents, then the plugin to approve; instructions that are off have no chip"
        );
        let warm: Vec<char> = parts
            .iter()
            .filter_map(|d| match d {
                Deco::Glyph { ch, ink: Role::Warn, .. } => Some(*ch),
                _ => None,
            })
            .collect();
        assert_eq!(warm, [glyph::PLUGIN, glyph::WARN], "the plugin's icon and the sign share the warm ink");
        assert!(parts.iter().any(|d| matches!(d, Deco::Text { text, ink: Role::Warn, .. } if text == "1")));
        assert!(parts.iter().any(|d| matches!(d, Deco::Text { text, ink: Role::Text3, .. } if text == "off")), "a type that is off is muted");
        assert!(parts.iter().any(|d| matches!(d, Deco::Glyph { ch, ink: Role::Text3, .. } if *ch == glyph::INSTRUCTION)));

        // With little room the types that need the user stay, and the others are counted in a chip of their own.
        let v = SourceView { status: "Disabled".to_string(), ..four_types() };
        let mut hidden = 0;
        for w in MIN_LIST_W..=400 {
            let (entry, parts) = entry_in_list(&v, w);
            assert_entry_is_tidy(&entry, &parts, &w.to_string());
            let warm_chip = parts.iter().any(|d| matches!(d, Deco::Glyph { ch, ink: Role::Warn, .. } if *ch == glyph::PLUGIN));
            if warm_chip {
                continue;
            }
            hidden += 1;
            let fills: Vec<Role> = parts.iter().filter_map(|d| if let Deco::Fill { fill, .. } = d { Some(*fill) } else { None }).collect();
            assert_eq!(fills, [Role::WarnBg], "at {w} no calm chip is kept in front of the warning, which the count carries");
            assert!(parts.iter().any(|d| matches!(d, Deco::Text { text, .. } if text == "+4")));
            assert!(parts.iter().any(|d| matches!(d, Deco::Glyph { ch: glyph::WARN, .. })));
        }
        assert!(hidden > 0, "the narrowest list has no room for the plugin's chip beside the status");
    }

    #[test]
    fn a_source_that_is_up_to_date_has_no_status_text_and_every_other_state_says_so() {
        let healthy = skills_view("o/a", vec![ok("x")]);
        for w in [MIN_LIST_W, NAV_W, 400] {
            let (_, parts) = entry_in_list(&healthy, w);
            let words: Vec<String> = parts.iter().map(deco_text).filter(|t| !t.is_empty()).collect();
            assert_eq!(words, ["o/", "a", "1"], "only the repository and the count are written, the dot says the rest ({w})");
        }
        for (status, level, ink) in [
            ("Needs attention (2)", Level::Attention, Role::Warn),
            ("Not found", Level::Error, Role::Err),
            ("Not checked yet", Level::Ok, Role::Text2),
            ("Disabled", Level::Ok, Role::Text2),
            ("Changes pending", Level::Ok, Role::Text2),
        ] {
            let v = SourceView { status: status.to_string(), level, ..healthy.clone() };
            let (_, parts) = entry_in_list(&v, 400);
            let said = parts.iter().find(|d| deco_text(d) == status).unwrap_or_else(|| panic!("{status:?} is not said"));
            assert!(matches!(said, Deco::Text { font: Font::Caption, ink: i, .. } if *i == ink), "{status:?}: {said:?}");
        }
    }

    #[test]
    fn source_marks_use_the_soft_check_dot_only_when_synced() {
        let synced = skills_view("o/a", vec![ok("x")]);
        let mark_of = |v: &SourceView| {
            let (entry, _) = entry_in_list(v, NAV_W);
            let Look::Source { mark, .. } = &entry.look else { panic!("{:?}", entry.look) };
            *mark
        };
        let enabled = [
            ("checked and fine", synced.clone(), Mark::Dot(Tone::Ok)),
            ("asking for attention", SourceView { level: Level::Attention, ..synced.clone() }, Mark::Dot(Tone::Warn)),
            ("in error", SourceView { level: Level::Error, ..synced.clone() }, Mark::Dot(Tone::Err)),
            ("not checked yet", SourceView { last_check: None, ..synced.clone() }, Mark::Glyph(glyph::SYNC, Role::Text2)),
            ("in error on its first check", SourceView { level: Level::Error, last_check: None, ..synced.clone() }, Mark::Dot(Tone::Err)),
        ];
        for (state, v, want) in enabled {
            assert_eq!(mark_of(&v), want, "an enabled source that is {state}");
        }
        for level in [Level::Ok, Level::Attention, Level::Error] {
            for last_check in [Some(NOW - 120), None] {
                let paused = SourceView { enabled: false, level, last_check, ..synced.clone() };
                assert_eq!(mark_of(&paused), Mark::Dot(Tone::Off), "a paused source at {level:?}, last checked {last_check:?}, has the calm dot");
            }
        }
    }

    /// Every text a surface shows, in its controls and drawn on it.
    fn all_text(s: &Surface) -> Vec<String> {
        let drawn = s.decos.iter().filter_map(|d| match d {
            Deco::Text { text, .. } => Some(text.clone()),
            _ => None,
        });
        s.items.iter().map(|i| i.text.clone()).chain(drawn).collect()
    }

    #[test]
    fn a_sources_page_groups_its_items_by_type_and_each_row_leads_to_its_lane() {
        let mut v = four_types();
        v.kinds[0].rows.push(edited("y"));
        v.kinds[0].filtered = 2;
        v.kinds[1].notes.push(crate::inventory::SummaryNote {
            kind: Some(SourceKind::Agents),
            path: "x/b.agent.md".into(),
            reason: "the name is taken".into(),
        });
        v.notes.push(crate::inventory::SummaryNote { kind: None, path: "AGENTS.md".into(), reason: "it is never synced".into() });
        v.more_notes = 3;
        let mut cfg = source_cfg("o/a");
        cfg.lanes = cfg
            .lanes
            .with(SourceKind::Agents, "o/a-agents".into())
            .with(SourceKind::Instructions, "o/a-instructions".into())
            .with(SourceKind::Plugins, "o/a-plugins".into());
        for w in [344, 900] {
            let s = page_of(&v, &cfg, &HashSet::new(), w);
            fits_in(&s, w);
            let heads: Vec<(i32, &str)> = s
                .items
                .iter()
                .filter_map(|item| match &item.look {
                    Look::Section { title, .. } => {
                        let name = title.split_once(" (")?.0;
                        ["Skills", "Agents", "Instructions", "Plugins"].contains(&name).then_some((item.at[1], name))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(heads.iter().map(|h| h.1).collect::<Vec<_>>(), ["Skills", "Agents", "Instructions", "Plugins"]);
            assert!(heads.windows(2).all(|p| p[0].0 < p[1].0), "in that order down the page");
            let text = all_text(&s);
            assert!(text.iter().any(|t| t.contains("Skills (3)")));
            assert!(text.iter().any(|t| t.contains("Instructions (1 · Sync off)")));
            for expected in
                ["2 skipped by folder rules", "Not synced: x/b.agent.md: the name is taken", "Never synced: AGENTS.md: it is never synced", "and 3 more"]
            {
                assert!(text.iter().any(|t| t == expected), "{expected:?} is not in {text:?}");
            }
            assert!(!text.iter().any(|t| t.contains("Skills [")), "the sub line does not name a lane");
            // Rows are numbered across the sections.
            assert!(item(&s, skill_id(2, 0)).text.starts_with("r.agent.md\n"));
            assert!(item(&s, skill_id(3, 0)).text.starts_with("s.instructions.md\nUp to date"));
            assert!(matches!(&item(&s, skill_id(3, 0)).look, Look::Row { commands, .. } if commands.is_empty()), "a type that is off offers no comparison");
            assert_eq!(item(&s, skill_id(4, 4)).text, "Approve");
        }
        let (k, r) = row_at(&v, 2).unwrap();
        assert_eq!((k.lane.as_str(), r.name.as_str()), ("o/a-agents", "r.agent.md"));
        let (k, r) = row_at(&v, 4).unwrap();
        assert_eq!((k.kind, r.name.as_str()), (SourceKind::Plugins, "p"));
        assert!(row_at(&v, 5).is_none());
        assert_eq!(row_index(&v, "o/a-plugins", "p"), Some(4));
        assert_eq!(row_index(&v, "o/a", "r.agent.md"), None, "a name is looked for in its own lane");
        let lane = lane_config(&cfg, SourceKind::Agents, "o/a-agents");
        assert_eq!((lane.id.as_str(), lane.kind, lane.source.as_str(), lane.path.as_str()), ("o/a-agents", SourceKind::Agents, "o/a", "skills"));
    }

    #[test]
    fn what_to_sync_offers_the_default_on_and_off_for_every_type() {
        let v = skills_view("o/a", vec![ok("x")]);
        let mut cfg = source_cfg("o/a");
        cfg.types.set(SourceKind::Agents, false);
        for w in [344, 900] {
            let s = page_of(&v, &cfg, &HashSet::new(), w);
            fits_in(&s, w);
            assert_eq!(item(&s, id::SOURCE_TYPE).text, "Sync skills\nDefault (On)");
            assert_eq!(item(&s, id::SOURCE_TYPE + 1).text, "Sync agents\nOff");
            assert_eq!(item(&s, id::SOURCE_TYPE + 3).look, Look::Combo { items: vec!["Default (On)".into(), "On".into(), "Off".into()] });
            assert_eq!(item(&s, id::REPO_ITEMS).look, Look::Combo { items: vec!["Default (skip)".into(), "Include".into(), "Skip".into()] });
            assert_eq!(item(&s, id::FOLDER_RULES).text, "Folder rules…");
            assert_eq!(item(&s, id::FOLDER_RULES_TEXT).text, "Skips the ignored folders set in Settings.");
        }
        assert_eq!(source_choice(id::SOURCE_TYPE + 2, "Off"), Some(SourceEdit::Type(SourceKind::Instructions, Some(false))));
        assert_eq!(source_choice(id::SOURCE_TYPE + 1, "Default (On)"), Some(SourceEdit::Type(SourceKind::Agents, None)));
        assert_eq!(source_choice(id::REPO_ITEMS, "Include"), Some(SourceEdit::RepoItems(Some(true))));
        assert_eq!(source_choice(id::SOURCE_TYPE + 4, "On"), None);
        source_choice(id::SOURCE_TYPE + 2, "Off").unwrap().apply(&mut cfg);
        source_choice(id::SOURCE_TYPE + 1, "Default (Off)").unwrap().apply(&mut cfg);
        source_choice(id::REPO_ITEMS, "Skip").unwrap().apply(&mut cfg);
        let expected = crate::sources::TypeOverrides { instructions: Some(false), ..Default::default() };
        assert_eq!((cfg.types, cfg.include_repo_items), (expected, Some(false)));

        assert_eq!(folder_rules(" a,, b ", "c"), Ok(SourceEdit::FolderRules { add: vec!["a".into(), "b".into()], allow: vec!["c".into()] }));
        assert!(folder_rules("a", "A").is_err(), "a name cannot be skipped and never skipped");
        assert!(folder_rules("a/b", "").is_err());
        assert!(parse_folders("x, x").is_err());
        folder_rules("tests", "examples").unwrap().apply(&mut cfg);
        assert_eq!(folder_rules_text(&cfg), "Also skips: tests. Never skips: examples.");
    }

    #[test]
    fn what_to_sync_has_a_type_section_header_outside_its_card_with_the_type_icons() {
        let mut v = four_types();
        v.notes.push(crate::inventory::SummaryNote { kind: None, path: "AGENTS.md".into(), reason: "it is never synced".into() });
        let mut cfg = source_cfg("o/a");
        cfg.types.set(SourceKind::Agents, false);
        let marks = vec![(glyph::SKILL, Role::Text), (glyph::AGENT, Role::Text3), (glyph::INSTRUCTION, Role::Text), (glyph::PLUGIN, Role::Text)];
        for w in [344, 900] {
            for open in [false, true] {
                let s = page_with(&v, &cfg, &HashSet::new(), &HashMap::from([(SectionKey::WhatToSync, open)]), w);
                fits_in(&s, w);
                let (head, skills) = (item(&s, id::SECTION_WHAT), item(&s, id::SECTION_KIND));
                let [x, y, hw, hh] = head.at;
                assert_eq!([x, hw, hh], [PAD_X, w - 2 * PAD_X, 28], "at {w}: the header is not where a type header is");
                assert_eq!((skills.at[0], skills.at[2], skills.at[3]), (x, hw, hh));
                let state = if open { "expanded" } else { "collapsed" };
                assert_eq!(head.text, format!("What to sync, {state}. Skills on; Agents off; Instructions on; Plugins on"));
                assert_eq!(head.look, Look::Section { title: "What to sync".into(), icon: glyph::SYNC, open, marks: marks.clone(), warn: false });
                for (i, kind) in KINDS.into_iter().enumerate() {
                    let look = &item(&s, id::SECTION_KIND + i as i32).look;
                    assert!(matches!(look, Look::Section { icon, .. } if *icon == kind_glyph(kind)), "{kind:?} has no icon: {look:?}");
                }
                assert!(matches!(item(&s, id::SECTION_NEVER).look, Look::Section { icon: glyph::INFO, warn: false, .. }));
                let cards: Vec<Rect> = s
                    .decos
                    .iter()
                    .filter_map(|d| match d {
                        Deco::Fill { at, fill: Role::Card, .. } => Some(*at),
                        _ => None,
                    })
                    .collect();
                let on_header = |[cx, cy, cw, ch]: Rect| cx < x + hw && x < cx + cw && cy < y + hh && y < cy + ch;
                assert!(!cards.iter().any(|&c| on_header(c)), "at {w}: the header is drawn on a card: {cards:?}");
                if !open {
                    assert_eq!(skills.at[1], y + 32 + GAP, "at {w}: a closed What to sync is spaced like a type section");
                    assert!(!s.items.iter().any(|i| [id::SOURCE_TYPE, id::REPO_ITEMS, id::FOLDER_RULES].contains(&i.id)));
                    continue;
                }
                let card = [x, y + 32, hw, skills.at[1] - GAP - (y + 32)];
                assert!(cards.contains(&card), "at {w}: no card {card:?} under the header in {cards:?}");
                let drawn = s.decos.iter().filter_map(|d| match d {
                    Deco::Glyph { at, .. } | Deco::Text { at, .. } => Some(at[1]),
                    Deco::Fill { .. } | Deco::BandPrint { .. } => None,
                });
                let first = s.items.iter().map(|i| i.at[1]).chain(drawn).filter(|&t| t > y && t < card[1] + card[3]).min();
                assert_eq!(first, Some(y + if w == 900 { 40 } else { 48 }), "at {w}: the card's contents start 8 below its top");
            }
        }
    }

    #[test]
    fn saved_source_starts_with_its_contents_visible_and_what_to_sync_closed() {
        let mut v = four_types();
        v.notes.push(crate::inventory::SummaryNote { kind: None, path: "AGENTS.md".into(), reason: "it is never synced".into() });
        let s = page_with(&v, &source_cfg("o/a"), &HashSet::new(), &HashMap::new(), 720);
        assert!(matches!(item(&s, id::SECTION_WHAT).look, Look::Section { open: false, .. }));
        assert!(!s.items.iter().any(|i| i.id == id::SOURCE_TYPE));
        for n in [id::SECTION_KIND, id::SECTION_KIND + 1, id::SECTION_KIND + 2, id::SECTION_KIND + 3, id::SECTION_NEVER] {
            assert!(matches!(item(&s, n).look, Look::Section { open: true, .. }));
        }
        assert!(s.items.iter().any(|i| i.id == id::SKILL));
    }

    #[test]
    fn a_closed_type_that_needs_attention_warns_after_its_title() {
        let mut v = four_types();
        v.kinds[0].rows.push(edited("y"));
        v.kinds[2].rows.push(edited("z"));
        v.notes.push(crate::inventory::SummaryNote { kind: None, path: "AGENTS.md".into(), reason: "it is never synced".into() });
        let cfg = source_cfg("o/a");
        let heads = |sections: HashMap<SectionKey, bool>| {
            let s = page_with(&v, &cfg, &HashSet::new(), &sections, 720);
            fits_in(&s, 720);
            [id::SECTION_WHAT, id::SECTION_KIND, id::SECTION_KIND + 1, id::SECTION_KIND + 2, id::SECTION_KIND + 3, id::SECTION_NEVER].map(|n| {
                let i = item(&s, n);
                let Look::Section { warn, .. } = i.look else { panic!("{n} is not a section header") };
                (i.text.clone(), warn)
            })
        };
        let plugins_open = heads(HashMap::from([(SectionKey::Kind(SourceKind::Skills), false), (SectionKey::Kind(SourceKind::Instructions), false)]));
        let expected = [
            ("What to sync, collapsed. Skills on; Agents on; Instructions on; Plugins on", false),
            ("Skills (3), collapsed. Needs your attention.", true),
            ("Agents (1), expanded.", false),
            ("Instructions (2 · Sync off), collapsed.", false),
            ("Plugins (1), expanded.", false),
            ("Never synced (1), expanded.", false),
        ];
        assert_eq!(plugins_open, expected.map(|(t, w)| (t.to_string(), w)), "only a closed type that is on and needs you warns");
        let skills_open = heads(HashMap::from([(SectionKey::Kind(SourceKind::Plugins), false)]));
        assert_eq!((skills_open[1].0.as_str(), skills_open[1].1), ("Skills (3), expanded.", false));
        assert_eq!((skills_open[4].0.as_str(), skills_open[4].1), ("Plugins (1), collapsed. Needs your attention.", true));
    }

    /// Where a text between two heights is, whether it wraps, and its font, from the decorations or the controls.
    fn text_at(s: &Surface, text: &str, (top, bottom): (i32, i32)) -> (Rect, u32, Font) {
        let same = |t: &str| t.split_whitespace().collect::<String>() == text.split_whitespace().collect::<String>();
        let decos = s.decos.iter().filter_map(|d| match d {
            Deco::Text { at, text: t, font, flags, .. } if same(t) => Some((*at, *flags, *font)),
            _ => None,
        });
        let items = s.items.iter().filter_map(|i| match &i.look {
            Look::Text { font, flags, .. } if same(&i.text) => Some((i.at, *flags, *font)),
            Look::Section { title, .. } if same(title) => Some((i.at, ONE_LINE, Font::Strong)),
            _ => None,
        });
        decos.chain(items).find(|(at, ..)| top <= at[1] && at[1] < bottom).unwrap_or_else(|| panic!("no text {text:?}"))
    }

    #[test]
    fn issue8_what_to_sync_shows_every_title_and_value_whole_with_the_lists_lined_up() {
        let v = skills_view("o/a", vec![ok("x")]);
        let cfg = source_cfg("o/a");
        let ids = [id::SOURCE_TYPE, id::SOURCE_TYPE + 1, id::SOURCE_TYPE + 2, id::SOURCE_TYPE + 3, id::REPO_ITEMS];
        let titles = ["Skills", "Agents", "Instructions", "Plugins", "Repository-specific skills and agents"];
        for w in [344, 420, 560, 900] {
            let s = page_of(&v, &cfg, &HashSet::new(), w);
            fits_in(&s, w);
            let combos: Vec<Rect> = ids.iter().map(|&n| item(&s, n).at).collect();
            for (&n, at) in ids.iter().zip(&combos) {
                let Look::Combo { items } = &item(&s, n).look else { panic!("{n} is not a list") };
                let longest = items.iter().map(|i| fit(i, Font::Body, 0).0).max().unwrap();
                assert!(at[2] - 44 >= longest, "at {w}: list {n} is {} wide, too narrow for its longest choice ({longest} + 44)", at[2]);
            }
            assert!(combos.iter().all(|c| c[0] == combos[0][0] && c[2] == combos[0][2]), "at {w}: the lists do not line up: {combos:?}");
            let card = (text_at(&s, "What to sync", (0, i32::MAX)).0[1] + 1, text_at(&s, "Folder rules", (0, i32::MAX)).0[1]);
            for (title, combo) in titles.iter().zip(&combos) {
                let (at, flags, font) = text_at(&s, title, card);
                if flags & WRAP != 0 {
                    assert!(at[3] >= fit(title, font, at[2]).1, "at {w}: {title:?} is cut off: {at:?}");
                } else {
                    assert!(at[2] >= fit(title, font, 0).0, "at {w}: {title:?} is cut off: {at:?}");
                }
                let [x, y, tw, th] = at;
                let [cx, cy, cw, ch] = *combo;
                let over = x < cx + cw && cx < x + tw && y < cy + ch && cy < y + th;
                assert!(!over, "at {w}: {title:?} at {at:?} is under its list at {combo:?}");
                if w == 900 {
                    assert!(cy < y + th && y < cy + ch, "at {w}: {title:?} at {at:?} and its list at {combo:?} are not on one line");
                }
            }
        }
    }

    #[test]
    fn the_settings_sources_switches_change_the_global_settings() {
        let mut g = GlobalSettings::default();
        assert_eq!(global_switch(id::GLOBAL_TYPE + 3, false), Some(GlobalEdit::Type(SourceKind::Plugins, false)));
        assert_eq!(global_switch(id::GLOBAL_REPO_ITEMS, true), Some(GlobalEdit::RepoItems(true)));
        assert_eq!(global_switch(id::GLOBAL_TYPE - 1, true), None);
        global_switch(id::GLOBAL_TYPE + 3, false).unwrap().apply(&mut g);
        global_switch(id::GLOBAL_REPO_ITEMS, true).unwrap().apply(&mut g);
        GlobalEdit::Ignore(vec!["node_modules".into()]).apply(&mut g);
        assert!(!g.types.enabled(SourceKind::Plugins) && g.types.enabled(SourceKind::Skills));
        assert!(g.include_repo_items);
        assert_eq!(g.ignore_folders, ["node_modules"]);
        let page = SettingsPage { global: Box::leak(Box::new(g)), ..settings(Ok(&Startup::Off)) };
        for w in [344, 900] {
            let s = settings_page(&page, w, &fit);
            fits_in(&s, w);
            assert_eq!(item(&s, id::GLOBAL_TYPE).text, "Sync skills by default");
            assert_eq!(item(&s, id::GLOBAL_TYPE + 3).look, Look::Toggle { on: false });
            assert_eq!(item(&s, id::GLOBAL_REPO_ITEMS).look, Look::Toggle { on: true });
            assert_eq!(item(&s, id::IGNORED_EDIT).text, "Edit…");
            assert_eq!(item(&s, id::IGNORED_TEXT).text, "Folders with these names are skipped in every source: node_modules.");
        }
    }

    #[test]
    fn the_splitter_clamps_both_views_without_changing_the_preference() {
        for (window, list, page) in [(400, 160, 228), (520, 160, 348), (532, 160, 360), (640, 268, 360), (980, 520, 448), (1400, 520, 868)] {
            let main = split_layout(window, HERO_H, 520, 520);
            let diff = diff_layout(window, 600, 520);
            for layout in [&main, &diff.split] {
                assert_eq!((layout.list[2], layout.page[2]), (list, page), "window of {window} DIP");
                assert_eq!(layout.list[0] + layout.list[2], layout.splitter[0]);
                assert_eq!(layout.splitter[0] + layout.splitter[2], layout.page[0]);
                assert_eq!(layout.page[0] + layout.page[2], window);
            }
            assert_eq!(list_width(window, 1), 160);
            assert_eq!(diff.head[3], diff.split.splitter[1]);
        }
        let surface = splitter(SPLITTER_W, 520);
        assert_eq!(surface.items.len(), 1);
        let handle = item(&surface, id::SPLITTER);
        assert_eq!((handle.text.as_str(), &handle.look, handle.enabled), ("Resize list", &Look::Splitter, true));
        fits_in(&surface, SPLITTER_W);
    }

    #[test]
    fn splitter_keys_step_from_the_displayed_width_and_stop_at_the_bounds() {
        use SplitKey::{End, Home, Left, Right};
        assert_eq!(width_after_key(980, 248, Left, false), 240);
        assert_eq!(width_after_key(980, 248, Right, false), 256);
        assert_eq!(width_after_key(980, 248, Left, true), 216);
        assert_eq!(width_after_key(980, 248, Right, true), 280);
        assert_eq!(width_after_key(980, 248, Home, false), 160);
        assert_eq!(width_after_key(980, 248, End, false), 608);
        assert_eq!(width_after_key(640, 500, Left, false), 260, "start at the displayed width, not the saved width");
        assert_eq!(width_after_key(640, 265, Right, true), 268);
        assert_eq!(width_after_key(640, 161, Left, true), 160);
        assert_eq!(width_after_key(400, 248, End, false), 160);
    }

    #[test]
    fn a_source_says_where_its_items_come_from_and_how_it_is_read() {
        let v = skills_view("o/a", Vec::new());
        assert_eq!(source_sub(&v, NOW), "Public · the skills folder on main · checked 2 minutes ago");
        let private = SourceView { private: true, ..v.clone() };
        assert_eq!(source_sub(&private, NOW), "The skills folder on main · read with your GitHub sign-in · checked 2 minutes ago");
        let top = SourceView { folder: String::new(), branch: String::new(), last_check: None, ..v.clone() };
        assert_eq!(source_sub(&top, NOW), "The whole repository on the default branch · not checked yet");
        let seen = SourceView { seen_branch: Some("trunk".to_string()), ..top.clone() };
        assert_eq!(source_sub(&seen, NOW), "The whole repository on trunk · not checked yet", "the branch the listing read");
        assert_eq!(source_sub(&SourceView { level: Level::Error, ..top }, NOW), "The whole repository on the default branch · not checked successfully yet");
    }

    #[test]
    fn check_every_offers_the_default_the_presets_the_current_value_and_custom() {
        let (items, shown) = every_choices(None, Some(Interval::new(1, Unit::Hour)));
        assert_eq!(shown, "Default (1 hour)");
        assert_eq!(items.first().map(String::as_str), Some("Default (1 hour)"));
        assert_eq!(items.last().map(String::as_str), Some(CUSTOM));
        assert_eq!(items.len(), PRESETS.len() + 2);
        let (items, shown) = every_choices(Some(Interval::new(2, Unit::Hour)), Some(DEFAULT_INTERVAL));
        assert_eq!(shown, "2 hours");
        assert_eq!(items[items.len() - 2], "2 hours", "a value of its own is offered, so it shows as chosen");
        let (items, shown) = every_choices(Some(Interval::new(1, Unit::Day)), None);
        assert_eq!((shown.as_str(), items.len()), ("1 day", PRESETS.len() + 1), "no default to offer, and 1 day is a preset");
        for item in &items[..items.len() - 1] {
            assert!(parse_every(item).is_ok_and(|i| i.is_some()), "{item} must mean an interval");
        }
    }

    #[test]
    fn check_every_is_the_default_unless_it_names_an_interval_that_may_be_polled() {
        assert_eq!(parse_every(""), Ok(None));
        assert_eq!(parse_every("  Default (1 day)"), Ok(None));
        assert_eq!(parse_every("6 hours"), Ok(Some(Interval::new(6, Unit::Hour))));
        assert_eq!(parse_every(" 2h "), Ok(Some(Interval::new(2, Unit::Hour))));
        assert!(parse_every("1 minute").is_err());
        assert!(parse_every("often").is_err());
    }

    #[test]
    fn the_activity_never_shows_a_control_character_from_a_repository() {
        let mut board = Board::new();
        board.say(NOW, Level::Attention, "fx: a\u{1b}[2K\rb\u{202e}exe.pdf\nsecond line".to_string());
        board.say(NOW + 1, Level::Ok, "fx: plain".to_string());
        let notes = activity_of(&board);
        assert_eq!(notes.iter().map(|n| n.at).collect::<Vec<_>>(), [Some(NOW + 1), Some(NOW)], "newest first");
        assert_eq!(notes[0].text, "fx: plain");
        assert_eq!(notes[1].text, "fx: a\\u{1b}[2K\\u{d}b\\u{202e}exe.pdf\nsecond line");
        assert_eq!((notes[1].glyph, notes[1].ink), (glyph::WARN, Role::Warn));
        let page = activity_page(&notes, &|t| format!("t{t}"), 600, &fit);
        assert_eq!(item(&page, id::NOTE + 1).text, notes[1].text);
        fits_in(&page, 600);
    }

    #[test]
    fn skill_rows_show_names_written_out_and_their_ids_lead_to_the_real_rows() {
        let r = source_cfg("o/a");
        let v = skills_view("o/a", vec![edited("rlo-\u{202e}gpj"), ok("alpha")]);
        let none = HashSet::new();
        let page = |toggled: &HashSet<String>| page_of(&v, &r, toggled, 600);

        let s = page(&none);
        // The status is in the window text too, after a line break, so a screen reader reads it with the name.
        assert_eq!(item(&s, id::SKILL).text, "rlo-\\u{202e}gpj\nEdited on this PC\n\nabout rlo-\\u{202e}gpj");
        assert_eq!(hit(id::SKILL), Hit::Skill(0, Part::Row));
        assert_eq!(item(&s, id::SKILL + id::STRIDE).text, "alpha\nUp to date\n\nabout alpha");
        assert_eq!(hit(id::SKILL + id::STRIDE), Hit::Skill(1, Part::Row));
        // The skill that needs the user is open: its choices are there, Show changes first and accented.
        assert_eq!(item(&s, id::SKILL + 2).look, Look::Button { kind: Kind::Accent, glyph: Some(glyph::FILE) });
        assert_eq!(item(&s, id::SKILL + 4).text, "Use the GitHub version");
        assert_eq!(hit(id::SKILL + 5), Hit::Skill(0, Part::Action(1)));
        assert_eq!(hit(id::SKILL + 3), Hit::Skill(0, Part::Open));
        fits_in(&s, 600);

        let toggled: HashSet<String> = [toggle_key("o/a", "rlo-\u{202e}gpj"), toggle_key("o/a", "alpha")].into();
        let s = page(&toggled);
        assert!(!s.items.iter().any(|i| i.id == id::SKILL + 2), "closed by the user");
        assert!(item(&s, id::SKILL).text.ends_with("about rlo-\\u{202e}gpj"));
        assert!(!is_open(&v.kinds[0].rows[1], "o/a", &toggled), "a stale toggle cannot open a healthy skill");
        let other: HashSet<String> = [toggle_key("o/a-agents", "rlo-\u{202e}gpj")].into();
        assert!(page(&other).items.iter().any(|i| i.id == id::SKILL + 2), "closing a row of another type leaves this one open");
    }

    #[test]
    fn plugin_rows_show_facts_and_choices_but_no_file_commands() {
        use RowCmd::{CopyLink, OnGitHub};
        let r = SourceConfig { path: "pack".into(), lanes: crate::sources::Lanes::default().with(SourceKind::Plugins, "o/a".into()), ..source_cfg("o/a") };
        let details = board::PluginDetails {
            version: Some("1.2.3".into()),
            publisher: Some("Review Team".into()),
            path: Some("pack/plugin-folder".into()),
            capabilities: None,
        };
        let explanation =
            "Needs your approval. Plugins can run programs on this PC. Approve installs it now and its new versions from this branch automatically.";
        let plugin =
            Row { text: explanation.into(), plugin: Some(details), ..row("alpha", Level::Attention, "Needs your approval", vec![Action::ApprovePlugin]) };
        let rows = vec![
            plugin,
            Row {
                plugin: Some(board::PluginDetails::default()),
                listed: false,
                ..row("beta", Level::Attention, "Removed from GitHub", vec![Action::UninstallPlugin, Action::ForgetPlugin])
            },
        ];
        let v = source_view("o/a", vec![kind_view(SourceKind::Plugins, "o/a", rows)]);
        for w in [360, 700] {
            let s = page_of(&v, &r, &HashSet::new(), w);
            let a = skill_id(0, 0);
            let b = skill_id(1, 0);
            assert!(matches!(&item(&s, a).look, Look::Row { commands, .. } if commands == &[CopyLink, OnGitHub]));
            assert!(matches!(&item(&s, b).look, Look::Row { commands, .. } if commands.is_empty()));
            assert!(item(&s, a + 8).text.contains("Version: 1.2.3. Publisher: Review Team."));
            assert!(item(&s, a).text.ends_with(explanation));
            assert_eq!(item(&s, a + 8).text.matches("Plugins can run programs").count(), 1);
            assert_eq!(item(&s, a + 4).text, "Approve");
            assert_eq!(item(&s, b + 4).text, "Uninstall");
            assert_eq!(item(&s, b + 5).text, "Forget");
            assert!(s.items.iter().all(|item| item.id != a + 2 && item.id != a + 9));
            fits_in(&s, w);
        }
        assert_eq!(
            crate::github::web_url(&lane_config(&r, SourceKind::Plugins, "o/a"), Some("pack/plugin-folder")).as_deref(),
            Some("https://github.com/o/a/tree/main/pack/plugin-folder")
        );
    }

    #[test]
    fn plugin_choice_failure_is_shown_in_the_repo_infobar() {
        let r = source_cfg("o/a");
        let mut v = unreadable_source("o/a");
        v.kinds[0].kind = SourceKind::Plugins;
        let s = page_of(&v, &r, &HashSet::new(), 600);
        assert_eq!(item(&s, id::INFO_LEAD).text, "Could not check this plugin source.");
        assert_eq!(item(&s, id::INFO_TEXT).text, v.detail.clone().unwrap());
        assert_eq!(item(&s, id::RETRY).text, "Try again");
        v.kinds.insert(0, KindView { level: Level::Error, ..kind_view(SourceKind::Skills, "o/a", Vec::new()) });
        assert_eq!(item(&page_of(&v, &r, &HashSet::new(), 600), id::INFO_LEAD).text, "Can’t read this repository.");
    }

    #[test]
    fn plugin_reinstall_is_visible_on_an_informational_row() {
        let r = source_cfg("o/a");
        let missing = Row { plugin: Some(board::PluginDetails::default()), ..row("alpha", Level::Ok, "Removed in Copilot", vec![Action::ReinstallPlugin]) };
        let v = source_view("o/a", vec![kind_view(SourceKind::Plugins, "o/a-plugins", vec![missing])]);
        let s = page_of(&v, &r, &HashSet::new(), 360);
        assert_eq!(item(&s, skill_id(0, 4)).text, "Reinstall");
    }

    #[test]
    fn each_skill_has_sixteen_ids_and_each_part_is_told_apart() {
        let at = |j: i32, k: i32| id::SKILL + id::STRIDE * j + k;
        for j in [0, 1, 7, 5_000] {
            let skill = |part| Hit::Skill(j as usize, part);
            assert_eq!(hit(at(j, 0)), skill(Part::Row));
            assert_eq!(hit(at(j, 2)), skill(Part::Compare));
            assert_eq!(hit(at(j, 3)), skill(Part::Open));
            for k in 0..4 {
                assert_eq!(hit(at(j, 4 + k)), skill(Part::Action(k as usize)));
            }
            assert_eq!(hit(at(j, 9)), skill(Part::Preview));
            assert_eq!(hit(at(j, 10)), skill(Part::CopyPath));
            assert_eq!(hit(at(j, 11)), skill(Part::CopyLink));
            assert_eq!(hit(at(j, 12)), skill(Part::OnGitHub));
            for k in [1, 8, 13, 14, 15] {
                assert_eq!(hit(at(j, k)), Hit::Other(at(j, k)), "part {k} is text or unused");
            }
            for k in 0..id::STRIDE {
                assert_eq!(skill_id(j as usize, k), at(j, k));
            }
        }
    }

    fn facts(name: &str) -> SkillFacts {
        SkillFacts { name: name.to_string(), commit: "1a2b3c4d5e6f".to_string(), files: 3, bytes: 12 * 1024, updated: Some(NOW - 2 * 86_400) }
    }

    #[test]
    fn everyday_commands_and_disclosure_follow_the_current_row() {
        use RowCmd::*;
        let kept = row("mine", Level::Ok, "Kept as yours", vec![Action::ManageAgain]);
        let failed = row("busy", Level::Error, "Failed", vec![]);
        let deleted = Row { facts: Some(facts("gone")), ..row("gone", Level::Attention, board::DELETED_TAG, vec![Action::Restore, Action::LeaveRemoved]) };
        assert!(!expandable(&kept));
        assert!(!expandable(&failed));
        assert!(expandable(&deleted));
        assert_eq!(row_commands(&kept, SourceKind::Skills, false), [Open, CopyPath, Compare, ManageAgain]);
        assert_eq!(row_commands(&failed, SourceKind::Skills, false), [Open, CopyPath, Compare]);
        assert_eq!(row_commands(&deleted, SourceKind::Skills, true), [CopyLink, OnGitHub], "a record alone cannot be previewed");
        assert!(row_commands(&deleted, SourceKind::Skills, false).is_empty());
        assert_eq!(row_commands(&kept, SourceKind::Agents, true), [Compare, ManageAgain], "file actions must not use a skills folder or a guessed URL");
        let absent = Row { folder_missing: true, facts: Some(facts("busy")), ..failed };
        assert_eq!(
            row_commands(&absent, SourceKind::Skills, true),
            [CopyLink, OnGitHub],
            "neither the failure tag nor an install record proves the folder exists"
        );
    }

    #[test]
    fn skill_rows_are_one_line_with_details_for_the_tooltip_and_only_repairs_expand() {
        use RowCmd::*;
        let known = |row: Row| Row { facts: Some(facts(&row.name)), ..row };
        let explanation = "This skill could not be updated. ".repeat(5);
        let v = skills_view(
            "o/a",
            vec![
                known(ok("alpha")),
                Row { text: explanation.clone(), ..known(edited("beta")) },
                row("gamma", Level::Ok, "New", Vec::new()),
                Row { listed: false, ..known(row("delta", Level::Attention, "Removed on GitHub", vec![Action::KeepMine])) },
                ok("epsilon"),
            ],
        );
        let toggled: HashSet<String> = ["beta", "alpha"].map(|n| toggle_key("o/a", n)).into();
        let at = |j: i32, k: i32| id::SKILL + id::STRIDE * j + k;
        let has = |s: &Surface, n: i32| s.items.iter().any(|i| i.id == n);
        for w in [344, 600, 900] {
            let s = page_of(&v, &source_cfg("o/a"), &toggled, w);
            fits_in(&s, w);
            let look = |j, tone, glyph, commands, disclosure| {
                assert_eq!(item(&s, at(j, 0)).look, Look::Row { tone, glyph, commands, disclosure });
            };
            let facts = "Version 1a2b3c4 · 3 files, 12 KB · Updated 2 days ago";
            assert_eq!(item(&s, at(0, 0)).text, format!("alpha\nUp to date\n{facts}\n"));
            let everyday = vec![Open, Preview, CopyPath, CopyLink, OnGitHub];
            look(0, Tone::Ok, Some(glyph::OK), everyday.clone(), None);
            look(1, Tone::Warn, Some(glyph::EDIT), everyday, Some(false));
            assert!(!has(&s, at(1, 2)) && !has(&s, at(1, 4)));
            assert_eq!(item(&s, at(1, 0)).text, format!("beta\nEdited on this PC\n{facts}\n{explanation}"));
            assert_eq!(item(&s, at(2, 0)).at[1] - item(&s, at(1, 0)).at[1], ROW_H + 4);
            look(2, Tone::Off, None, vec![CopyLink, OnGitHub], None);
            look(3, Tone::Warn, Some(glyph::WARN), vec![Open, Preview, CopyPath], Some(true));
            assert_eq!(item(&s, at(3, 4)).text, "Keep my version");
            assert!(item(&s, at(3, 4)).at[1] >= item(&s, at(3, 0)).at[1] + ROW_H);
            assert!(item(&s, at(4, 0)).text.ends_with("\n\nabout epsilon"));
            for j in 0..5 {
                assert!(!has(&s, at(j, 1)) && !has(&s, at(j, 8)), "no extra lines below the header");
                assert!([3, 9, 10, 11, 12].iter().all(|&k| !has(&s, at(j, k))), "no duplicate everyday buttons");
            }
        }

        // No link is made to a repository whose name can't be put in one as it is.
        let odd = SourceConfig { repo: "o/\u{202e}a".to_string(), ..source_cfg("o/a") };
        let s = page_of(&v, &odd, &toggled, 600);
        assert!(matches!(&item(&s, at(0, 0)).look, Look::Row { commands, .. } if commands == &[Open, Preview, CopyPath]));
    }

    #[test]
    fn every_page_fits_a_narrow_and_a_wide_window() {
        let r = SourceConfig { interval: Some(Interval::new(2, Unit::Hour)), ..source_cfg("o/a") };
        let v = SourceView { private: true, detail: Some("GitHub answered not found.".to_string()), ..four_types() };
        let needy = SourceView { private: true, ..skills_view("o/a", vec![edited("e"), ok("x")]) };
        let none = HashSet::new();
        // The longest note there is, with a path that has no spaces to break at.
        let other = Startup::OtherCopy(format!(r"D:\{}\tiny-skill-sync.exe", "d".repeat(200)));
        let settings = SettingsPage { skills_dir: r"C:\Users\someone\.copilot\skills", token_saved: true, ..settings(Ok(&other)) };
        for w in [344, 900] {
            let s = page_of(&v, &r, &none, w);
            fits_in(&s, w);
            assert_eq!(item(&s, id::EVERY).text, "Check every\n2 hours");
            assert!(item(&s, id::PRIVATE).at[0] > 24);
            let mut tops: Vec<i32> = s.items.iter().map(|i| i.at[1]).collect();
            tops.dedup();
            assert!(s.height > *tops.iter().max().unwrap_or(&0));
            fits_in(&settings_page(&settings, w, &fit), w);
            fits_in(&welcome(w, &fit), w);
            fits_in(&nav(std::slice::from_ref(&v), &Page::Settings, NAV_W, 480, &fit), NAV_W);
            fits_in(&hero(&hero_of(std::slice::from_ref(&needy), None, "", false, NOW), true, w + NAV_W, &fit), w + NAV_W);
        }
    }

    #[test]
    fn the_settings_page_offers_to_replace_a_saved_token_and_to_save_one_otherwise() {
        let p = |token_saved| SettingsPage { token_saved, ..settings(Ok(&Startup::Off)) };
        let saved = settings_page(&p(true), 700, &fit);
        assert_eq!(item(&saved, id::SAVE_TOKEN).text, "Replace token…");
        assert_eq!(item(&saved, id::DELETE_TOKEN).text, "Delete token");
        assert_eq!(item(&saved, id::DEFAULT_EVERY).text, "1 day");
        let none = settings_page(&p(false), 700, &fit);
        assert_eq!(item(&none, id::SAVE_TOKEN).text, "Save a token…");
        assert!(!none.items.iter().any(|i| i.id == id::DELETE_TOKEN));
    }

    #[test]
    fn app_updates_are_separate_from_skill_checks_and_manual_actions_stay_available() {
        let report = "Installed on disk: 0.3.0. Available: 0.4.0. Verified for Quit.";
        let p = SettingsPage {
            app_update: UpdatePrefs { enabled: false, interval: UpdateInterval::Monthly, timing: UpdateTiming::OnQuit },
            app_update_status: report,
            ..settings(Ok(&Startup::Off))
        };
        for w in [344, 900] {
            let s = settings_page(&p, w, &fit);
            fits_in(&s, w);
            assert_eq!(item(&s, id::APP_HEADING).text, "App updates");
            assert_eq!(item(&s, id::APP_STATUS).text, format!("Running tray build: 0.2.0.\n{report}"));
            assert_eq!(item(&s, id::APP_AUTO).look, Look::Toggle { on: false });
            assert!(item(&s, id::APP_AUTO).enabled);
            assert_eq!(item(&s, id::APP_EVERY).text, "App update check interval\nEvery 30 days");
            assert_eq!(item(&s, id::DEFAULT_EVERY).text, "1 day", "skill polling is a separate setting");
            assert_eq!(item(&s, id::APP_WHEN).text, "When to install app updates\nOn Quit");
            assert!(item(&s, id::APP_TIMING_TEXT).text.contains("Quit from the tray menu, not when you close this window. The tray stays closed"));
            for (id, label, action) in [(id::APP_CHECK, "Check only", AppAction::Check), (id::APP_INSTALL, "Install now", AppAction::Install)] {
                assert!(item(&s, id).enabled);
                assert_eq!(item(&s, id).text, label);
                assert_eq!(hit(id), Hit::AppUpdate(action));
            }
            assert_eq!(hit(id::SYNC), Hit::Other(id::SYNC));
        }

        let failed = SettingsPage { app_update_status: "Check failed: bad\u{202e}name", ..settings(Ok(&Startup::Off)) };
        let s = settings_page(&failed, 700, &fit);
        assert!(item(&s, id::APP_STATUS).text.ends_with("Check failed: bad\\u{202e}name"));
        assert_eq!(item(&s, id::APP_AUTO).look, Look::Toggle { on: true });
        assert_eq!(item(&s, id::APP_EVERY).text, "App update check interval\nDaily");
        assert_eq!(item(&s, id::APP_WHEN).text, "When to install app updates\nImmediate", "the default installs at once");
        assert!(item(&s, id::APP_TIMING_TEXT).text.contains("then restart the tray into it"));
        let Look::Combo { items } = &item(&s, id::APP_EVERY).look else { panic!("no app interval menu") };
        assert_eq!(items, &["Daily", "Every 7 days", "Every 30 days"]);
        assert_eq!(
            items.iter().map(|label| selection(APP_INTERVALS, label)).collect::<Vec<_>>(),
            [Some(UpdateInterval::Daily), Some(UpdateInterval::Weekly), Some(UpdateInterval::Monthly)]
        );
        let Look::Combo { items } = &item(&s, id::APP_WHEN).look else { panic!("no install timing menu") };
        assert_eq!(items.iter().map(|label| selection(APP_TIMINGS, label)).collect::<Vec<_>>(), [Some(UpdateTiming::Immediate), Some(UpdateTiming::OnQuit)]);
    }

    #[test]
    fn the_whole_app_report_fits_above_its_actions_even_with_a_long_unbroken_path() {
        // Like DrawText(DT_CALCRECT), a word wider than the offered space can demand a wider rectangle.
        let measure = |text: &str, font, width| {
            if width == 0 {
                return fit(text, font, 0);
            }
            let widest = text.split_whitespace().map(|word| fit(word, font, 0).0).max().unwrap_or(0);
            (widest.max(fit(text, font, width).0), text.split('\n').map(|line| fit(line, font, width).1).sum())
        };
        let report = format!("Installed on disk: 0.3.0\nPending for Quit: none\nLast app result: check failed in C:\\{}", "測abcdefgh".repeat(32));
        let p = SettingsPage { app_update_status: &report, ..settings(Ok(&Startup::Off)) };
        for w in [303, 344, 900] {
            let s = settings_page(&p, w, &measure);
            fits_in(&s, w);
            let status = item(&s, id::APP_STATUS);
            let [_, y, width, height] = status.at;
            let expected = format!("Running tray build: 0.2.0.\n{report}");
            assert_eq!(status.text.replace('\n', ""), expected.replace('\n', ""), "wrapping must not discard part of a status or a file name");
            let (needed_w, needed_h) = measure(&status.text, Font::Caption, width);
            assert!(needed_w <= width && needed_h <= height);
            for id in [id::APP_INSTALL, id::APP_CHECK] {
                assert!(item(&s, id).at[1] >= y + height, "the report gets the full card width; actions go underneath");
            }
        }
    }

    #[test]
    fn start_with_windows_is_a_switch_that_says_how_it_is_and_is_greyed_out_when_it_cant_be_changed() {
        let page = |startup: Result<&Startup, &str>| settings_page(&settings(startup), 700, &fit);
        let switch = |s: &Surface| {
            let i = item(s, id::STARTUP);
            (i.look.clone(), i.enabled)
        };
        let s = page(Ok(&Startup::On));
        assert_eq!(item(&s, id::STARTUP).text, "Start with Windows", "what a screen reader says it is for");
        assert_eq!(switch(&s), (Look::Toggle { on: true }, true));
        assert_eq!(item(&s, id::STARTUP_TEXT).text, Startup::On.note(Via::Window));
        assert_eq!(hit(id::STARTUP), Hit::Other(id::STARTUP));
        for state in [Startup::Off, Startup::OffInWindows, Startup::OtherCopy(r"D:\old\tiny-skill-sync.exe".to_string())] {
            let s = page(Ok(&state));
            assert_eq!(switch(&s), (Look::Toggle { on: false }, true), "{state:?} can be turned on");
            assert_eq!(item(&s, id::STARTUP_TEXT).text, state.note(Via::Window));
        }
        let s = page(Ok(&Startup::TooLong));
        assert_eq!(switch(&s), (Look::Toggle { on: false }, false), "greyed out");
        assert_eq!(item(&s, id::STARTUP_TEXT).text, Startup::TooLong.note(Via::Window));
        let s = page(Err("Access is denied. (os error 5)"));
        assert_eq!(switch(&s), (Look::Toggle { on: false }, false), "not changed blind");
        assert_eq!(item(&s, id::STARTUP_TEXT).text, "Whether Windows starts it can’t be read: Access is denied. (os error 5)");
        fits_in(&s, 700);
    }

    #[test]
    fn each_kind_of_difference_is_summed_up_in_the_list_of_files() {
        let bytes = |s: &str| Content::Bytes(s.as_bytes().to_vec());
        let github = vec![
            ("SKILL.md".to_string(), bytes("a\nb\n")),
            ("gone.md".to_string(), bytes("x\n")),
            ("bom.md".to_string(), bytes("l\n")),
            ("crlf.md".to_string(), bytes("l\n\nm\n")),
            ("logo.png".to_string(), bytes("\0png")),
        ];
        let here = vec![
            ("SKILL.md".to_string(), bytes("a\nc\nd\n")),
            ("new.md".to_string(), bytes("n\n")),
            ("bom.md".to_string(), bytes("\u{feff}l\r\n")),
            ("crlf.md".to_string(), bytes("l\r\nm\r\n\r\n")),
            ("logo.png".to_string(), bytes("\0gif")),
        ];
        let changes = compare(github, here);
        let summary = |p: &str| changes.iter().find(|c| c.path == p).map(file_summary);
        let find = |p: &str| summary(p).map(|(_, _, s)| s);
        assert_eq!(find("SKILL.md").as_deref(), Some("Changed · 1 added, 2 removed"), "GitHub's line comes in, this PC's two go");
        assert_eq!(summary("new.md"), Some((glyph::REMOVE, Role::Err, "Only on this PC".to_string())));
        assert_eq!(summary("gone.md"), Some((glyph::ADD, Role::Ok, "Only on GitHub".to_string())));
        assert_eq!(find("bom.md").as_deref(), Some("Only invisible changes"));
        assert_eq!(find("crlf.md"), None, "line breaks and blank lines don't count");
        assert_eq!(find("logo.png").as_deref(), Some("Different (not text)"));
        let hidden = FileChange { path: "big".to_string(), presence: Presence::Both, body: Body::Hidden("it is too big to show".to_string()) };
        assert_eq!(file_summary(&hidden).2, "Not shown");
    }

    fn diff_of(old: &str, new: &str) -> DiffView {
        let change = FileChange { path: "SKILL.md".to_string(), presence: Presence::Both, body: Body::Text(text_diff(old, new)) };
        DiffView {
            source: RepoConfig { repo: "o/a".into(), id: "o-a".into(), ..Default::default() },
            skill: "beta".to_string(),
            preview: false,
            actions: vec![Action::Overwrite, Action::KeepMine],
            report: Report::Ready(DiffReport { on_github: true, here: true, files: vec![change] }),
            file: 0,
            gaps: Vec::new(),
        }
    }

    #[test]
    fn a_preview_shows_each_file_whole_with_one_column_of_numbers_and_offers_no_choices() {
        let file = |path: &str, content: Content| FileChange { path: path.to_string(), presence: Presence::Both, body: whole(content) };
        let files = vec![
            file("SKILL.md", Content::Bytes(b"# A\n\ntext\n".to_vec())),
            file("one.md", Content::Bytes(b"x".to_vec())),
            file("empty.md", Content::Bytes(Vec::new())),
            file("logo.png", Content::Bytes(b"a\0b".to_vec())),
            file("big.bin", Content::Hidden("it is too big to show".to_string())),
        ];
        let mut v = DiffView { preview: true, report: Report::Ready(DiffReport { on_github: true, here: true, files }), ..diff_of("a\n", "b\n") };
        assert_eq!(diff_sub(&v), "The files GitHub put in this skill, as they are on this PC. Files you added are not shown.");
        let summaries: Vec<String> = v.files().iter().map(|f| v.summary(f).2).collect();
        assert_eq!(summaries, ["3 lines", "1 line", "Empty", "Not text", "Not shown"]);
        assert_eq!(v.summary(&v.files()[0]).0, glyph::FILE);

        for w in [640, 900, 1400] {
            let l = diff_layout(w, 600, NAV_W);
            let head = diff_head(&v, &l, &fit);
            assert!(head.items.iter().all(|i| !matches!(hit(i.id), Hit::DiffAction(_))), "Preview changes nothing, so it offers no choice");
            assert!(head.decos.iter().any(|d| deco_text(d) == "5 files"));
            assert!(!head.decos.iter().any(|d| deco_text(d).starts_with("Only on")), "and has nothing to tell apart");
            fits_in(&head, w);
            fits_in(&diff_files(&v, l.split.list[2] - 17, &fit), l.split.list[2] - 17);
        }

        let s = diff_code(&v, 500, &fit);
        let numbers_beside = |text: &str| -> Vec<String> {
            let y = s.decos.iter().find_map(|d| matches!(d, Deco::Text { text: t, .. } if t == text).then(|| deco_at(d)[1])).unwrap();
            s.decos.iter().filter(|d| matches!(d, Deco::Text { text: t, .. } if t != text && deco_at(d)[1] == y)).map(deco_text).collect()
        };
        assert_eq!(numbers_beside("# A"), ["1"]);
        assert_eq!(numbers_beside("text"), ["3"], "a blank line is a line too");
        assert!(s.decos.iter().any(|d| deco_text(d) == "2"));
        assert!(!s.decos.iter().any(|d| matches!(d, Deco::Fill { .. })), "no line is marked as added or removed");
        fits_in(&s, 500);

        let message = |v: &DiffView| diff_code(v, 500, &fit).items.iter().find(|i| i.id == id::EMPTY).map(|i| i.text.clone());
        v.file = 2;
        assert_eq!(message(&v).as_deref(), Some("This file is empty."));
        v.file = 3;
        assert_eq!(message(&v).as_deref(), Some("This file is not text, so it is not shown."));
        v.file = 4;
        assert_eq!(message(&v).as_deref(), Some("This file is not shown: it is too big to show."));
        let one = DiffView { report: Report::Ready(DiffReport { on_github: true, here: true, files: v.files()[..1].to_vec() }), ..v.clone() };
        let head = diff_head(&one, &diff_layout(900, 600, NAV_W), &fit);
        assert!(head.decos.iter().any(|d| deco_text(d) == "1 file"));
        let loading = DiffView { report: Report::Loading, ..v };
        assert_eq!(diff_sub(&loading), "Reading the files…");
        assert_eq!(message(&loading).as_deref(), Some("Reading the files…"));
    }

    #[test]
    fn hovering_over_long_list_labels_does_not_change_the_width() {
        let long = "team/a-repository-with-a-very-long-name";
        let named = [SourceView { folder: "a/folder/with/a/long/path".to_string(), ..skills_view(long, vec![ok("x")]) }];
        let main = split_layout(980, HERO_H, 600, NAV_W);
        let source = nav(&named, &Page::Activity, main.list[2], 600, &fit);
        let entry = item(&source, id::NAV_REPO);
        assert!(fit(&printable(long), Font::Body, 0).0 > entry.at[2] - 48, "a long name is clipped, not given more space");
        fits_in(&source, NAV_W);

        let mut v = diff_of("old\n", "new\n");
        let Report::Ready(r) = &mut v.report else { unreachable!() };
        let path = "references/library/component-with-a-long-name\u{202e}.md";
        r.files[0].path = path.to_string();
        let diff = diff_layout(980, 600, NAV_W);
        assert_eq!(diff.split.list[2], main.list[2]);
        let files = diff_files(&v, diff.split.list[2], &fit);
        let entry = item(&files, id::FILE);
        assert!(fit(&printable(path), Font::Body, 0).0 > entry.at[2] - 48, "a long path is clipped, not given more space");
        fits_in(&files, NAV_W);
    }

    #[test]
    fn the_diff_cards_headers_and_contents_move_together_without_losing_the_selected_file_or_open_lines() {
        let old: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let mut v = diff_of(&old, &old.replace("line 15\n", "line fifteen\n"));
        let Report::Ready(r) = &mut v.report else { unreachable!() };
        r.files.push(FileChange { path: "references/library/long-component-name.md".to_string(), ..r.files[0].clone() });
        v.file = 1;
        v.gaps.push(0);
        for w in [640, 980, 1400] {
            let narrow = diff_layout(w, 680, NAV_W);
            let wide = diff_layout(w, 680, NAV_W + 20);
            let delta = wide.split.list[2] - narrow.split.list[2];
            assert!(delta > 0);
            assert_eq!(wide.split.page[2], narrow.split.page[2] - delta);
            assert_eq!(wide.split.page[0], narrow.split.page[0] + delta);
            assert_eq!(wide.code_card[0] + wide.code_card[2], w);
            for l in [&narrow, &wide, &narrow] {
                for (card, content) in [(l.files_card, l.split.list), (l.code_card, l.split.page)] {
                    assert_eq!((content[0], content[2]), (card[0], card[2]));
                }
                let head = diff_head(&v, l, &fit);
                let path = head.decos.iter().find(|d| deco_text(d) == v.files()[1].path).unwrap();
                assert_eq!(deco_at(path)[0], l.code_card[0] + 14);
                for d in head.decos.iter().filter(|d| deco_at(d)[1] >= DIFF_TOP) {
                    let [x, _, width, _] = deco_at(d);
                    let card = if deco_text(d).ends_with("files are different") { l.files_card } else { l.code_card };
                    assert!(x >= card[0] && x + width <= card[0] + card[2], "{d:?}");
                }
                let files = diff_files(&v, l.split.list[2] - 17, &fit);
                assert!(matches!(item(&files, id::FILE + 1).look, Look::Nav { selected: true, .. }));
                let code = diff_code(&v, l.split.page[2], &fit);
                assert!(code.decos.iter().any(|d| deco_text(d) == "line 2"), "the opened unchanged lines stay open");
            }
        }
    }

    #[test]
    fn the_same_lines_fold_away_until_opened_and_changed_lines_are_marked() {
        let old: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let new = old.replace("line 15\n", "line fifteen\n");
        let mut v = diff_of(&old, &new);
        let s = diff_code(&v, 500, &fit);
        let gaps: Vec<&Item> = s.items.iter().filter(|i| matches!(hit(i.id), Hit::Gap(_))).collect();
        assert_eq!(gaps.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["11 lines are the same", "12 lines are the same"]);
        let shows = |s: &Surface, text: &str| s.decos.iter().any(|d| matches!(d, Deco::Text { text: t, .. } if t == text));
        assert!(shows(&s, "line fifteen") && shows(&s, "line 15") && shows(&s, "−") && shows(&s, "+"));
        assert!(!shows(&s, "line 2"));
        let Hit::Gap(first) = hit(gaps[0].id) else { unreachable!() };
        v.gaps.push(first);
        let s = diff_code(&v, 500, &fit);
        assert!(shows(&s, "line 2"), "an opened run shows its lines");
        assert_eq!(s.items.iter().filter(|i| matches!(hit(i.id), Hit::Gap(_))).count(), 1);
    }

    #[test]
    fn an_opened_run_shows_each_line_with_its_numbers_in_the_files_and_no_blank_lines() {
        let old: String = (1..=30).map(|n| format!("line {n}\n\n")).collect();
        let new: String = (1..=30).map(|n| if n == 15 { "line fifteen\n".to_string() } else { format!("line {n}\n") }).collect();
        let mut v = diff_of(&old, &new);
        let first = diff_code(&v, 500, &fit).items.iter().find_map(|i| if let Hit::Gap(r) = hit(i.id) { Some(r) } else { None }).unwrap();
        v.gaps.push(first);
        let s = diff_code(&v, 500, &fit);
        let numbers_beside = |text: &str| -> Vec<String> {
            let y = s.decos.iter().find_map(|d| matches!(d, Deco::Text { text: t, .. } if t == text).then(|| deco_at(d)[1])).unwrap();
            s.decos.iter().filter(|d| matches!(d, Deco::Text { text: t, .. } if t != text && deco_at(d)[1] == y)).map(deco_text).collect()
        };
        assert_eq!(numbers_beside("line 2"), ["3", "2"]);
        assert_eq!(numbers_beside("line 11"), ["21", "11"]);
        let texts = s.decos.iter().filter(|d| matches!(d, Deco::Text { font: Font::Mono, .. })).count();
        assert_eq!(texts, 3 * 17 + 3 * 2, "17 unchanged lines with two numbers each, and the two changed lines with a number and a sign");
    }

    fn deco_at(d: &Deco) -> Rect {
        match d {
            Deco::Fill { at, .. } | Deco::BandPrint { at } | Deco::Glyph { at, .. } | Deco::Text { at, .. } => *at,
        }
    }

    fn deco_text(d: &Deco) -> String {
        if let Deco::Text { text, .. } = d { text.clone() } else { String::new() }
    }

    #[test]
    fn the_diff_header_offers_the_skills_choices_with_the_github_version_accented() {
        let v = diff_of("a\n", "b\n");
        let s = diff_head(&v, &diff_layout(900, 600, NAV_W), &fit);
        assert_eq!(item(&s, id::DIFF_ACTION).look, Look::Button { kind: Kind::BandAccent, glyph: None });
        assert!(matches!(item(&s, id::DIFF_ACTION + 1).look, Look::Button { kind: Kind::Standard, .. }));
        assert_eq!(item(&s, id::DIFF_ACTION + 1).text, "Keep my version");
        assert!(item(&s, id::DIFF_ACTION).at[0] > item(&s, id::DIFF_ACTION + 1).at[0], "the accented choice is last");
        assert_eq!(hit(id::DIFF_ACTION + 1), Hit::DiffAction(1));
        assert_eq!(item(&s, id::TITLE).text, "beta");
        fits_in(&s, 900);
        // The legend reads from this PC to GitHub, each sign beside its words.
        let x_of = |text: &str| s.decos.iter().find(|d| deco_text(d) == text).map(|d| deco_at(d)[0]).unwrap();
        assert_eq!(x_of("−") + 14, x_of("Only on this PC"));
        assert_eq!(x_of("+") + 14, x_of("Only on GitHub"));
        assert!(x_of("Only on this PC") < x_of("+"));
        assert!(diff_sub(&v).starts_with("How the copy on this PC would change to match GitHub."), "{}", diff_sub(&v));
        let gone = DiffView { report: Report::Ready(DiffReport { on_github: true, here: true, files: Vec::new() }), ..v };
        assert_eq!(diff_sub(&gone), "The copy on this PC is the same as GitHub.");
    }

    #[test]
    fn a_long_line_is_cut_and_a_tab_is_spaces() {
        assert_eq!(code_line("\tx\u{202e}"), "    x\\u{202e}");
        let long = "y".repeat(MAX_LINE_CHARS + 50);
        assert_eq!(code_line(&long).chars().count(), MAX_LINE_CHARS + 1);
    }

    #[test]
    fn the_page_shown_stays_while_it_is_there_and_otherwise_the_repository_that_needs_the_user_comes_first() {
        let shown = |id: &str| Some(Page::Repo(id.to_string()));
        let mut cfg = Config::default();
        cfg.edit(|s, _| {
            s.extend(["o/a", "o/b", "o/c"].map(source_cfg));
            s[0].lanes = s[0].lanes.clone().with(SourceKind::Agents, "o/a-agents".to_string());
        });
        let views = vec![skills_view("o/a", vec![ok("x")]), skills_view("o/b", vec![edited("e")]), unreadable_source("o/c")];
        assert_eq!(settle(shown("o/a"), &views, &cfg), shown("o/a"));
        assert_eq!(settle(shown("o/a-agents"), &views, &cfg), shown("o/a"), "a lane ID leads to its source");
        assert_eq!(settle(Some(Page::Settings), &views, &cfg), Some(Page::Settings));
        assert_eq!(settle(Some(Page::Activity), &[], &cfg), Some(Page::Activity));
        assert_eq!(settle(shown("o/removed"), &views, &cfg), shown("o/b"));
        assert_eq!(settle(None, &views, &cfg), shown("o/b"));
        let paused = vec![skills_view("o/a", vec![ok("x")]), SourceView { enabled: false, ..skills_view("o/b", vec![edited("e")]) }];
        assert_eq!(settle(None, &paused, &cfg), shown("o/a"), "a paused repository does not ask for the user");
        assert_eq!(settle(shown("o/a"), &[], &cfg), None, "the welcome page, when there is nothing else");
        let a = cfg.find_source("o/a").unwrap();
        assert_eq!(cfg.find("o/a-agents"), Some(&lane_config(a, SourceKind::Agents, "o/a-agents")), "a row's lane equals the one the settings build");
    }

    #[test]
    fn add_says_how_it_reads_the_location() {
        assert_eq!(location_hint(" owner/other "), Ok("owner/other · the whole repository on the default branch".to_string()));
        assert_eq!(location_hint("https://github.com/o/r/tree/dev/x"), Ok("o/r · dev/x (GitHub tells where the branch name ends)".to_string()));
        assert_eq!(location_hint(""), Err("Enter a repository like owner/name or paste its GitHub address.".to_string()));
        assert!(location_hint("https://github.com/o/r/blob/main/a.md").is_err());
    }

    fn location_preview(plugin: bool) -> crate::engine::LocationPreview {
        use crate::inventory::{Inventory, Item, PluginDetails, Status};
        use crate::sources::{Lanes, SourceConfig, Types};
        let lanes = Lanes::default().with(SourceKind::Skills, "tools".into()).with(SourceKind::Plugins, "tools-plugins".into());
        let source = SourceConfig { id: "tools".into(), repo: "team/tools".into(), branch: "main".into(), lanes, ..SourceConfig::default() };
        let mut inventory = Inventory::default();
        inventory.skills.push(Item { path: "skills/alpha".into(), install_name: "alpha".into(), status: Status::Installable, data: vec![] });
        let status = if plugin { Status::Installable } else { Status::Standalone };
        let data = PluginDetails { publisher: Some("Team".into()), version: Some("1.0.0".into()), ..PluginDetails::default() };
        inventory.plugins.push(Item { path: "plugins/x".into(), install_name: "x".into(), status, data });
        crate::engine::LocationPreview { source, branch: "main".into(), follows_default: true, types: Types::default(), inventory, destinations: vec![] }
    }

    fn preview_snapshot(preview: crate::engine::LocationPreview) -> std::sync::Arc<crate::app::PreviewSnapshot> {
        std::sync::Arc::new(crate::app::PreviewSnapshot::new(preview))
    }

    #[test]
    fn add_source_duplicate_refusal_wraps_and_is_replaced_by_the_next_preview() {
        use crate::app::App;
        use crate::config::Config;

        let id = format!("saved-{}", "w".repeat(120));
        let source = RepoConfig { id: id.clone(), path: String::new(), branch: String::new(), ..repo("Team/Tools") };
        let mut app = App::new(Config::from_lanes(vec![source]), "not used".into());
        let queued = app.queued().len();
        let refused = app.preview_location("team/tools", None).expect("the refusal belongs on the Add source page");
        assert_eq!(app.queued().len(), queued, "a duplicate needs no worker job");
        assert!(app.confirm_source(refused, false).is_err());

        // Like DrawText, report a too-wide word even when asked to wrap the paragraph.
        let measure = |text: &str, font, width| {
            let (w, h) = fit(text, font, width);
            let longest = if width > 0 { text.split_whitespace().map(|word| fit(word, font, 0).0).max().unwrap_or(0) } else { w };
            (w.max(longest), h)
        };
        let width = 980 - NAV_W;
        let refusal = preview_page(app.draft().unwrap(), width, &measure);
        assert_eq!(item(&refusal, id::INFO_TEXT).text, format!("This source is already set up. Its ID is '{id}'."));
        assert_eq!(item(&refusal, id::EDIT_SOURCE).text, "Edit location");
        assert_eq!(item(&refusal, id::CANCEL_SOURCE).text, "Cancel");
        assert!(!refusal.items.iter().any(|i| i.id == id::CONFIRM_SOURCE));
        assert!(refusal.height <= 680 - HERO_H);
        assert_eq!(item(&refusal, id::TITLE).text, "team/tools");
        fits_in(&refusal, width);

        app.preview_location("team/other", None).unwrap();
        let next = preview_page(app.draft().unwrap(), width, &measure);
        assert!(next.items.iter().all(|i| !i.text.contains("already set up") && !i.text.contains(&id)));
    }

    #[test]
    fn source_preview_asks_for_plugin_consent_only_when_plugins_would_sync() {
        use crate::app::DraftInput;
        let settings = SourceConfig { enabled: false, ..SourceConfig::default() };
        let input = std::sync::Arc::new(DraftInput { ticket: 1, request: 1, text: "team/tools".into(), settings: settings.clone() });
        let mut nothing = location_preview(false);
        nothing.inventory.skills.clear();
        for (stage, can_add, can_approve, cancel) in [
            (DraftStage::Loading, false, false, true),
            (DraftStage::Failed("Cannot read the listing.".into()), false, false, true),
            (DraftStage::Reviewed(preview_snapshot(nothing)), false, false, true),
            (DraftStage::Reviewed(preview_snapshot(location_preview(false))), true, false, true),
            (DraftStage::Reviewed(preview_snapshot(location_preview(true))), true, true, true),
            (DraftStage::Saving, false, false, false),
        ] {
            for width in [360, 980 - NAV_W] {
                let surface =
                    preview_page(&SourceDraft { input: input.clone(), settings: settings.clone(), previous_preview: None, stage: stage.clone() }, width, &fit);
                let text = |id| surface.items.iter().find(|i| i.id == id).map(|i| i.text.as_str());
                assert_eq!(text(id::CONFIRM_SOURCE) == Some("Add and keep in sync"), can_add, "{stage:?}");
                assert_eq!(text(id::APPROVE_SOURCE) == Some("Add and approve plugins"), can_approve, "{stage:?}");
                assert_eq!(text(id::CANCEL_SOURCE).is_some(), cancel);
                if can_add {
                    assert!(text(id::INFO_TEXT).unwrap().contains("Preview only"), "{stage:?}");
                }
                fits_in(&surface, width);
                no_overlaps(&surface);
            }
        }
    }

    #[test]
    fn source_preview_distinguishes_empty_inventory_from_types_turned_off() {
        use crate::app::DraftInput;
        let global = GlobalSettings::default();
        let mut settings = SourceConfig { enabled: false, ..SourceConfig::default() };
        settings.types.skills = Some(true);
        let input = std::sync::Arc::new(DraftInput { ticket: 1, request: 1, text: "team/tools".into(), settings: settings.clone() });
        let mut empty = location_preview(false);
        empty.inventory.skills.clear();
        empty.inventory.plugins.clear();
        let empty_draft =
            SourceDraft { input: input.clone(), settings: settings.clone(), previous_preview: None, stage: DraftStage::Reviewed(preview_snapshot(empty)) };
        let empty_surface = source_preview(
            &PreviewPage { draft: &empty_draft, global: &global, default: DEFAULT_INTERVAL, now: NOW, toggled: &HashSet::new(), sections: &HashMap::new() },
            720,
            &fit,
        );
        let empty_message = item(&empty_surface, id::INFO_TEXT).text.as_str();
        assert_eq!(empty_message, "No syncable items were found. Change the location or folder rules before adding this source.");

        let mut off = settings.clone();
        off.types.skills = Some(false);
        off.types.agents = Some(false);
        off.types.instructions = Some(false);
        off.types.plugins = Some(false);
        let off_input = std::sync::Arc::new(DraftInput { settings: off.clone(), ..(*input).clone() });
        let off_draft =
            SourceDraft { input: off_input, settings: off, previous_preview: None, stage: DraftStage::Reviewed(preview_snapshot(location_preview(false))) };
        let off_surface = preview_page(&off_draft, 720, &fit);
        assert_eq!(item(&off_surface, id::INFO_TEXT).text, "All item types are turned off. Turn on a type before adding this source.");
        assert!(!off_surface.items.iter().any(|item| item.id == id::CONFIRM_SOURCE));

        let mut plugin_off = settings.clone();
        plugin_off.types.plugins = Some(false);
        let plugin_input = std::sync::Arc::new(DraftInput { settings: plugin_off.clone(), ..(*input).clone() });
        let plugin_draft = SourceDraft {
            input: plugin_input,
            settings: plugin_off,
            previous_preview: None,
            stage: DraftStage::Reviewed(preview_snapshot(location_preview(true))),
        };
        let plugin_surface = preview_page(&plugin_draft, 720, &fit);
        assert!(plugin_surface.items.iter().any(|item| item.id == id::CONFIRM_SOURCE));
        assert!(!plugin_surface.items.iter().any(|item| item.id == id::APPROVE_SOURCE));

        let failed = SourceDraft { input, settings, previous_preview: None, stage: DraftStage::Failed("Cannot read the listing.".into()) };
        let failed_surface = preview_page(&failed, 720, &fit);
        assert!(item(&failed_surface, id::INFO_TEXT).text.contains("The source could not be read"));
        assert!(!item(&failed_surface, id::INFO_TEXT).text.contains("No syncable items"));
    }

    #[test]
    fn source_preview_controls_name_their_purpose_and_current_state() {
        use crate::app::DraftInput;
        let global = GlobalSettings::default();
        let settings = SourceConfig { enabled: false, ..SourceConfig::default() };
        let input = std::sync::Arc::new(DraftInput { ticket: 1, request: 1, text: "team/tools".into(), settings: settings.clone() });
        let draft = SourceDraft {
            input: input.clone(),
            settings: settings.clone(),
            previous_preview: None,
            stage: DraftStage::Reviewed(preview_snapshot(location_preview(false))),
        };
        let render = |draft: &SourceDraft| {
            source_preview(
                &PreviewPage { draft, global: &global, default: DEFAULT_INTERVAL, now: NOW, toggled: &HashSet::new(), sections: &HashMap::new() },
                720,
                &fit,
            )
        };

        let off = render(&draft);
        assert_eq!(item(&off, id::EVERY).text, "Check every\nDefault (1 day)");
        assert_eq!(item(&off, id::ENABLED).text, "Keep in sync: Off");
        assert!(matches!(item(&off, id::ENABLED).look, Look::Toggle { on: false }));

        let mut enabled = draft.clone();
        enabled.settings.enabled = true;
        let on = render(&enabled);
        assert_eq!(item(&on, id::ENABLED).text, "Keep in sync: On");
        assert!(matches!(item(&on, id::ENABLED).look, Look::Toggle { on: true }));
    }

    #[test]
    fn preview_uses_shared_editable_source_sections_with_detected_zero_counts() {
        use crate::inventory::{Item, Status};
        let mut preview = location_preview(false);
        preview.inventory.agents.push(Item {
            path: ".github/agents/review.agent.md".into(),
            install_name: "review.agent.md".into(),
            status: Status::Installable,
            data: None,
        });
        preview.inventory.instructions.push(Item {
            path: ".github/instructions/style.instructions.md".into(),
            install_name: "style.instructions.md".into(),
            status: Status::Installable,
            data: None,
        });
        preview.inventory.plugins.clear();
        let settings = SourceConfig { enabled: false, ..preview.source.clone() };
        let input = std::sync::Arc::new(crate::app::DraftInput { ticket: 1, request: 1, text: "team/tools".into(), settings: settings.clone() });
        let draft = SourceDraft { input, settings, previous_preview: None, stage: DraftStage::Reviewed(preview_snapshot(preview)) };
        let surface = preview_page(&draft, 720, &fit);
        assert!(matches!(item(&surface, id::SECTION_WHAT).look, Look::Section { open: true, .. }));
        assert!(item(&surface, id::SECTION_WHAT).text.contains("Skills on; Agents on; Instructions on; Plugins on"));
        assert_eq!(item(&surface, id::SECTION_KIND).text, "Skills (1), collapsed.");
        assert_eq!(item(&surface, id::SECTION_KIND + 1).text, "Agents (1), collapsed.");
        assert_eq!(item(&surface, id::SECTION_KIND + 2).text, "Instructions (1), collapsed.");
        assert_eq!(item(&surface, id::SECTION_KIND + 3).text, "Plugins (0), collapsed.");
        assert!(matches!(item(&surface, id::SECTION_KIND).look, Look::Section { open: false, .. }));
        assert!(!surface.items.iter().any(|item| item.id == id::SKILL), "item rows start collapsed");
        assert!(matches!(item(&surface, id::ENABLED).look, Look::Toggle { on: false }));
        assert_eq!(item(&surface, id::ENABLED).text, "Keep in sync: Off");
        let mut sections = HashMap::new();
        sections.insert(SectionKey::Kind(SourceKind::Skills), true);
        sections.insert(SectionKey::Kind(SourceKind::Agents), true);
        sections.insert(SectionKey::Kind(SourceKind::Instructions), true);
        let global = GlobalSettings::default();
        let toggled = HashSet::new();
        let open = source_preview(
            &PreviewPage { draft: &draft, global: &global, default: DEFAULT_INTERVAL, now: NOW, toggled: &toggled, sections: &sections },
            720,
            &fit,
        );
        assert!(matches!(item(&open, id::SECTION_KIND).look, Look::Section { open: true, .. }));
        assert!(matches!(&item(&open, id::SKILL).look, Look::Row { commands, .. } if commands == &[RowCmd::CopyLink, RowCmd::OnGitHub]));
        assert!(matches!(&item(&open, skill_id(1, 0)).look, Look::Row { commands, .. } if commands == &[RowCmd::CopyLink, RowCmd::OnGitHub]));
        assert!(matches!(&item(&open, skill_id(2, 0)).look, Look::Row { commands, .. } if commands == &[RowCmd::CopyLink, RowCmd::OnGitHub]));
        fits_in(&surface, 720);
        no_overlaps(&surface);
        fits_in(&open, 720);
        no_overlaps(&open);
    }

    fn no_overlaps(surface: &Surface) {
        for (n, a) in surface.items.iter().enumerate() {
            for b in &surface.items[n + 1..] {
                let apart = a.at[0] + a.at[2] <= b.at[0] || b.at[0] + b.at[2] <= a.at[0] || a.at[1] + a.at[3] <= b.at[1] || b.at[1] + b.at[3] <= a.at[1];
                assert!(apart, "{} and {} overlap: {:?} {:?}", a.id, b.id, a.at, b.at);
            }
        }
    }

    #[test]
    fn continuation_failed_and_empty_previews_offer_both_retry_and_edit() {
        use crate::app::DraftInput;
        let mut nothing = location_preview(false);
        nothing.inventory.skills.clear();
        for stage in [DraftStage::Reviewed(preview_snapshot(nothing)), DraftStage::Failed("No such branch.".into())] {
            let settings = SourceConfig { enabled: false, ..SourceConfig::default() };
            let draft = SourceDraft {
                input: std::sync::Arc::new(DraftInput { ticket: 1, request: 1, text: "team/tools".into(), settings: settings.clone() }),
                settings,
                previous_preview: None,
                stage,
            };
            let surface = preview_page(&draft, 360, &fit);
            assert!(surface.items.iter().any(|i| i.text == "Retry"), "a preview that could not be added needs Retry");
            assert!(surface.items.iter().any(|i| i.text == "Edit location"), "a preview that could not be added needs Edit");
            assert!(!surface.items.iter().any(|i| i.id == id::CONFIRM_SOURCE || i.id == id::APPROVE_SOURCE));
            fits_in(&surface, 360);
        }

        let settings = SourceConfig { enabled: false, ..SourceConfig::default() };
        let input = std::sync::Arc::new(DraftInput { ticket: 1, request: 2, text: "team/tools".into(), settings: settings.clone() });
        let previous_preview = preview_snapshot(location_preview(false));
        for stage in [DraftStage::Loading, DraftStage::Failed("No network.".into())] {
            let draft = SourceDraft { input: input.clone(), settings: settings.clone(), previous_preview: Some(previous_preview.clone()), stage };
            let surface = preview_page(&draft, 360, &fit);
            assert!(surface.items.iter().any(|i| i.id == id::SECTION_KIND && i.text == "Skills (1), collapsed."));
            assert!(!surface.items.iter().any(|i| i.id == id::CONFIRM_SOURCE || i.id == id::APPROVE_SOURCE));
            if matches!(draft.stage, DraftStage::Loading) {
                assert!(surface.decos.iter().any(|deco| matches!(deco, Deco::Text { text, .. } if text == "Refreshing preview")));
                assert!(item(&surface, id::INFO_TEXT).text.contains("previous preview stays visible"));
            } else {
                assert!(item(&surface, id::INFO_TEXT).text.contains("previous preview is shown below"));
            }
        }
    }

    #[test]
    fn issue8_a_preview_that_finds_nothing_to_sync_leads_with_edit() {
        use crate::app::DraftInput;
        let mut nothing = location_preview(false);
        nothing.inventory.skills.clear();
        let settings = SourceConfig { enabled: false, ..SourceConfig::default() };
        let input = std::sync::Arc::new(DraftInput { ticket: 1, request: 1, text: "team/tools".into(), settings: settings.clone() });
        let (accent, standard) = (Look::Button { kind: Kind::Accent, glyph: None }, Look::Button { kind: Kind::Standard, glyph: None });
        let cases = [
            (DraftStage::Reviewed(preview_snapshot(nothing)), id::EDIT_SOURCE, id::RETRY_SOURCE),
            (DraftStage::Failed("No network.".into()), id::RETRY_SOURCE, id::EDIT_SOURCE),
        ];
        for (stage, lead, other) in cases {
            let draft = SourceDraft { input: input.clone(), settings: settings.clone(), previous_preview: None, stage };
            let s = preview_page(&draft, 360, &fit);
            let (first, second) = (item(&s, lead), item(&s, other));
            assert_eq!((&first.look, &second.look), (&accent, &standard), "{:?} leads with {:?}", draft.stage, first.text);
            assert!((first.at[1], first.at[0]) < (second.at[1], second.at[0]), "{:?}: the leading button comes first", draft.stage);
        }
    }
    #[test]
    fn a_question_has_a_text_box_only_with_a_field_and_its_buttons_below_everything() {
        let q = Question {
            heading: "Remove o/a?".to_string(),
            text: "Its skills are no longer kept up to date.".to_string(),
            field: None,
            focused: false,
            combo: None,
            choices: vec!["Leave them".to_string(), "Delete them".to_string()],
            choice: 1,
            note: Some((glyph::LOCK, "A note".to_string())),
            ok: "Remove".to_string(),
            cancel: String::new(),
        };
        let (s, edit) = ask(&q, &fit);
        assert_eq!(edit, None);
        let on: Vec<i32> = s.items.iter().filter(|i| i.look == Look::Radio { on: true }).map(|i| i.id).collect();
        assert_eq!(on, vec![ask_id::CHOICE + 1]);
        let lowest = s.items.iter().filter(|i| ![ask_id::OK, ask_id::CANCEL].contains(&i.id)).map(|i| i.at[1] + i.at[3]).max().unwrap_or(0);
        assert_eq!(item(&s, ask_id::OK).text, "Remove");
        assert_eq!(item(&s, ask_id::CANCEL).text, "Cancel");
        assert!(item(&s, ask_id::OK).at[1] > lowest);
        assert!(s.height >= item(&s, ask_id::OK).at[1] + 32);
        fits_in(&s, ASK_W);

        let typed = Question { field: Some(("Repository".to_string(), Hint::Err("Not a repository".to_string()))), choices: Vec::new(), ..q };
        let (s, edit) = ask(&typed, &fit);
        let [ex, ey, ew, eh] = edit.expect("a text box");
        assert!(ex >= 0 && ex + ew <= ASK_W);
        assert!(item(&s, ask_id::LABEL).at[1] + LINE_H <= ey && ey + eh <= item(&s, ask_id::HINT).at[1]);
        assert_eq!(item(&s, ask_id::HINT).text, "Not a repository");
        assert_eq!(item(&s, ask_id::HINT).look, Look::Text { font: Font::Caption, ink: Role::Err, flags: WRAP });

        let offer = Question { ok: "Turn on".to_string(), cancel: "Not now".to_string(), ..typed };
        assert_eq!(item(&ask(&offer, &fit).0, ask_id::CANCEL).text, "Not now");
    }
}
