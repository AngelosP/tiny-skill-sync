//! When two copies of a file are the same. Byte for byte, except for text: how lines end (`\r\n` or `\n`), the line
//! break at the end of the file and blank lines do not count. A copy saved by a Windows editor, or with a blank line
//! added or taken out, is still the same skill. This holds everywhere: finding edits, updating and removing a skill,
//! taking over a folder that is already there, and "Show changes". Pure.

use crate::hash::{Sha1, hex};

/// Longer files are compared byte for byte, because comparing text reads the whole file into memory.
pub const TEXT_LIMIT: u64 = 1 << 20;

/// The text of a file, or `None` when it is not text (not UTF-8, or it has a NUL in it).
pub fn decode(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok().filter(|text| !text.contains('\0'))
}

/// Holds nothing but spaces and tabs.
pub fn is_blank(line: &str) -> bool {
    line.bytes().all(|b| b == b' ' || b == b'\t')
}

/// Every line of a text with its number (from 1), without its line break. `\n` and `\r\n` both end a line, and a line
/// break at the very end makes no line.
pub fn numbered_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    let lines = (!text.is_empty()).then(|| body.split('\n')).into_iter().flatten();
    lines.map(|line| line.strip_suffix('\r').unwrap_or(line)).enumerate().map(|(i, line)| (i + 1, line))
}

/// The lines that count, with their numbers: every line that is not blank.
pub fn counted_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    numbered_lines(text).filter(|(_, line)| !is_blank(line))
}

fn text_of(bytes: &[u8]) -> Option<&str> {
    if bytes.len() as u64 > TEXT_LIMIT { None } else { decode(bytes) }
}

/// What a file is compared by, as kept in the record of an installed skill: a hash of the lines that count, or an
/// empty string when the file is compared byte for byte (it is not text, or it is longer than [`TEXT_LIMIT`]).
pub fn canon(bytes: &[u8]) -> String {
    let Some(text) = text_of(bytes) else { return String::new() };
    let mut sha = Sha1::new();
    for (_, line) in counted_lines(text) {
        sha.update(line.as_bytes());
        sha.update(b"\n");
    }
    hex(&sha.finish())
}

/// Are two copies of a file the same?
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a == b
        || match (text_of(a), text_of(b)) {
            (Some(a), Some(b)) => counted_lines(a).map(|(_, line)| line).eq(counted_lines(b).map(|(_, line)| line)),
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<(usize, &str)> {
        numbered_lines(text).collect()
    }

    #[test]
    fn lines_end_with_either_line_break_and_the_last_one_makes_no_line() {
        assert_eq!(lines("a\r\nb\n"), [(1, "a"), (2, "b")]);
        assert_eq!(lines("a\nb"), [(1, "a"), (2, "b")]);
        assert_eq!(lines("a\n\nb\n\n"), [(1, "a"), (2, ""), (3, "b"), (4, "")]);
        assert_eq!(lines("a\rb\r\n"), [(1, "a\rb")], "a lone carriage return is part of the line");
        assert!(lines("").is_empty());
        assert_eq!(lines("\n"), [(1, "")]);
    }

    #[test]
    fn line_breaks_the_last_line_break_and_blank_lines_do_not_count() {
        let unix = b"---\nname: alpha\n---\n\n# Alpha\n\nDo the thing.\n";
        for copy in [
            &b"---\r\nname: alpha\r\n---\r\n\r\n# Alpha\r\n\r\nDo the thing.\r\n"[..],
            b"---\nname: alpha\n---\n\n# Alpha\n\nDo the thing.",
            b"---\nname: alpha\n---\n# Alpha\nDo the thing.\n",
            b"\n\n---\nname: alpha\n---\n\n\n\n# Alpha\n \t \nDo the thing.\n\n\n",
            b"---\r\nname: alpha\n---\r\n# Alpha\r\n\r\n\r\nDo the thing.\r\n\r\n",
        ] {
            assert!(same(unix, copy), "{:?}", String::from_utf8_lossy(copy));
            assert_eq!(canon(unix), canon(copy));
        }
        assert_eq!(canon(unix).len(), 40);
        assert!(same(b"", b"\r\n\r\n"), "an empty file and blank lines");
    }

    #[test]
    fn any_other_change_counts() {
        let unix = b"# Alpha\n\nDo the thing.\n";
        for other in [
            &b"# Alpha\n\nDo the other thing.\n"[..],
            b"# Alpha\n\n  Do the thing.\n",
            b"# Alpha\n\nDo the thing. \n",
            b"# Alpha\nDo the thing.\nDo the thing.\n",
            b"Do the thing.\n# Alpha\n",
            b"# Alpha Do the thing.\n",
            b"# Alpha\r\rDo the thing.\n",
            "\u{feff}# Alpha\n\nDo the thing.\n".as_bytes(),
        ] {
            assert!(!same(unix, other), "{:?}", String::from_utf8_lossy(other));
            assert_ne!(canon(unix), canon(other));
        }
    }

    #[test]
    fn files_that_are_not_text_or_too_long_are_compared_byte_for_byte() {
        let nul = b"a\0\r\nb\r\n";
        assert_eq!(canon(nul), "");
        assert!(!same(nul, b"a\0\nb\n"));
        assert!(same(nul, nul));
        let latin1 = b"caf\xe9\r\n";
        assert_eq!(canon(latin1), "");
        assert!(!same(latin1, b"caf\xe9\n"));

        let long = "x\r\n".repeat(TEXT_LIMIT as usize / 3 + 1);
        assert_eq!(canon(long.as_bytes()), "");
        assert!(!same(long.as_bytes(), long.replace('\r', "").as_bytes()));
        let fits = "x\r\n".repeat(TEXT_LIMIT as usize / 3);
        assert!(same(fits.as_bytes(), fits.replace('\r', "").as_bytes()));
    }
}
