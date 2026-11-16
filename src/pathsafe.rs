//! Rules for names and relative paths that come from a remote repository.
//!
//! Everything downloaded ends up as a file on a Windows disk, so a name must be safe there: no traversal,
//! no reserved device names, no characters Windows rejects, and no two names that differ only by case.

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
}
