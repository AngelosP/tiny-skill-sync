//! Rules for names and relative paths that come from a remote repository.
//!
//! Everything downloaded ends up as a file on a Windows disk, so a name must be safe there: no traversal,
//! no reserved device names, no characters Windows rejects, and no two names that differ only by case.

use std::borrow::Cow;
use std::fmt;

/// The longest full path we install. Windows tools often still fail above 259 characters, even with long paths on.
pub const MAX_INSTALLED_PATH: usize = 259;
pub const MAX_COMPONENT_UTF16: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    Traversal,
    BadCharacter(char),
    TrailingDotOrSpace,
    Reserved(String),
    TooLong,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::Empty => write!(f, "empty name"),
            PathError::Traversal => write!(f, "'.' or '..' is not allowed"),
            PathError::BadCharacter(c) => write!(f, "character {c:?} is not allowed in Windows names"),
            PathError::TrailingDotOrSpace => write!(f, "a name cannot end with a dot or a space"),
            PathError::Reserved(n) => write!(f, "'{n}' is a reserved Windows device name"),
            PathError::TooLong => write!(f, "name is too long"),
        }
    }
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
    "LPT8", "LPT9",
];

fn is_reserved(name: &str) -> bool {
    // Device names are reserved with or without an extension ("CON.txt"), and the superscript digit forms too.
    let stem = name.split('.').next().unwrap_or(name).trim_end().to_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return true;
    }
    let mut chars = stem.chars();
    let head: String = chars.by_ref().take(3).collect();
    matches!((head.as_str(), chars.next(), chars.next()), ("COM" | "LPT", Some('\u{b9}' | '\u{b2}' | '\u{b3}'), None))
}

/// One path segment: a skill folder name, a file name or a directory name.
pub fn check_component(name: &str) -> Result<(), PathError> {
    if name.is_empty() {
        return Err(PathError::Empty);
    }
    if name == "." || name == ".." {
        return Err(PathError::Traversal);
    }
    if let Some(c) = name.chars().find(|c| (*c as u32) < 0x20 || "<>:\"/\\|?*".contains(*c) || *c == '\u{7f}') {
        return Err(PathError::BadCharacter(c));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(PathError::TrailingDotOrSpace);
    }
    if is_reserved(name) {
        return Err(PathError::Reserved(name.to_string()));
    }
    if name.encode_utf16().count() > MAX_COMPONENT_UTF16 {
        return Err(PathError::TooLong);
    }
    Ok(())
}

/// A relative path with `/` separators, as Git writes it. No leading or trailing slash, no empty segments.
pub fn check_rel_path(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    path.split('/').try_for_each(check_component)
}

/// The key two names share when Windows would treat them as the same file.
pub fn fold(name: &str) -> String {
    name.to_lowercase()
}

/// Text that came from a repository, safe to show: control characters (an escape sequence can move the cursor or
/// erase what is above it), line and paragraph separators, and the characters that reorder text (so `exe.pdf` cannot
/// read as `pdf.exe`) are written out as `\u{1b}` and the like. The line break is escaped too, so this is for one
/// line; a name Windows accepts can still hold all of these.
pub fn printable(text: &str) -> Cow<'_, str> {
    escape(text, false)
}

/// Like `printable`, but line breaks (`\n`) stay, for text that is several lines long.
pub fn printable_lines(text: &str) -> Cow<'_, str> {
    escape(text, true)
}

fn needs_escape(c: char, keep_newlines: bool) -> bool {
    if c == '\n' && keep_newlines {
        return false;
    }
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}' | '\u{61c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn escape(text: &str, keep_newlines: bool) -> Cow<'_, str> {
    if !text.chars().any(|c| needs_escape(c, keep_newlines)) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if needs_escape(c, keep_newlines) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Normalizes a folder path typed by a person: slashes, surrounding spaces and stray slashes. Empty means the repository root.
pub fn normalize_repo_dir(input: &str) -> Result<String, PathError> {
    let cleaned = input.trim().replace('\\', "/");
    let trimmed = cleaned.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    check_rel_path(trimmed)?;
    Ok(trimmed.to_string())
}

/// Length in characters of the path `root\name\rel` once installed.
pub fn installed_len(root_len: usize, name: &str, rel: &str) -> usize {
    root_len + 1 + name.chars().count() + 1 + rel.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        for ok in ["SKILL.md", "alpha", "my skill", "a-b_c.d", "ünïcode", "file.tar.gz", ".hidden", "COM10", "CONSOLE", "AUXILIARY.txt"] {
            assert_eq!(check_component(ok), Ok(()), "{ok:?}");
        }
    }

    #[test]
    fn rejects_traversal_and_empty() {
        assert_eq!(check_component(""), Err(PathError::Empty));
        assert_eq!(check_component("."), Err(PathError::Traversal));
        assert_eq!(check_component(".."), Err(PathError::Traversal));
        assert!(check_rel_path("a/../b").is_err());
        assert!(check_rel_path("a//b").is_err());
        assert!(check_rel_path("/a").is_err());
        assert!(check_rel_path("a/").is_err());
        assert!(check_rel_path("./a").is_err());
    }

    #[test]
    fn rejects_characters_windows_refuses() {
        for bad in ["a:b", "a\\b", "a|b", "a?b", "a*b", "a<b", "a>b", "a\"b", "a\u{1}b", "a\tb", "a\u{7f}b"] {
            assert!(matches!(check_component(bad), Err(PathError::BadCharacter(_))), "{bad:?}");
        }
        assert!(check_rel_path("dir\\file").is_err());
        assert!(check_rel_path("C:/x").is_err());
    }

    #[test]
    fn rejects_trailing_dot_or_space() {
        assert_eq!(check_component("name."), Err(PathError::TrailingDotOrSpace));
        assert_eq!(check_component("name "), Err(PathError::TrailingDotOrSpace));
    }

    #[test]
    fn rejects_reserved_device_names() {
        for bad in ["CON", "con", "Nul", "aux.txt", "COM1", "lpt9.log", "PRN.", "COM\u{b9}", "LPT\u{b2}.txt"] {
            assert!(check_component(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn rejects_overlong_components() {
        assert!(check_component(&"a".repeat(255)).is_ok());
        assert_eq!(check_component(&"a".repeat(256)), Err(PathError::TooLong));
    }

    #[test]
    fn repo_dir_normalization() {
        assert_eq!(normalize_repo_dir(""), Ok(String::new()));
        assert_eq!(normalize_repo_dir("  /skills/ "), Ok("skills".to_string()));
        assert_eq!(normalize_repo_dir("a\\b"), Ok("a/b".to_string()));
        assert!(normalize_repo_dir("a/../b").is_err());
        assert!(normalize_repo_dir("a:b").is_err());
    }

    #[test]
    fn fold_makes_case_variants_equal() {
        assert_eq!(fold("Skill.MD"), fold("skill.md"));
        assert_ne!(fold("a"), fold("b"));
    }

    #[test]
    fn installed_length_counts_both_separators() {
        assert_eq!(installed_len(10, "ab", "c/d.md"), 10 + 1 + 2 + 1 + 6);
    }

    #[test]
    fn printable_text_is_left_exactly_as_it_is() {
        for same in ["alpha", "my skill", "caf\u{e9}", "\u{65e5}\u{672c}\u{8a9e}", "a\u{200d}b", "x (changed: a.md, b.md)", ""] {
            assert!(matches!(printable(same), Cow::Borrowed(s) if s == same), "{same:?}");
        }
    }

    #[test]
    fn printable_writes_out_what_could_steer_a_terminal_or_reorder_text() {
        assert_eq!(printable("a\u{1b}[2K\rb"), "a\\u{1b}[2K\\u{d}b");
        assert_eq!(printable("a\nb\tc\u{0}"), "a\\u{a}b\\u{9}c\\u{0}");
        assert_eq!(printable("a\u{7f}b\u{9b}c"), "a\\u{7f}b\\u{9b}c");
        // Line and paragraph separators, direction marks and overrides, and the isolates that came after them.
        for c in [
            '\u{2028}', '\u{2029}', '\u{200e}', '\u{200f}', '\u{61c}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}',
        ] {
            let text = format!("ex{c}e");
            let shown = printable(&text);
            assert!(!shown.contains(c), "U+{:04X} was shown as it is", c as u32);
            assert_eq!(shown, format!("ex\\u{{{:x}}}e", c as u32));
        }
    }

    #[test]
    fn printable_lines_keep_line_breaks_only() {
        assert_eq!(printable_lines("one\ntwo\u{1b}\nthree\r"), "one\ntwo\\u{1b}\nthree\\u{d}");
        assert!(matches!(printable_lines("one\ntwo"), Cow::Borrowed(_)));
    }

    #[test]
    fn what_a_skill_name_may_contain_and_what_is_shown_agree() {
        // A name that passes `check_component` can still hold characters that must not reach a screen as they are.
        for name in ["a\u{202e}b", "a\u{2029}b", "a\u{9b}b"] {
            assert_eq!(check_component(name), Ok(()), "{name:?} is installable");
            assert_ne!(printable(name), name, "{name:?} is shown as it is");
        }
    }
}
