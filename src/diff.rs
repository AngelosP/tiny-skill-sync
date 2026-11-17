//! What is different between the GitHub copy of a skill and the copy on this PC, for the "Show changes" view. Pure: the
//! engine reads both sides and hands the bytes in. GitHub is the old side (`-`), this PC the new one (`+`).

use std::collections::{BTreeMap, HashMap};

use crate::pathsafe::fold;

/// Unchanged lines shown on each side of a change.
pub const CONTEXT: usize = 3;
/// A run of unchanged lines is folded only when that hides at least this many lines.
const MIN_FOLD: usize = 2;

/// How far the search for the shortest script goes. Beyond either limit, what lies between the first and the last
/// difference is shown as removed and added as a whole. `edits` bounds the memory (the search keeps about `edits`
/// squared numbers), `steps` the time, which long runs of repeated lines can make large.
#[derive(Clone, Copy, Debug)]
struct Limits {
    edits: usize,
    steps: u64,
}

const LIMITS: Limits = Limits { edits: 1_000, steps: 20_000_000 };

/// One line of the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// The same on both sides. Line numbers count from 1.
    Same { old: usize, new: usize },
    /// Only on GitHub.
    Removed { old: usize },
    /// Only on this PC.
    Added { new: usize },
    /// `count` unchanged lines that are not shown, starting at these line numbers.
    Folded { old: usize, new: usize, count: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextDiff {
    /// The lines on GitHub, without their line breaks.
    pub old: Vec<String>,
    /// The lines on this PC.
    pub new: Vec<String>,
    pub rows: Vec<Row>,
    /// Lines only on this PC.
    pub added: usize,
    /// Lines only on GitHub.
    pub removed: usize,
}

impl TextDiff {
    /// The bytes differ but every line is the same: only the line breaks, a byte order mark or the line break at the
    /// end of the file are different.
    pub fn only_invisible(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Both,
    OnlyHere,
    OnlyOnGitHub,
}

/// One side of a file, as the engine read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Bytes(Vec<u8>),
    /// Not read, and why (it is too big to show, or a link, which is never followed).
    Hidden(String),
}

/// What is shown for a file that is different.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// A file on one side only is compared with an empty one, so all its lines show as added or removed.
    Text(TextDiff),
    /// A side is not text (not UTF-8, or it has a NUL in it): it is only said to be different.
    NotText,
    /// A side was not read, and why.
    Hidden(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Relative to the skill folder, with `/` separators. GitHub's spelling when both sides have the file.
    pub path: String,
    pub presence: Presence,
    pub body: Body,
}

/// The files that are different, by path. Files are matched as Windows matches names, whatever their case; files with
/// the same bytes on both sides are left out. A file that was not read on a side is taken to be different: the caller
/// leaves out the files it knows are the same.
pub fn compare(github: Vec<(String, Content)>, here: Vec<(String, Content)>) -> Vec<FileChange> {
    let mut theirs: BTreeMap<String, (String, Content)> = github.into_iter().map(|(path, content)| (fold(&path), (path, content))).collect();
    let mut out = Vec::new();
    let none = Content::Bytes(Vec::new());
    for (path, mine) in here {
        match theirs.remove(&fold(&path)) {
            Some((_, Content::Bytes(old))) if matches!(&mine, Content::Bytes(new) if *new == old) => {}
            Some((name, old)) => out.push(FileChange { path: name, presence: Presence::Both, body: body(&old, &mine) }),
            None => out.push(FileChange { path, presence: Presence::OnlyHere, body: body(&none, &mine) }),
        }
    }
    for (path, old) in theirs.into_values() {
        out.push(FileChange { path, presence: Presence::OnlyOnGitHub, body: body(&old, &none) });
    }
    out.sort_by_cached_key(|change| fold(&change.path));
    out
}

fn body(old: &Content, new: &Content) -> Body {
    match (old, new) {
        (Content::Hidden(why), _) | (_, Content::Hidden(why)) => Body::Hidden(why.clone()),
        (Content::Bytes(old), Content::Bytes(new)) => match (decode(old), decode(new)) {
            (Some(old), Some(new)) => Body::Text(text_diff(old, new)),
            _ => Body::NotText,
        },
    }
}

/// The text of a file, or `None` when it is not text. A byte order mark is not part of the text.
fn decode(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok().filter(|text| !text.contains('\0'))?;
    Some(text.strip_prefix('\u{feff}').unwrap_or(text))
}

/// The lines of two texts, compared. `\n` and `\r\n` both end a line, and a line break at the very end makes no line.
pub fn text_diff(old: &str, new: &str) -> TextDiff {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let ops = script(&old, &new, LIMITS);
    let removed = ops.iter().filter(|&&op| op == Op::Del).count();
    let added = ops.iter().filter(|&&op| op == Op::Ins).count();
    TextDiff {
        rows: rows(&ops, CONTEXT),
        old: old.into_iter().map(str::to_string).collect(),
        new: new.into_iter().map(str::to_string).collect(),
        added,
        removed,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Same,
    /// A line of the old side only.
    Del,
    /// A line of the new side only.
    Ins,
}

/// How to turn `old` into `new`, line by line, with as few lines added and removed as the limits allow. Within each
/// change the removed lines come first.
fn script<'a>(old: &[&'a str], new: &[&'a str], limits: Limits) -> Vec<Op> {
    // Lines are compared as numbers, one per distinct text.
    let mut ids: HashMap<&'a str, u32> = HashMap::new();
    let mut id = |line: &'a str| -> u32 {
        let next = ids.len() as u32;
        *ids.entry(line).or_insert(next)
    };
    let a: Vec<u32> = old.iter().map(|line| id(line)).collect();
    let b: Vec<u32> = new.iter().map(|line| id(line)).collect();

    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..].iter().rev().zip(b[head..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (a_mid, b_mid) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    let middle =
        shortest(a_mid, b_mid, limits).unwrap_or_else(|| std::iter::repeat_n(Op::Del, a_mid.len()).chain(std::iter::repeat_n(Op::Ins, b_mid.len())).collect());

    let mut ops = Vec::with_capacity(head + middle.len() + tail);
    ops.extend(std::iter::repeat_n(Op::Same, head));
    ops.extend(middle);
    ops.extend(std::iter::repeat_n(Op::Same, tail));
    ops
}

/// The shortest edit script (Myers, "An O(ND) difference algorithm", 1986), or `None` beyond the limits. Round `d`
/// finds, for each diagonal `k = x - y`, the furthest point reachable with `d` edits.
fn shortest(a: &[u32], b: &[u32], limits: Limits) -> Option<Vec<Op>> {
    // Files are at most a few MiB, so line counts fit.
    let (n, m) = (i32::try_from(a.len()).ok()?, i32::try_from(b.len()).ok()?);
    let limit = i32::try_from(limits.edits).unwrap_or(i32::MAX).min(n.saturating_add(m));
    let off = limit + 1;
    let mut v = vec![0i32; 2 * limit as usize + 3];
    // `trace[d]` is what round `d` started from, for the diagonals `-d..=d`: the walk back needs it.
    let mut trace: Vec<Vec<i32>> = Vec::new();
    let mut steps: u64 = 0;
    for d in 0..=limit {
        trace.push(v[(off - d) as usize..=(off + d) as usize].to_vec());
        for k in (-d..=d).step_by(2) {
            // Going on from the further of the two neighbours also puts the removed lines of each change before the
            // added ones: an added line followed by a removed one always reaches less far than the reverse.
            let down = k == -d || (k != d && v[(off + k - 1) as usize] < v[(off + k + 1) as usize]);
            let mut x = if down { v[(off + k + 1) as usize] } else { v[(off + k - 1) as usize] + 1 };
            let mut y = x - k;
            let from = x;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            steps += 1 + (x - from) as u64;
            if steps > limits.steps {
                return None;
            }
            v[(off + k) as usize] = x;
            if x >= n && y >= m {
                return Some(walk_back(&trace, n, m, d));
            }
        }
    }
    None
}

fn walk_back(trace: &[Vec<i32>], n: i32, m: i32, end: i32) -> Vec<Op> {
    let mut ops = Vec::with_capacity((n + m) as usize);
    let (mut x, mut y) = (n, m);
    for d in (1..=end).rev() {
        let v = &trace[d as usize];
        let at = |k: i32| v[(k + d) as usize];
        let k = x - y;
        let down = k == -d || (k != d && at(k - 1) < at(k + 1));
        let prev_k = if down { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Same);
            x -= 1;
            y -= 1;
        }
        ops.push(if down { Op::Ins } else { Op::Del });
        x = prev_x;
        y = prev_y;
    }
    // Round 0 is the run of equal lines the script starts with.
    ops.extend(std::iter::repeat_n(Op::Same, x as usize));
    ops.reverse();
    ops
}

/// The rows of the view: every change, `context` unchanged lines around each, and the rest folded.
fn rows(ops: &[Op], context: usize) -> Vec<Row> {
    let mut out = Vec::new();
    let (mut old, mut new) = (1, 1);
    let mut i = 0;
    while i < ops.len() {
        match ops[i] {
            Op::Del => {
                out.push(Row::Removed { old });
                old += 1;
                i += 1;
            }
            Op::Ins => {
                out.push(Row::Added { new });
                new += 1;
                i += 1;
            }
            Op::Same => {
                let start = i;
                while i < ops.len() && ops[i] == Op::Same {
                    i += 1;
                }
                let run = i - start;
                // Lines shown after the change before this run, and before the change after it.
                let lead = if start == 0 { 0 } else { context };
                let trail = if i == ops.len() { 0 } else { context };
                let same = |j: usize| Row::Same { old: old + j, new: new + j };
                if run >= lead + trail + MIN_FOLD {
                    let hidden = run - lead - trail;
                    out.extend((0..lead).map(same));
                    out.push(Row::Folded { old: old + lead, new: new + lead, count: hidden });
                    out.extend((lead + hidden..run).map(same));
                } else {
                    out.extend((0..run).map(same));
                }
                old += run;
                new += run;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(count: usize) -> Vec<String> {
        (1..=count).map(|n| format!("line {n}")).collect()
    }

    fn joined(lines: &[String]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    fn same(old: usize, new: usize) -> Row {
        Row::Same { old, new }
    }

    /// Applies the script to `a` and checks that it gives `b`. Returns the number of lines added and removed.
    fn replay(a: &[&str], b: &[&str], ops: &[Op]) -> usize {
        let (mut i, mut j, mut edits) = (0, 0, 0);
        for op in ops {
            match op {
                Op::Same => {
                    assert_eq!(a[i], b[j], "a line kept the same is the same: {ops:?}");
                    i += 1;
                    j += 1;
                }
                Op::Del => {
                    i += 1;
                    edits += 1;
                }
                Op::Ins => {
                    j += 1;
                    edits += 1;
                }
            }
        }
        assert_eq!((i, j), (a.len(), b.len()), "the script covers both sides: {ops:?}");
        edits
    }

    fn lcs(a: &[&str], b: &[&str]) -> usize {
        let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                table[i][j] = if a[i] == b[j] { table[i + 1][j + 1] + 1 } else { table[i + 1][j].max(table[i][j + 1]) };
            }
        }
        table[0][0]
    }

    #[test]
    fn the_script_is_the_shortest_and_turns_one_side_into_the_other() {
        // A fixed pseudo-random sequence, so a failure can be run again.
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |below: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % below
        };
        const WORDS: [&str; 3] = ["a", "b", "c"];
        for _ in 0..2_000 {
            let a: Vec<&str> = (0..next(12)).map(|_| WORDS[next(3) as usize]).collect();
            let b: Vec<&str> = (0..next(12)).map(|_| WORDS[next(3) as usize]).collect();
            let ops = script(&a, &b, LIMITS);
            assert_eq!(replay(&a, &b, &ops), a.len() + b.len() - 2 * lcs(&a, &b), "{a:?} -> {b:?}: {ops:?}");
            for pair in ops.windows(2) {
                assert_ne!(pair, [Op::Ins, Op::Del], "removed lines come first in a change: {ops:?}");
            }
        }
    }

    #[test]
    fn beyond_either_limit_the_middle_is_replaced_as_a_whole() {
        let a = ["same", "a1", "a2", "a3", "end"];
        let b = ["same", "b1", "b2", "end"];
        let replaced = [Op::Same, Op::Del, Op::Del, Op::Del, Op::Ins, Op::Ins, Op::Same];
        assert_eq!(script(&a, &b, Limits { edits: 3, ..LIMITS }), replaced);
        assert_eq!(script(&a, &b, Limits { edits: 5, ..LIMITS }), replaced, "within the limit the shortest script is the same here");

        // Long runs of one repeated line make each round of the search long.
        let a: Vec<&str> = std::iter::repeat_n("x", 400).chain(["a"]).chain(std::iter::repeat_n("x", 400)).collect();
        let b: Vec<&str> = std::iter::repeat_n("x", 399).chain(["b", "x"]).chain(std::iter::repeat_n("y", 400)).collect();
        let short = script(&a, &b, LIMITS);
        let cut = script(&a, &b, Limits { steps: 1_000, ..LIMITS });
        assert!(replay(&a, &b, &cut) > replay(&a, &b, &short), "the cut script is valid, only longer");
        assert_eq!(replay(&a, &b, &short), a.len() + b.len() - 2 * lcs(&a, &b));
    }

    #[test]
    fn the_same_lines_are_one_folded_run() {
        let text = joined(&numbered(5));
        let diff = text_diff(&text, &text);
        assert!(diff.only_invisible());
        assert_eq!(diff.rows, [Row::Folded { old: 1, new: 1, count: 5 }]);
        assert_eq!(text_diff("", "").rows, []);
    }

    #[test]
    fn a_change_keeps_three_lines_around_it_and_folds_the_rest() {
        let old = numbered(20);
        let mut new = old.clone();
        new[9] = "changed".to_string();
        let diff = text_diff(&joined(&old), &joined(&new));
        assert_eq!((diff.added, diff.removed), (1, 1));
        assert_eq!(
            diff.rows,
            [
                Row::Folded { old: 1, new: 1, count: 6 },
                same(7, 7),
                same(8, 8),
                same(9, 9),
                Row::Removed { old: 10 },
                Row::Added { new: 10 },
                same(11, 11),
                same(12, 12),
                same(13, 13),
                Row::Folded { old: 14, new: 14, count: 7 },
            ]
        );
        assert_eq!(diff.old[9], "line 10");
        assert_eq!(diff.new[9], "changed");
    }

    #[test]
    fn line_numbers_follow_each_side_and_a_short_gap_is_shown_not_folded() {
        // Two changes with seven equal lines between them: three after the first, three before the second, and one
        // in the middle, which is not worth a fold.
        let old = numbered(12);
        let mut new = old.clone();
        new.remove(1);
        new.insert(8, "new".to_string());
        let diff = text_diff(&joined(&old), &joined(&new));
        assert_eq!((diff.added, diff.removed), (1, 1));
        let expected: Vec<Row> = [same(1, 1), Row::Removed { old: 2 }]
            .into_iter()
            .chain((3..=9).map(|n| same(n, n - 1)))
            .chain([Row::Added { new: 9 }])
            .chain((10..=12).map(|n| same(n, n)))
            .collect();
        assert_eq!(diff.rows, expected);
    }

    fn bytes(files: &[(&str, &[u8])]) -> Vec<(String, Content)> {
        files.iter().map(|(path, body)| (path.to_string(), Content::Bytes(body.to_vec()))).collect()
    }

    fn text_of(change: &FileChange) -> &TextDiff {
        match &change.body {
            Body::Text(diff) => diff,
            other => panic!("{} is not shown as text: {other:?}", change.path),
        }
    }

    #[test]
    fn line_breaks_a_byte_order_mark_and_the_last_line_break_are_not_changes() {
        let unix = "one\ntwo\n";
        assert!(text_diff(unix, "one\r\ntwo\r\n").only_invisible());
        assert!(text_diff(unix, "one\ntwo").only_invisible());
        let files = compare(bytes(&[("a.md", unix.as_bytes())]), bytes(&[("a.md", "\u{feff}one\r\ntwo".as_bytes())]));
        assert!(text_of(&files[0]).only_invisible(), "{files:?}");
        assert!(!text_diff(unix, "one\n\ntwo\n").only_invisible(), "an empty line is a line");
    }

    #[test]
    fn files_are_matched_whatever_their_case_and_files_that_are_the_same_are_left_out() {
        let github =
            bytes(&[("SKILL.md", b"name: x\nold\n"), ("same.txt", b"same\n"), ("gone.md", b"only upstream\n"), ("logo.png", &[0x89, b'P', b'N', b'G', 0, 1])]);
        let here =
            bytes(&[("skill.md", b"name: x\nnew\n"), ("Same.txt", b"same\n"), ("notes/mine.md", b"a\nb\n"), ("LOGO.png", &[0x89, b'P', b'N', b'G', 0, 2])]);
        let files = compare(github, here);
        let shown: Vec<(&str, Presence)> = files.iter().map(|f| (f.path.as_str(), f.presence)).collect();
        assert_eq!(
            shown,
            [("gone.md", Presence::OnlyOnGitHub), ("logo.png", Presence::Both), ("notes/mine.md", Presence::OnlyHere), ("SKILL.md", Presence::Both)]
        );
        let file = |path: &str| files.iter().find(|f| f.path == path).unwrap();
        assert_eq!(file("logo.png").body, Body::NotText, "not text: only said to be different");
        let gone = text_of(file("gone.md"));
        assert_eq!((gone.added, gone.removed, &gone.rows), (0, 1, &vec![Row::Removed { old: 1 }]));
        let mine = text_of(file("notes/mine.md"));
        assert_eq!((mine.added, mine.removed), (2, 0));
        assert_eq!(text_of(file("SKILL.md")).rows, [same(1, 1), Row::Removed { old: 2 }, Row::Added { new: 2 }]);
    }

    #[test]
    fn a_file_with_a_nul_or_bytes_that_are_not_utf8_is_not_text() {
        let files = compare(bytes(&[("a", b"x\0y"), ("b", &[0xff, 0xfe])]), bytes(&[("a", b"x"), ("b", b"x")]));
        assert!(files.iter().all(|f| f.body == Body::NotText), "{files:?}");
    }

    #[test]
    fn a_file_not_read_on_either_side_is_shown_with_the_reason() {
        let hidden = |why: &str| Content::Hidden(why.to_string());
        let github = vec![("big.bin".to_string(), hidden("too big")), ("a.md".to_string(), Content::Bytes(b"x\n".to_vec()))];
        let here = vec![("BIG.bin".to_string(), Content::Bytes(b"x\n".to_vec())), ("a.md".to_string(), hidden("a link")), ("l".to_string(), hidden("a link"))];
        let files = compare(github, here);
        let shown: Vec<(&str, Presence, &Body)> = files.iter().map(|f| (f.path.as_str(), f.presence, &f.body)).collect();
        assert_eq!(
            shown,
            [
                ("a.md", Presence::Both, &Body::Hidden("a link".into())),
                ("big.bin", Presence::Both, &Body::Hidden("too big".into())),
                ("l", Presence::OnlyHere, &Body::Hidden("a link".into())),
            ]
        );
    }
}
