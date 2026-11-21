//! What each source's last complete listing held, kept for display only. The file is local to this PC (never
//! portable), written by the engine only when it read a complete listing whose summary differs from the saved one, and
//! read when the app starts and by `tss status`. A missing, unreadable or newer file only means no details are shown
//! yet; nothing in it is ever used to install, keep or remove anything.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::inventory::Summary;

pub const SEEN_FILE: &str = "seen.json";
const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Seen {
    /// When the listing was read, in Unix seconds.
    pub at: i64,
    /// The branch it was read from; for a source that follows the default branch, that branch's name then.
    pub branch: String,
    pub summary: Summary,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Wire {
    version: u32,
    sources: BTreeMap<String, Seen>,
}

/// Every source's entry, by source ID. A missing, unreadable, malformed or newer file reads as empty.
pub fn load(path: &Path) -> BTreeMap<String, Seen> {
    std::fs::read(path).ok().map(|bytes| parse(&bytes)).unwrap_or_default()
}

fn parse(bytes: &[u8]) -> BTreeMap<String, Seen> {
    serde_json::from_slice::<Wire>(bytes).ok().filter(|wire| wire.version == VERSION).map(|wire| wire.sources).unwrap_or_default()
}

/// Replaces the file so a reader sees the old or the new content, never a partial file.
pub fn save(path: &Path, sources: &BTreeMap<String, Seen>) -> io::Result<()> {
    let wire = Wire { version: VERSION, sources: sources.clone() };
    let json = serde_json::to_vec_pretty(&wire).map_err(io::Error::other)?;
    crate::fsx::write_atomic(path, &json)
}

/// Reads the file as it is now, lets `change` edit it, and saves it only when that changed something. Another copy of
/// the app may have saved since this one last read it, so the merge starts from the file, never from an older copy.
/// A file from a newer version of the app is never written over: `Ok(None)`. A missing, unreadable or malformed file
/// starts empty. Returns what the file holds afterwards.
pub fn update(path: &Path, change: impl FnOnce(&mut BTreeMap<String, Seen>)) -> io::Result<Option<BTreeMap<String, Seen>>> {
    #[derive(Deserialize)]
    struct Version {
        #[serde(default)]
        version: u32,
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if bytes.as_deref().and_then(|b| serde_json::from_slice::<Version>(b).ok()).is_some_and(|v| v.version > VERSION) {
        return Ok(None);
    }
    let held = bytes.as_deref().map(parse).unwrap_or_default();
    let mut next = held.clone();
    change(&mut next);
    if next != held {
        save(path, &next)?;
    }
    Ok(Some(next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::SummaryNote;
    use crate::source::SourceKind;

    #[test]
    fn a_saved_summary_reads_back_and_a_bad_or_newer_file_reads_as_empty() {
        let base = std::env::var_os("TSS_TEST_ROOT").filter(|s| !s.is_empty()).map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("tss-seen-{}", crate::fsx::unique_suffix()));
        let file = dir.join(SEEN_FILE);
        assert!(load(&file).is_empty());
        let note = SummaryNote { kind: Some(SourceKind::Plugins), path: "tools/x".into(), reason: "why".into() };
        let seen = Seen { at: 7, branch: "main".into(), summary: Summary { found: [1, 0, 0, 1], notes: vec![note], ..Summary::default() } };
        let all = BTreeMap::from([("src".to_string(), seen)]);
        save(&file, &all).unwrap();
        assert_eq!(load(&file), all);
        std::fs::write(&file, b"{not json").unwrap();
        assert!(load(&file).is_empty());
        std::fs::write(&file, br#"{"version":2,"sources":{"src":{"at":1}}}"#).unwrap();
        assert!(load(&file).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
