//! GitHub source addresses, before and after splitting a tree URL at a known branch name.

use crate::config::{check_branch, check_owner, check_repo_name};
use crate::pathsafe;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub owner: String,
    pub name: String,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Leave the configured branch and folder empty to follow the current default branch.
    Bare,
    /// The first segment is the prefix to ask GitHub for. Only heads can resolve the split.
    Tree(Vec<String>),
}

impl Location {
    pub fn parse(input: &str) -> Result<Self, String> {
        let text = input.trim().split(['?', '#']).next().unwrap_or_default();
        let (path, url) = if let Some((scheme, rest)) = text.split_once("://") {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            if !scheme.eq_ignore_ascii_case("https") || !(host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("www.github.com")) {
                return Err("Use an HTTPS address on github.com.".into());
            }
            (path, true)
        } else {
            (text, false)
        };
        let path = path.strip_suffix('/').unwrap_or(path);
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() < 2 {
            return Err("Enter a repository like owner/name or paste its GitHub address.".into());
        }
        if !url && parts.len() != 2 {
            return Err("Use owner/name, or paste the GitHub address of the branch or folder.".into());
        }
        let mut parts = parts.into_iter().map(|part| if url { decode(part) } else { Ok(part.to_owned()) }).collect::<Result<Vec<_>, _>>()?.into_iter();
        let owner = parts.next().expect("at least two parts");
        let mut name = parts.next().expect("at least two parts");
        let target = match parts.next().as_deref() {
            None => {
                if url && name.ends_with(".git") {
                    name.truncate(name.len() - 4);
                }
                Target::Bare
            }
            Some("tree") => {
                let segments: Vec<_> = parts.collect();
                if segments.is_empty() {
                    return Err("The GitHub tree address needs a branch name after /tree/.".into());
                }
                Target::Tree(segments)
            }
            Some(_) => return Err("Use a GitHub repository, branch or folder address, not a file, tag or other page.".into()),
        };
        check_owner(&owner).map_err(|why| pathsafe::printable(&why).into_owned())?;
        check_repo_name(&name).map_err(|why| pathsafe::printable(&why).into_owned())?;
        Ok(Self { owner, name, target })
    }
}

pub(crate) fn check_segment(part: &str) -> Result<(), String> {
    if matches!(part, "" | "." | "..") || part.contains(['/', '\\', '\0']) {
        return Err("The GitHub address contains an empty or unsafe path component.".into());
    }
    Ok(())
}

fn decode(part: &str) -> Result<String, String> {
    fn hex(b: u8) -> Option<u8> {
        char::from(b).to_digit(16).map(|d| d as u8)
    }
    let mut bytes = part.bytes();
    let mut decoded = Vec::with_capacity(part.len());
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let high = bytes.next().and_then(hex);
            let low = bytes.next().and_then(hex);
            let (Some(high), Some(low)) = (high, low) else { return Err("The GitHub address contains an invalid percent escape.".into()) };
            decoded.push((high << 4) | low);
        } else {
            decoded.push(b);
        }
    }
    let decoded = String::from_utf8(decoded).map_err(|_| "The GitHub address contains a name that is not valid UTF-8.")?;
    check_segment(&decoded)?;
    Ok(decoded)
}

/// Resolve a tree URL against verified head names without `refs/heads/`. Matching is exact and case-sensitive.
/// The caller must still check that the resulting folder exists and is a plain directory on the resolved branch.
pub fn resolve_tree(segments: &[String], heads: &[String]) -> Result<(String, String), String> {
    for segment in segments {
        check_segment(segment)?;
    }
    let head = heads
        .iter()
        .filter(|head| {
            let mut parts = segments.iter().map(String::as_str);
            head.split('/').all(|segment| parts.next() == Some(segment))
        })
        .max_by_key(|head| head.len())
        .ok_or("The GitHub address is not a branch in this repository.")?;
    check_branch(head).map_err(|why| format!("The branch name {why}."))?;
    let folder = segments[head.split('/').count()..].join("/");
    let normalized = pathsafe::normalize_repo_dir(&folder).map_err(|why| format!("Invalid GitHub folder: {why}."))?;
    // Don't let a percent-escaped leading or trailing space select a different tree (including the root).
    if normalized != folder {
        return Err("The GitHub folder must not start or end with whitespace.".into());
    }
    Ok((head.clone(), normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(url: &str, heads: &[&str]) -> Result<(String, String), String> {
        let Target::Tree(segments) = Location::parse(url)?.target else { panic!("expected a tree URL") };
        resolve_tree(&segments, &heads.iter().map(|s| (*s).into()).collect::<Vec<_>>())
    }

    #[test]
    fn bare_addresses_follow_the_default_branch() {
        for input in ["Owner/r_e.po", " Owner/r_e.po/ ", "https://github.com/Owner/r_e.po.git/?ignored#ignored", "HTTPS://WWW.GitHub.com/%4Fwner/r_e.po#readme"]
        {
            assert_eq!(Location::parse(input).unwrap(), Location { owner: "Owner".into(), name: "r_e.po".into(), target: Target::Bare });
        }
    }

    #[test]
    fn longest_whole_branch_prefix_wins() {
        let url = "https://github.com/o/r/tree/feature/x/My%20Skills/?foo#bar";
        for heads in [["feature", "feature/x", "feature/x-2"], ["feature/x", "feature/x-2", "feature"]] {
            assert_eq!(resolve(url, &heads).unwrap(), ("feature/x".into(), "My Skills".into()));
        }
        assert_eq!(resolve(url, &["feature", "feature/xy"]).unwrap(), ("feature".into(), "x/My Skills".into()));
        assert_eq!(resolve("https://github.com/o/r/tree/feature/x/", &["feature/x"]).unwrap(), ("feature/x".into(), "".into()));
    }

    #[test]
    fn encoded_components_are_decoded_exactly_once() {
        assert_eq!(
            resolve("https://github.com/o/r/tree/feat%2Bure/Caf%C3%A9%20%252F%23%3F", &["feat+ure"]).unwrap_err(),
            "Invalid GitHub folder: character '?' is not allowed in Windows names."
        );
        assert_eq!(
            resolve("https://github.com/o/r/tree/feat%2Bure/Caf%C3%A9%20%252F%23", &["feat+ure"]).unwrap(),
            ("feat+ure".into(), "Caf\u{e9} %2F#".into())
        );
        for part in ["%2F", "%5c", "%00", ".", "%2e%2E", "", "bad%", "bad%xy", "%FF"] {
            let url = format!("https://github.com/o/r/tree/main/{part}/skills");
            assert!(Location::parse(&url).is_err(), "{url}");
        }
        for path in ["%20", "%20skills", "skills%20", "CON"] {
            assert!(resolve(&format!("https://github.com/o/r/tree/main/{path}"), &["main"]).is_err(), "{path}");
        }
    }

    #[test]
    fn refuses_other_hosts_pages_and_bad_repo_names() {
        for input in [
            "http://github.com/o/r",
            "https://github.com.evil/o/r",
            "https://github.com@evil.test/o/r",
            "https://raw.githubusercontent.com/o/r/main/f",
            "https://github.com/o/r/blob/main/f",
            "https://github.com/o/r/raw/main/f",
            "https://github.com/o/r/commit/abc",
            "https://github.com/o/r/tags",
            "https://github.com/o/r/releases",
            "https://github.com/o/r/pull/42",
            "https://github.com/o/r/tree/",
            "https://github.com/o/r//",
            "https://github.com/o%2fr/r",
            "o/r/tree/main",
            "invalid_owner/repo",
            "o/r$name",
            "https://github.com/o/.git",
        ] {
            assert!(Location::parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn a_tag_commit_or_case_variant_is_not_a_branch() {
        for no_head in ["v1.0.0", "abcdef0123456789abcdef0123456789abcdef01", "Main"] {
            assert!(resolve(&format!("https://github.com/o/r/tree/{no_head}"), &["main", "maint"]).unwrap_err().contains("not a branch"));
        }
        assert!(resolve("https://github.com/o/r/tree/-bad", &["-bad"]).unwrap_err().contains("not a valid branch name"));
    }
}
