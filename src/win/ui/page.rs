//! What the main window shows, built as [`Surface`]s from what the app knows. Nothing here calls Windows: text is
//! measured through a [`Fit`] the window passes in, so every layout and every sentence is tested without a screen.
//!
//! Text that came from a repository (names, paths, messages) is shown through `printable`, so it cannot reorder or
//! hide what is around it. Which repository, skill or file a control is for is said by its id (see [`hit`]), never by
//! its text.

use std::collections::HashSet;

use windows_sys::Win32::Graphics::Gdi::{DT_CENTER, DT_EDITCONTROL, DT_END_ELLIPSIS, DT_PATH_ELLIPSIS, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK};

use super::control::{Deco, Item, Kind, Look, Mark, Surface, two_lines};
use super::paint::{Font, Rect, Tone, glyph};
use super::sentence;
use super::theme::Role;
use crate::board::{self, Action, Board, Level, RepoView, Row, ago, repo_name};
use crate::config::{RepoConfig, RepoSpec};
use crate::diff::{Body, FileChange, Presence, Row as Line, TextDiff};
use crate::engine::DiffReport;
use crate::interval::{Interval, UnixSecs};
use crate::pathsafe::{printable, printable_lines};

/// Measures text in DIPs: (width, height), on one line when the width given is 0, else wrapped at it.
pub type Fit<'a> = &'a dyn Fn(&str, Font, i32) -> (i32, i32);

pub const NAV_W: i32 = 248;
pub const HERO_H: i32 = 66;
pub const SYNC_NOW: &str = "Sync now";
pub const SYNC_TIP: &str = "Gets new and updated skills from every repository now. Skills you edited or deleted are never changed.";
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
    pub const WELCOME_ADD: i32 = 305;
    pub const MORE: i32 = 310;
    pub const EVERY: i32 = 311;
    pub const ENABLED: i32 = 312;
    pub const RETRY: i32 = 313;
    pub const OPEN_SETTINGS: i32 = 314;
    pub const DEFAULT_EVERY: i32 = 320;
    pub const OPEN_SKILLS: i32 = 321;
    pub const OPEN_BACKUPS: i32 = 322;
    pub const SAVE_TOKEN: i32 = 323;
    pub const DELETE_TOKEN: i32 = 324;
    pub const BACK: i32 = 330;
    /// Plus the index of the action in the skill's row.
    pub const DIFF_ACTION: i32 = 333;
    pub const TITLE: i32 = 400;
    pub const PRIVATE: i32 = 401;
    pub const SUB: i32 = 402;
    pub const INFO_LEAD: i32 = 403;
    pub const INFO_TEXT: i32 = 404;
    pub const SKILLS_DIR: i32 = 410;
    pub const TOKEN_TEXT: i32 = 411;
    pub const EMPTY: i32 = 412;
    /// Plus the index of the repository in the settings.
    pub const NAV_REPO: i32 = 1_000;
    pub const NOTE: i32 = 100_000;
    pub const FILE: i32 = 200_000;
    pub const GAP: i32 = 300_000;
    /// Plus 8 per skill, plus the part: 0 the row, 1 its details, 2 Show changes, 3 Open folder, 4 and on its actions.
    pub const SKILL: i32 = 1_000_000;
}

/// A part of a skill's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Row,
    Compare,
    Open,
    Action(usize),
}

/// What a control's id stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Repo(usize),
    Skill(usize, Part),
    File(usize),
    Gap(usize),
    DiffAction(usize),
    Other(i32),
}

pub fn hit(n: i32) -> Hit {
    let index = |base: i32| (n - base) as usize;
    match n {
        id::SKILL.. => {
            let part = match (n - id::SKILL) % 8 {
                0 => Part::Row,
                1 => return Hit::Other(n),
                2 => Part::Compare,
                3 => Part::Open,
                k => Part::Action(k as usize - 4),
            };
            Hit::Skill(index(id::SKILL) / 8, part)
        }
        id::GAP.. => Hit::Gap(index(id::GAP)),
        id::FILE.. => Hit::File(index(id::FILE)),
        id::NOTE.. => Hit::Other(n),
        id::NAV_REPO.. => Hit::Repo(index(id::NAV_REPO)),
        id::DIFF_ACTION..id::TITLE => Hit::DiffAction(index(id::DIFF_ACTION)),
        _ => Hit::Other(n),
    }
}

/// Which page the right side shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    Repo(String),
    Activity,
    Settings,
}

/// The page to show now: the one shown while it is still there, else the first repository that needs the user, else
/// the first repository. `None` is the welcome page, for when there are none.
pub fn settle(shown: Option<Page>, views: &[RepoView]) -> Option<Page> {
    match shown {
        Some(Page::Repo(id)) if views.iter().any(|v| v.id == id) => Some(Page::Repo(id)),
        Some(page @ (Page::Activity | Page::Settings)) => Some(page),
        _ => views.iter().find(|v| v.enabled && v.level >= Level::Attention).or(views.first()).map(|v| Page::Repo(v.id.clone())),
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
/// or the user was just told something), so it replaces it.
pub fn hero_of(views: &[RepoView], problem: Option<&str>, status: &str, news: bool, now: UnixSecs) -> Hero {
    let on: Vec<&RepoView> = views.iter().filter(|v| v.enabled).collect();
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
    let place = |views: &[&RepoView]| match views {
        [one] => format!("In {}", printable(repo_name(&one.label))),
        many => format!("In {} repositories", many.len()),
    };
    if let Some(problem) = problem {
        return hero(Tone::Err, glyph::ERR, "Tiny Skill Sync has a problem".to_string(), calm(&sentence(&printable(problem))));
    }
    if views.is_empty() {
        let text = "Skills from the repositories you add are installed in your skills folder and kept up to date.";
        return hero(Tone::Off, glyph::INFO, "Add a repository to start".to_string(), calm(text));
    }
    if on.is_empty() {
        return hero(Tone::Off, glyph::PAUSE, "Every repository is paused".to_string(), calm("Turn on Keep in sync for one to check it again."));
    }
    let unreadable: Vec<&RepoView> = on.iter().copied().filter(|v| v.level == Level::Error && v.detail.is_some()).collect();
    if !unreadable.is_empty() {
        let title = match unreadable.len() {
            1 => "1 repository can’t be read".to_string(),
            n => format!("{n} repositories can’t be read"),
        };
        return hero(Tone::Err, glyph::ERR, title, sub(place(&unreadable)));
    }
    let needs = |row: &Row| row.level >= Level::Attention;
    let count: usize = on.iter().map(|v| v.skills.iter().filter(|r| needs(r)).count()).sum();
    if count > 0 {
        let with: Vec<&RepoView> = on.iter().copied().filter(|v| v.skills.iter().any(needs)).collect();
        let failed = on.iter().any(|v| v.skills.iter().any(|r| r.level == Level::Error));
        let title = if count == 1 { "1 skill needs your attention".to_string() } else { format!("{count} skills need your attention") };
        let (tone, mark) = if failed { (Tone::Err, glyph::ERR) } else { (Tone::Warn, glyph::WARN) };
        return hero(tone, mark, title, sub(place(&with)));
    }
    let empty: Vec<&RepoView> = on.iter().copied().filter(|v| v.level >= Level::Attention).collect();
    if !empty.is_empty() {
        let title = match empty.len() {
            1 => "1 repository needs your attention".to_string(),
            n => format!("{n} repositories need your attention"),
        };
        return hero(Tone::Warn, glyph::WARN, title, sub(place(&empty)));
    }
    if checked.is_none() {
        return hero(Tone::Off, glyph::SYNC, "Waiting for the first check".to_string(), sub(String::new()));
    }
    let skills: usize = on.iter().map(|v| v.skills.len()).sum();
    let head = format!("{} from {}", plural(skills, "skill", "skills"), plural(on.len(), "repository", "repositories"));
    hero(Tone::Ok, glyph::OK, "All skills are up to date".to_string(), sub(head))
}

pub fn hero(h: &Hero, can_sync: bool, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    o.fill([24, 8, 40, 40], 20, h.tone.bg(), Role::None);
    o.glyph([24, 8, 40, 40], h.glyph, 18, h.tone.ink());
    let bw = o.button_width(SYNC_NOW, Some(glyph::SYNC));
    let bx = w - PAD_X - bw;
    o.button(Btn { enabled: can_sync, ..btn(id::SYNC, SYNC_NOW, Kind::Accent, Some(glyph::SYNC)) }, bx, 12, 32);
    let tw = (bx - 16 - 78).max(0);
    o.label(id::HERO_TITLE, [78, 6, tw, 28], &h.title, Font::Title, Role::Text);
    o.label(id::HERO_SUB, [78, 34, tw, 16], &h.sub, Font::Caption, Role::Text2);
    o.done(HERO_H)
}

/// The second line of a repository in the list.
pub fn nav_sub(v: &RepoView) -> String {
    if !v.enabled {
        return "Paused".to_string();
    }
    let n = v.skills.len();
    if v.detail.is_some() || n == 0 {
        return v.status.clone();
    }
    let skills = plural(n, "skill", "skills");
    match v.skills.iter().filter(|r| r.level >= Level::Attention).count() {
        0 => format!("{skills} · {}", lower_first(&v.status)),
        1 => format!("{skills} · 1 needs you"),
        k => format!("{skills} · {k} need you"),
    }
}

fn nav_mark(v: &RepoView) -> Mark {
    match v.level {
        _ if !v.enabled => Mark::Dot(Tone::Off),
        Level::Error => Mark::Dot(Tone::Err),
        Level::Attention => Mark::Dot(Tone::Warn),
        Level::Ok if v.last_check.is_none() => Mark::Glyph(glyph::SYNC, Role::Text2),
        Level::Ok => Mark::Dot(Tone::Ok),
    }
}

/// The list on the left: the repositories, Add repository, and at the bottom Activity and Settings.
pub fn nav(views: &[RepoView], page: &Page, w: i32, h: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let iw = w - 16;
    o.label(id::NAV_HEAD, [16, 4, w - 32, 20], "Repositories", Font::Strong, Role::Text);
    let mut y = 30;
    for (i, v) in views.iter().enumerate() {
        let selected = *page == Page::Repo(v.id.clone());
        let look = Look::Nav { mark: nav_mark(v), ink: Role::Text, selected };
        o.item(id::NAV_REPO + i as i32, [8, y + 1, iw, 48], two_lines(&printable(&v.label), &nav_sub(v)), look, true);
        y += 50;
    }
    let plain = |ch, ink, selected| Look::Nav { mark: Mark::Glyph(ch, ink), ink, selected };
    o.item(id::NAV_ADD, [8, y + 1, iw, 36], "Add repository", plain(glyph::ADD, Role::AccentText, false), true);
    let bottom = (y + 38 + 12).max(h - 82);
    o.item(id::NAV_ACTIVITY, [8, bottom + 1, iw, 36], "Activity", plain(glyph::HISTORY, Role::Text, *page == Page::Activity), true);
    o.item(id::NAV_SETTINGS, [8, bottom + 39, iw, 36], "Settings", plain(glyph::SETTINGS, Role::Text, *page == Page::Settings), true);
    o.done(bottom + 82)
}

/// The page when no repository is set up yet.
pub fn welcome(w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let text = "Tiny Skill Sync installs Copilot skills from GitHub repositories into your skills folder and keeps them up to date. \
                Skills you edit or delete on this PC are never changed without asking.";
    let y = o.head(w, "Add your first repository", text);
    let bottom = o.flow(PAD_X, y, w - PAD_X, 32, vec![btn(id::WELCOME_ADD, "Add repository", Kind::Accent, Some(glyph::ADD))]);
    o.done(bottom + 16)
}

/// "Public · the skills folder on main · checked 2 minutes ago".
pub fn repo_sub(repo: &RepoConfig, view: &RepoView, now: UnixSecs) -> String {
    let folder = if repo.path.is_empty() { "the top of the repository".to_string() } else { format!("the {} folder", printable(&repo.path)) };
    let branch = if repo.branch.is_empty() { "the default branch".to_string() } else { printable(&repo.branch).into_owned() };
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

/// What a repository typed into Add reads as, or what is wrong with it, such as being set up already.
pub fn spec_hint(text: &str, repos: &[RepoConfig]) -> Result<String, String> {
    let spec = RepoSpec::parse(text).map_err(|e| sentence(&e))?;
    let full = spec.full_name();
    if let Some(r) = repos.iter().find(|r| r.repo.eq_ignore_ascii_case(&full) && r.branch == spec.branch && r.path == spec.path) {
        return Err(format!("{} is already set up", printable(&r.label())));
    }
    let folder = if spec.path.is_empty() { "the top of the repository".to_string() } else { format!("the {} folder", printable(&spec.path)) };
    let branch = if spec.branch.is_empty() { "the default branch".to_string() } else { printable(&spec.branch).into_owned() };
    Ok(format!("{} · {folder} on {branch}", printable(&full)))
}

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

/// A skill that needs the user shows its choices until they close it; another shows them once opened.
pub fn is_open(row: &Row, toggled: &HashSet<String>) -> bool {
    (row.level >= Level::Attention) != toggled.contains(&row.name)
}

fn action_glyph(action: Action) -> Option<char> {
    match action {
        Action::Restore => Some(glyph::UNDO),
        Action::RemoveThem => Some(glyph::DELETE),
        _ => None,
    }
}

pub struct RepoPage<'a> {
    pub view: &'a RepoView,
    pub repo: &'a RepoConfig,
    pub default: Interval,
    pub now: UnixSecs,
    /// The skills, by name, the user opened or closed.
    pub toggled: &'a HashSet<String>,
}

pub fn repo_page(p: &RepoPage, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let (x, cw) = (PAD_X, w - 2 * PAD_X);
    let right = x + cw;
    let mut y = PAD_TOP;
    o.item(id::MORE, [right - 32, y, 32, 32], "More", Look::Button { kind: Kind::Icon, glyph: Some(glyph::MORE) }, true);
    let room = right - 32 - 8 - x;
    let title = printable(&p.repo.repo).into_owned();
    let pill_w = o.width("Private", Font::CaptionStrong) + 18 + 16;
    let title_w = (o.width(&title, Font::Title) + 2).min(room - if p.view.private { pill_w + 10 } else { 0 }).max(0);
    o.label(id::TITLE, [x, y, title_w, 28], title, Font::Title, Role::Text);
    if p.view.private {
        o.item(id::PRIVATE, [x + title_w + 10, y, pill_w, 28], "Private", Look::Pill { tone: Tone::Off, glyph: Some(glyph::LOCK) }, true);
    }
    let h = o.para(id::SUB, [x, y + 28, room], repo_sub(p.repo, p.view, p.now), Font::Caption, Role::Text2);
    y = (y + 28 + h).max(y + 32) + GAP + 4;
    if let Some(detail) = &p.view.detail {
        y = infobar(&mut o, p.view, detail, x, y, cw) + GAP;
    }
    y = interval_card(&mut o, p, x, y, cw) + GAP;
    y = skills(&mut o, p, x, y, cw);
    o.done(y + 16)
}

/// Why a repository can't be read, or why it lists nothing, with what can be done about it.
fn infobar(o: &mut Out, view: &RepoView, detail: &str, x: i32, y: i32, cw: i32) -> i32 {
    let error = view.level == Level::Error;
    let (tone, mark, lead) = if error {
        (Tone::Err, glyph::ERROR_BADGE, "Can’t read this repository.".to_string())
    } else {
        (Tone::Warn, glyph::WARN, format!("{}.", view.status))
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

/// Check every and Keep in sync, on one row when they fit, else on two.
fn interval_card(o: &mut Out, p: &RepoPage, x: i32, y: i32, cw: i32) -> i32 {
    let (items, shown) = every_choices(p.repo.interval, Some(p.default));
    let (every, keep) = ("Check every", "Keep in sync");
    let labels = o.width(every, Font::Body).max(o.width(keep, Font::Body));
    let fixed = 16 + 20 + 16 + 16 + COMBO_W + 16 + 1 + 16 + 16 + TOGGLE_W + 16;
    let mark = o.s.decos.len();
    let toggle = Look::Toggle { on: p.repo.enabled };
    let h = if (cw - fixed) / 2 >= labels {
        let flex = (cw - fixed) / 2;
        o.glyph([x + 16, y, 20, 56], glyph::TIMER, 16, Role::Text);
        o.text([x + 52, y, flex, 56], every, Font::Body, Role::Text, ONE_LINE);
        let cx = x + 52 + flex + 16;
        o.item(id::EVERY, [cx, y + 12, COMBO_W, 32], shown, Look::Combo { items }, true);
        let sx = cx + COMBO_W + 16;
        o.fill([sx, y + 12, 1, 32], 0, Role::Divider, Role::None);
        o.text([sx + 17, y, flex, 56], keep, Font::Body, Role::Text, ONE_LINE);
        o.item(id::ENABLED, [x + cw - 16 - TOGGLE_W, y + 12, TOGGLE_W, 32], keep, toggle, true);
        56
    } else {
        let combo = COMBO_W.min(cw - 52 - 16 - 8 - labels).max(120);
        let lw = cw - 52 - 16 - combo - 8;
        o.glyph([x + 16, y, 20, 52], glyph::TIMER, 16, Role::Text);
        o.text([x + 52, y, lw, 52], every, Font::Body, Role::Text, ONE_LINE);
        o.item(id::EVERY, [x + cw - 16 - combo, y + 10, combo, 32], shown, Look::Combo { items }, true);
        o.fill([x + 1, y + 52, cw - 2, 1], 0, Role::Divider, Role::None);
        o.text([x + 52, y + 53, cw - 52 - 16 - TOGGLE_W - 8, 52], keep, Font::Body, Role::Text, ONE_LINE);
        o.item(id::ENABLED, [x + cw - 16 - TOGGLE_W, y + 63, TOGGLE_W, 32], keep, toggle, true);
        105
    };
    o.card(mark, [x, y, cw, h]);
    y + h
}

/// The skills of a repository, each with its status and, once opened, what can be done about it.
fn skills(o: &mut Out, p: &RepoPage, x: i32, mut y: i32, cw: i32) -> i32 {
    let rows = &p.view.skills;
    if rows.is_empty() {
        let text = if !p.repo.enabled {
            "Its skills are not kept up to date while Keep in sync is off."
        } else if p.view.detail.is_some() {
            return y - GAP;
        } else if p.view.last_check.is_none() {
            "Its skills are listed here after the first check."
        } else {
            "There are no skills to show."
        };
        return y + o.para(id::EMPTY, [x, y, cw], text, Font::Body, Role::Text2);
    }
    let head_w = o.width("Skills", Font::Strong) + 2;
    o.text([x, y, head_w, 20], "Skills", Font::Strong, Role::Text, ONE_LINE);
    o.text([x + head_w + 8, y, 60, 20], rows.len().to_string(), Font::Caption, Role::Text2, ONE_LINE);
    y += 28;
    let (mark, top) = (o.s.decos.len(), y);
    for (j, row) in rows.iter().enumerate() {
        if j > 0 {
            o.fill([x + 1, y, cw - 2, 1], 0, Role::Divider, Role::None);
        }
        let base = id::SKILL + 8 * j as i32;
        let (tone, pill) = pill_of(row);
        let look = Look::Row { tone, glyph: pill };
        o.item(base, [x + 1, y, cw - 2, ROW_H], two_lines(&printable(&row.name), row.tag), look, true);
        y += ROW_H;
        if !is_open(row, p.toggled) {
            continue;
        }
        let (dx, dw) = (x + 44, cw - 44 - 16);
        y += o.para(base + 1, [dx, y - 6, dw], printable_lines(&row.text), Font::Caption, Role::Text2) - 6 + 10;
        let mut buttons = Vec::new();
        if row.can_compare() {
            let kind = if row.actions.contains(&Action::Overwrite) { Kind::Accent } else { Kind::Standard };
            buttons.push(btn(base + 2, "Show changes", kind, Some(glyph::FILE)));
        }
        for (k, action) in row.actions.iter().enumerate() {
            buttons.push(btn(base + 4 + k as i32, action.label(), Kind::Standard, action_glyph(*action)));
        }
        if row.has_folder() {
            buttons.push(btn(base + 3, "Open folder", Kind::Standard, Some(glyph::OPEN)));
        }
        y = o.flow(dx, y, dx + dw, 28, buttons) + 12;
    }
    o.card(mark, [x, top, cw, y - top]);
    y
}

pub struct SettingsPage<'a> {
    pub default: Interval,
    pub skills_dir: &'a str,
    pub token_saved: bool,
    pub version: &'a str,
}

enum Ctl {
    Button(Btn),
    Combo { id: i32, items: Vec<String>, shown: String },
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
        }
        cx += w + 8;
    }
    o.card(deco_at, [x, y, cw, h]);
    y + h
}

pub fn token_text(saved: bool) -> &'static str {
    if saved {
        "Used automatically when a repository can’t be read without it. Your token is saved in Windows Credential Manager on this PC."
    } else {
        "Used automatically when a repository can’t be read without it. No token is saved, so your GitHub CLI sign-in is used if you have one."
    }
}

pub fn settings_page(p: &SettingsPage, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let (x, cw) = (PAD_X, w - 2 * PAD_X);
    let mut y = o.head(w, "Settings", "Changes are saved as soon as you make them.");
    let (items, shown) = every_choices(Some(p.default), None);
    let every = Ctl::Combo { id: id::DEFAULT_EVERY, items, shown };
    let desc = Desc::Text(0, "Used by every repository set to Default".to_string());
    y = setting(&mut o, x, y, cw, glyph::TIMER, "Check for updates every", desc, vec![every]) + GAP;
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
    let about = format!("Tiny Skill Sync {} · runs in the notification area · tss.exe is the command line", p.version);
    y = setting(&mut o, x, y, cw, glyph::INFO, "About", Desc::Text(0, about), Vec::new());
    o.done(y + 16)
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
            "Skills recorded for repositories that are no longer in the settings are left alone: {}. To stop tracking them, run: tss remove ID",
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

/// What the window paints behind the pages: the raised layer under the hero, right of the list. Only its top-left
/// corner is rounded; the others are past the edges.
pub fn backdrop(w: i32, h: i32) -> Vec<Deco> {
    vec![Deco::Fill { at: [NAV_W, HERO_H, w - NAV_W + 20, h - HERO_H + 20], radius: 8, fill: Role::Layer, border: Role::LayerBorder }]
}

/// Where the parts of the diff view go: the header, then the list of files and the lines of one file, each in a card
/// whose own header is drawn by the header panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffLayout {
    pub head: Rect,
    pub files_card: Rect,
    pub code_card: Rect,
    pub files: Rect,
    pub code: Rect,
}

const DIFF_TOP: i32 = 70;
const CARD_HEAD: i32 = 41;

pub fn diff_layout(w: i32, h: i32) -> DiffLayout {
    let ch = (h - DIFF_TOP - 16).max(CARD_HEAD + 2);
    let files_card = [20, DIFF_TOP, 236, ch];
    let code_card = [268, DIFF_TOP, (w - 288).max(2), ch];
    let inner = (ch - CARD_HEAD - 1).max(0);
    DiffLayout {
        head: [0, 0, w, DIFF_TOP + CARD_HEAD],
        files_card,
        code_card,
        files: [21, DIFF_TOP + CARD_HEAD, 234, inner],
        code: [code_card[0] + 1, DIFF_TOP + CARD_HEAD, code_card[2] - 2, inner],
    }
}

pub fn diff_backdrop(l: &DiffLayout, w: i32, h: i32) -> Vec<Deco> {
    let mut out = vec![Deco::Fill { at: [-20, 0, w + 40, h + 20], radius: 0, fill: Role::Layer, border: Role::None }];
    for card in [l.files_card, l.code_card] {
        out.push(Deco::Fill { at: card, radius: 6, fill: Role::Card, border: Role::CardBorder });
        out.push(Deco::Fill { at: [card[0] + 1, DIFF_TOP + CARD_HEAD - 1, card[2] - 2, 1], radius: 0, fill: Role::Divider, border: Role::None });
    }
    out
}

/// What "Show changes" found, or that it is still looking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    Loading,
    Failed(String),
    Ready(DiffReport),
}

/// The diff view of one skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffView {
    pub repo_id: String,
    pub repo_label: String,
    pub skill: String,
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
}

/// The sentence under the skill's name.
pub fn diff_sub(v: &DiffView) -> String {
    match &v.report {
        Report::Loading => "Reading both copies…".to_string(),
        Report::Failed(why) => sentence(&printable(why)),
        Report::Ready(r) if r.files.is_empty() => "The copy on this PC is the same as GitHub.".to_string(),
        Report::Ready(r) if !r.here => "Its folder is not on this PC, so every file shows as only on GitHub.".to_string(),
        Report::Ready(r) if !r.on_github => "GitHub no longer lists it, so every file shows as only on this PC.".to_string(),
        Report::Ready(_) => "What is different between GitHub and the copy on this PC. Nothing changes until you choose.".to_string(),
    }
}

/// The header: Back, which skill, its choices, and the headers of the two cards.
pub fn diff_head(v: &DiffView, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    o.item(id::BACK, [12, 10, 32, 32], "Back", Look::Button { kind: Kind::Icon, glyph: Some(glyph::BACK) }, true);
    // From the right: the first choice, which takes GitHub's version, ends the row.
    let mut right = w - 20;
    for (k, action) in v.actions.iter().enumerate() {
        if *action == Action::RemoveThem {
            continue;
        }
        let kind = if matches!(action, Action::Overwrite | Action::Restore) { Kind::Accent } else { Kind::Standard };
        let b = btn(id::DIFF_ACTION + k as i32, action.label(), kind, action_glyph(*action));
        let bw = o.button_width(&b.text, b.glyph);
        if right - bw < 56 + 120 {
            break;
        }
        right -= bw;
        o.button(b, right, 10, 32);
        right -= 8;
    }
    let room = (right - 8 - 56).max(0);
    let repo = printable(&v.repo_label).into_owned();
    let skill = printable(&v.skill).into_owned();
    let repo_w = (o.width(&repo, Font::Title) + 2).min(room / 2);
    o.text([56, 12, repo_w, 28], repo, Font::Title, Role::Text2, ONE_LINE);
    o.glyph([56 + repo_w + 8, 12, 12, 28], glyph::CHEVRON_RIGHT, 12, Role::Text2);
    let sx = 56 + repo_w + 28;
    o.label(id::TITLE, [sx, 12, (right - 8 - sx).max(0), 28], skill, Font::Title, Role::Text);
    o.label(id::SUB, [56, 42, (w - 56 - 24).max(0), 20], diff_sub(v), Font::Caption, Role::Text2);
    let l = diff_layout(w, 0);
    let files = v.files();
    if !files.is_empty() {
        let count = if files.len() == 1 { "1 file is different".to_string() } else { format!("{} files are different", files.len()) };
        o.text([l.files_card[0] + 12, DIFF_TOP, l.files_card[2] - 24, CARD_HEAD - 1], count, Font::Caption, Role::Text2, ONE_LINE);
    }
    if let Some(change) = v.shown() {
        let [cx, _, cw, _] = l.code_card;
        let mut lx = cx + cw - 14;
        if matches!(change.body, Body::Text(_)) {
            for (sign, ink, text, bg) in [("+", Role::Ok, "Only on this PC", Role::AddBg), ("−", Role::Err, "Only on GitHub", Role::DelBg)] {
                let lw = 8 + 8 + 6 + o.width(text, Font::Caption) + 2 + 8;
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

/// The mark, its colour and the second line of a file in the list.
pub fn file_summary(change: &FileChange) -> (char, Role, String) {
    match (change.presence, &change.body) {
        (Presence::OnlyHere, _) => (glyph::ADD, Role::Ok, "Only on this PC".to_string()),
        (Presence::OnlyOnGitHub, _) => (glyph::REMOVE, Role::Err, "Only on GitHub".to_string()),
        (Presence::Both, Body::Text(t)) if t.only_invisible() => (glyph::EDIT, Role::Warn, "Only invisible changes".to_string()),
        (Presence::Both, Body::Text(t)) => (glyph::EDIT, Role::Warn, format!("Changed · {} added, {} removed", t.added, t.removed)),
        (Presence::Both, Body::NotText) => (glyph::BINARY, Role::Text2, "Different (not text)".to_string()),
        (Presence::Both, Body::Hidden(_)) => (glyph::INFO, Role::Text2, "Not shown".to_string()),
    }
}

/// The files that are different, in the left card.
pub fn diff_files(v: &DiffView, w: i32, fit: Fit) -> Surface {
    let mut o = Out::new(fit);
    let mut y = 4;
    for (i, change) in v.files().iter().enumerate() {
        let (mark, ink, sub) = file_summary(change);
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
        (Report::Loading, _) => message(&mut o, "Reading both copies…"),
        (Report::Ready(_), None) | (Report::Failed(_), _) => 0,
        (Report::Ready(_), Some(change)) => match &change.body {
            Body::NotText => message(&mut o, "This file is not text, so only the fact that it is different is shown."),
            Body::Hidden(why) => message(&mut o, &format!("This file is not shown: {}.", printable(why))),
            Body::Text(t) if t.only_invisible() => message(
                &mut o,
                "Every line is the same. Only what can't be seen is different: the line breaks, a mark at the start of the file, \
                 or the line break at its end.",
            ),
            Body::Text(t) => lines(&mut o, t, &v.gaps, w),
        },
    };
    o.done(bottom)
}

fn lines(o: &mut Out, t: &TextDiff, open: &[usize], w: i32) -> i32 {
    let most = t.old.len().max(t.new.len()).max(1);
    let gw = 38.max(o.width(&"0".repeat(most.to_string().len()), Font::Mono) + 16);
    let sx = 2 * gw;
    let tx = sx + 18;
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
        for (k, number) in [old, new].into_iter().enumerate() {
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
                for i in 0..count.min(MAX_LINES - shown) {
                    line(o, y, Some(old + i), Some(new + i));
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
    o.item(ask_id::CANCEL, [x + bw + 8, y + 20, cw - bw - 8, 32], "Cancel", Look::Button { kind: Kind::Standard, glyph: None }, true);
    (o.done(y + FOOT_H), edit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_INTERVAL;
    use crate::diff::{Content, compare, text_diff};
    use crate::interval::Unit;

    const NOW: UnixSecs = 1_700_000_000;

    /// 7 DIPs a character, wrapped by characters.
    fn fit(text: &str, font: Font, width: i32) -> (i32, i32) {
        let w = text.chars().count() as i32 * 7;
        let lh = line_height(font);
        if width <= 0 { (w, lh) } else { (w.min(width), ((w + width - 1) / width).max(1) * lh) }
    }

    fn row(name: &str, level: Level, tag: &'static str, actions: Vec<Action>) -> Row {
        Row { name: name.to_string(), text: format!("about {name}"), tag, level, actions }
    }

    fn ok(name: &str) -> Row {
        row(name, Level::Ok, board::UP_TO_DATE_TAG, Vec::new())
    }

    fn edited(name: &str) -> Row {
        row(name, Level::Attention, board::EDITED_TAG, vec![Action::Overwrite, Action::KeepMine])
    }

    fn view(label: &str, skills: Vec<Row>) -> RepoView {
        let level = skills.iter().map(|r| r.level).max().unwrap_or(Level::Ok);
        RepoView {
            id: label.to_string(),
            label: label.to_string(),
            enabled: true,
            status: "Up to date".to_string(),
            detail: None,
            level,
            last_check: Some(NOW - 120),
            private: false,
            skills,
        }
    }

    fn unreadable(label: &str) -> RepoView {
        RepoView { level: Level::Error, status: "Not found".to_string(), detail: Some("GitHub answered not found.".to_string()), ..view(label, Vec::new()) }
    }

    fn repo(name: &str) -> RepoConfig {
        RepoConfig { id: name.to_string(), repo: name.to_string(), path: "skills".to_string(), branch: "main".to_string(), ..RepoConfig::default() }
    }

    fn texts(s: &Surface) -> Vec<(i32, String)> {
        s.items.iter().map(|i| (i.id, i.text.clone())).collect()
    }

    fn item(s: &Surface, n: i32) -> &Item {
        s.items.iter().find(|i| i.id == n).unwrap_or_else(|| panic!("no control {n} in {:?}", texts(s)))
    }

    fn fits_in(s: &Surface, w: i32) {
        for i in &s.items {
            let [x, _, iw, _] = i.at;
            assert!(x >= 0 && iw >= 0 && x + iw <= w, "{} ({:?}) is outside 0..{w}: {:?}", i.id, i.text, i.at);
        }
    }

    #[test]
    fn the_hero_says_the_most_important_thing_first() {
        let status = "Next check in 1 hour";
        let hero = |views: &[RepoView], problem: Option<&str>| hero_of(views, problem, status, false, NOW);
        let fine = view("o/a", vec![ok("x"), ok("y")]);
        let other = view("o/b", vec![ok("z")]);

        let got = hero(&[fine.clone(), other.clone()], None);
        assert_eq!((got.tone, got.title.as_str()), (Tone::Ok, "All skills are up to date"));
        assert_eq!(got.sub, "3 skills from 2 repositories · checked 2 minutes ago · next check in 1 hour");

        let needy = view("o/\u{202e}b@main : skills", vec![edited("e"), ok("z")]);
        let got = hero(&[fine.clone(), needy.clone()], None);
        assert_eq!((got.tone, got.title.as_str()), (Tone::Warn, "1 skill needs your attention"));
        assert_eq!(got.sub, "In o/\\u{202e}b · checked 2 minutes ago · next check in 1 hour", "a name from the settings is escaped");

        let got = hero(&[needy.clone(), unreadable("o/c"), unreadable("o/d")], None);
        assert_eq!(
            (got.tone, got.title.as_str(), got.sub.as_str()),
            (Tone::Err, "2 repositories can’t be read", "In 2 repositories · checked 2 minutes ago · next check in 1 hour")
        );

        let got = hero(std::slice::from_ref(&fine), Some("the skills folder is a file"));
        assert_eq!((got.title.as_str(), got.sub.as_str()), ("Tiny Skill Sync has a problem", "The skills folder is a file"));

        assert_eq!(hero(&[], None).title, "Add a repository to start");
        assert_eq!(hero(&[RepoView { enabled: false, ..fine.clone() }], None).title, "Every repository is paused");
        let empty = RepoView { level: Level::Attention, status: "No skills found".to_string(), detail: Some("…".to_string()), ..view("o/e", Vec::new()) };
        assert_eq!(hero(&[fine.clone(), empty], None).title, "1 repository needs your attention");
        let fresh = RepoView { last_check: None, ..view("o/f", Vec::new()) };
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
    fn the_list_says_how_each_repository_is_in_a_few_words() {
        assert_eq!(nav_sub(&view("o/a", vec![ok("x"), ok("y")])), "2 skills · up to date");
        assert_eq!(nav_sub(&view("o/a", vec![ok("x"), edited("e")])), "2 skills · 1 needs you");
        assert_eq!(nav_sub(&view("o/a", vec![edited("d"), edited("e")])), "2 skills · 2 need you");
        assert_eq!(nav_sub(&view("o/a", vec![ok("x")])), "1 skill · up to date");
        assert_eq!(nav_sub(&unreadable("o/c")), "Not found");
        assert_eq!(nav_sub(&RepoView { enabled: false, ..unreadable("o/c") }), "Paused");
        assert_eq!(nav_sub(&RepoView { status: "Not checked yet".to_string(), last_check: None, ..view("o/a", Vec::new()) }), "Not checked yet");

        // Both lines are the entry's window text, for a screen reader; Add repository has only one.
        let views = [view("o/a", vec![ok("x"), edited("e")])];
        let s = nav(&views, &Page::Activity, NAV_W, 600, &fit);
        assert_eq!(item(&s, id::NAV_REPO).text, "o/a\n2 skills · 1 needs you");
        assert_eq!(item(&s, id::NAV_ADD).text, "Add repository");
    }

    #[test]
    fn a_repository_says_where_its_skills_come_from_and_how_it_is_read() {
        let r = repo("o/a");
        assert_eq!(repo_sub(&r, &view("o/a", Vec::new()), NOW), "Public · the skills folder on main · checked 2 minutes ago");
        let private = RepoView { private: true, ..view("o/a", Vec::new()) };
        assert_eq!(repo_sub(&r, &private, NOW), "The skills folder on main · read with your GitHub sign-in · checked 2 minutes ago");
        let top = RepoConfig { path: String::new(), branch: String::new(), ..repo("o/a") };
        assert_eq!(
            repo_sub(&top, &RepoView { last_check: None, ..view("o/a", Vec::new()) }, NOW),
            "The top of the repository on the default branch · not checked yet"
        );
        assert_eq!(
            repo_sub(&top, &RepoView { last_check: None, ..unreadable("o/a") }, NOW),
            "The top of the repository on the default branch · not checked successfully yet"
        );
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
        let r = repo("o/a");
        let v = view("o/a", vec![edited("rlo-\u{202e}gpj"), ok("alpha")]);
        let none = HashSet::new();
        let page = |toggled: &HashSet<String>| repo_page(&RepoPage { view: &v, repo: &r, default: DEFAULT_INTERVAL, now: NOW, toggled }, 600, &fit);

        let s = page(&none);
        // The status is in the window text too, after a line break, so a screen reader reads it with the name.
        assert_eq!(item(&s, id::SKILL).text, "rlo-\\u{202e}gpj\nEdited on this PC");
        assert_eq!(hit(id::SKILL), Hit::Skill(0, Part::Row));
        assert_eq!(item(&s, id::SKILL + 8).text, "alpha\nUp to date");
        assert_eq!(hit(id::SKILL + 8), Hit::Skill(1, Part::Row));
        // The skill that needs the user is open: its choices are there, Show changes first and accented.
        assert_eq!(item(&s, id::SKILL + 2).look, Look::Button { kind: Kind::Accent, glyph: Some(glyph::FILE) });
        assert_eq!(item(&s, id::SKILL + 4).text, "Use the GitHub version");
        assert_eq!(hit(id::SKILL + 5), Hit::Skill(0, Part::Action(1)));
        assert_eq!(hit(id::SKILL + 3), Hit::Skill(0, Part::Open));
        assert!(!s.items.iter().any(|i| i.id == id::SKILL + 8 + 3), "a skill that is fine is closed");
        fits_in(&s, 600);

        let toggled: HashSet<String> = ["rlo-\u{202e}gpj".to_string(), "alpha".to_string()].into();
        let s = page(&toggled);
        assert!(!s.items.iter().any(|i| i.id == id::SKILL + 2), "closed by the user");
        assert_eq!(item(&s, id::SKILL + 8 + 3).text, "Open folder", "opened by the user");
    }

    #[test]
    fn every_page_fits_a_narrow_and_a_wide_window() {
        let r = RepoConfig { interval: Some(Interval::new(2, Unit::Hour)), ..repo("o/a") };
        let v = RepoView { private: true, detail: Some("GitHub answered not found.".to_string()), ..view("o/a", vec![edited("e"), ok("x")]) };
        let none = HashSet::new();
        let settings = SettingsPage { default: DEFAULT_INTERVAL, skills_dir: r"C:\Users\someone\.copilot\skills", token_saved: true, version: "0.2.0" };
        for w in [344, 900] {
            let p = RepoPage { view: &v, repo: &r, default: DEFAULT_INTERVAL, now: NOW, toggled: &none };
            let s = repo_page(&p, w, &fit);
            fits_in(&s, w);
            assert_eq!(item(&s, id::EVERY).text, "2 hours");
            assert!(item(&s, id::PRIVATE).at[0] > 24);
            let mut tops: Vec<i32> = s.items.iter().map(|i| i.at[1]).collect();
            tops.dedup();
            assert!(s.height > *tops.iter().max().unwrap_or(&0));
            fits_in(&settings_page(&settings, w, &fit), w);
            fits_in(&welcome(w, &fit), w);
            fits_in(&nav(std::slice::from_ref(&v), &Page::Settings, NAV_W, 480, &fit), NAV_W);
            fits_in(&hero(&hero_of(std::slice::from_ref(&v), None, "", false, NOW), true, w + NAV_W, &fit), w + NAV_W);
        }
    }

    #[test]
    fn the_settings_page_offers_to_replace_a_saved_token_and_to_save_one_otherwise() {
        let p = |token_saved| SettingsPage { default: DEFAULT_INTERVAL, skills_dir: "C:\\skills", token_saved, version: "0.2.0" };
        let saved = settings_page(&p(true), 700, &fit);
        assert_eq!(item(&saved, id::SAVE_TOKEN).text, "Replace token…");
        assert_eq!(item(&saved, id::DELETE_TOKEN).text, "Delete token");
        assert_eq!(item(&saved, id::DEFAULT_EVERY).text, "1 day");
        let none = settings_page(&p(false), 700, &fit);
        assert_eq!(item(&none, id::SAVE_TOKEN).text, "Save a token…");
        assert!(!none.items.iter().any(|i| i.id == id::DELETE_TOKEN));
    }

    #[test]
    fn each_kind_of_difference_is_summed_up_in_the_list_of_files() {
        let bytes = |s: &str| Content::Bytes(s.as_bytes().to_vec());
        let github = vec![
            ("SKILL.md".to_string(), bytes("a\nb\n")),
            ("gone.md".to_string(), bytes("x\n")),
            ("crlf.md".to_string(), bytes("l\n")),
            ("logo.png".to_string(), bytes("\0png")),
        ];
        let here = vec![
            ("SKILL.md".to_string(), bytes("a\nc\nd\n")),
            ("new.md".to_string(), bytes("n\n")),
            ("crlf.md".to_string(), bytes("l\r\n")),
            ("logo.png".to_string(), bytes("\0gif")),
        ];
        let summaries: Vec<(String, String)> = compare(github, here).iter().map(|c| (c.path.clone(), file_summary(c).2)).collect();
        let find = |p: &str| summaries.iter().find(|(path, _)| path == p).map(|(_, s)| s.as_str());
        assert_eq!(find("SKILL.md"), Some("Changed · 2 added, 1 removed"));
        assert_eq!(find("new.md"), Some("Only on this PC"));
        assert_eq!(find("gone.md"), Some("Only on GitHub"));
        assert_eq!(find("crlf.md"), Some("Only invisible changes"));
        assert_eq!(find("logo.png"), Some("Different (not text)"));
        let hidden = FileChange { path: "big".to_string(), presence: Presence::Both, body: Body::Hidden("it is too big to show".to_string()) };
        assert_eq!(file_summary(&hidden).2, "Not shown");
    }

    fn diff_of(old: &str, new: &str) -> DiffView {
        let change = FileChange { path: "SKILL.md".to_string(), presence: Presence::Both, body: Body::Text(text_diff(old, new)) };
        DiffView {
            repo_id: "o/a".to_string(),
            repo_label: "o/a".to_string(),
            skill: "beta".to_string(),
            actions: vec![Action::Overwrite, Action::KeepMine],
            report: Report::Ready(DiffReport { on_github: true, here: true, files: vec![change] }),
            file: 0,
            gaps: Vec::new(),
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
    fn the_diff_header_offers_the_skills_choices_with_the_github_version_accented() {
        let v = diff_of("a\n", "b\n");
        let s = diff_head(&v, 900, &fit);
        assert_eq!(item(&s, id::DIFF_ACTION).look, Look::Button { kind: Kind::Accent, glyph: None });
        assert_eq!(item(&s, id::DIFF_ACTION + 1).text, "Keep my version");
        assert!(item(&s, id::DIFF_ACTION).at[0] > item(&s, id::DIFF_ACTION + 1).at[0], "the accented choice is last");
        assert_eq!(hit(id::DIFF_ACTION + 1), Hit::DiffAction(1));
        assert_eq!(item(&s, id::TITLE).text, "beta");
        fits_in(&s, 900);
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
        let views = vec![view("o/a", vec![ok("x")]), view("o/b", vec![edited("e")]), unreadable("o/c")];
        assert_eq!(settle(shown("o/a"), &views), shown("o/a"));
        assert_eq!(settle(Some(Page::Settings), &views), Some(Page::Settings));
        assert_eq!(settle(Some(Page::Activity), &[]), Some(Page::Activity));
        assert_eq!(settle(shown("o/removed"), &views), shown("o/b"));
        assert_eq!(settle(None, &views), shown("o/b"));
        let paused = vec![view("o/a", vec![ok("x")]), RepoView { enabled: false, ..view("o/b", vec![edited("e")]) }];
        assert_eq!(settle(None, &paused), shown("o/a"), "a paused repository does not ask for the user");
        assert_eq!(settle(shown("o/a"), &[]), None, "the welcome page, when there is nothing else");
    }

    #[test]
    fn add_says_how_it_reads_what_was_typed_and_finds_a_repository_already_set_up() {
        let repos = vec![repo("Owner/Skills"), RepoConfig { branch: String::new(), path: String::new(), ..repo("Owner/Top") }];
        assert_eq!(spec_hint(" owner/other ", &repos), Ok("owner/other · the top of the repository on the default branch".to_string()));
        assert_eq!(spec_hint("https://github.com/o/r/tree/dev/x", &[]), Ok("o/r · the x folder on dev".to_string()));
        let already = |r: &RepoConfig| Err(format!("{} is already set up", r.label()));
        assert_eq!(spec_hint("https://github.com/owner/skills/tree/main/skills", &repos), already(&repos[0]), "in any case");
        assert_eq!(spec_hint("OWNER/top", &repos), already(&repos[1]));
        assert!(spec_hint("owner/skills", &repos).is_ok(), "the top of a repository whose skills folder is set up is another source");
        assert_eq!(spec_hint("", &repos), Err("Enter a repository like owner/name".to_string()));
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
    }
}
