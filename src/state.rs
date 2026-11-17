//! The ownership manifest (`state.json`): which skills this app installed, what their files were,
//! and what the file system reported for each file at that moment.
//!
//! Only skills listed here are ever updated or deleted. Anything else in the skills folder is the user's.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::fsx;
use crate::pathsafe;
use crate::tree::SkillFile;

pub const STATE_VERSION: u32 = 1;

/// What Windows reported about a file when its content was last known to match the manifest.
/// Stored as `[file id (2 numbers), size, last write, change time, USN]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub file_id: [u64; 2],
    pub size: u64,
    pub write: u64,
    pub change: u64,
    /// The file's USN journal sequence number, or 0 when the volume has no journal.
    pub usn: u64,
}

impl Serialize for Fingerprint {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        [self.file_id[0], self.file_id[1], self.size, self.write, self.change, self.usn].serialize(s)
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let [a, b, size, write, change, usn] = <[u64; 6]>::deserialize(d)?;
        Ok(Fingerprint { file_id: [a, b], size, write, change, usn })
    }
}

/// Identifies the volume and its change journal. If either differs from what was stored, no stored USN can be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeId {
    pub serial: u64,
    /// 0 when the volume has no change journal.
    pub journal: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedFile {
    pub path: String,
    /// Git blob SHA-1 of the content.
    pub blob: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fp: Option<Fingerprint>,
}

impl From<&SkillFile> for ManagedFile {
    fn from(f: &SkillFile) -> Self {
        ManagedFile { path: f.path.clone(), blob: f.blob.clone(), size: f.size, fp: None }
    }
}

/// Written before a skill folder is swapped or deleted, so a crash in the middle can be understood afterwards.
/// `txn` names the transaction's folders in the work directory (`stage-<txn>`, `old-<txn>`, `trash-<txn>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Pending {
    Update { txn: String, commit: String, files: Vec<ManagedFile> },
    Delete { txn: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedSkill {
    pub name: String,
    pub commit: String,
    pub files: Vec<ManagedFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoState {
    /// The ETag of the last tree listing that was fully applied. Empty while work is held, so the next poll fetches again.
    pub etag: Option<String>,
    pub last_check: Option<i64>,
    /// Skills the user chose to keep as their own; the app leaves same-named upstream skills alone.
    pub ignored: Vec<String>,
    pub skills: Vec<OwnedSkill>,
    /// The last listing that was read had no skills in it at all, not even ones refused as unsafe.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub lists_nothing: bool,
}

impl RepoState {
    pub fn skill(&self, name: &str) -> Option<&OwnedSkill> {
        let key = pathsafe::fold(name);
        self.skills.iter().find(|s| pathsafe::fold(&s.name) == key)
    }

    pub fn skill_mut(&mut self, name: &str) -> Option<&mut OwnedSkill> {
        let key = pathsafe::fold(name);
        self.skills.iter_mut().find(|s| pathsafe::fold(&s.name) == key)
    }

    pub fn is_ignored(&self, name: &str) -> bool {
        let key = pathsafe::fold(name);
        self.ignored.iter().any(|n| pathsafe::fold(n) == key)
    }

    pub fn remove_skill(&mut self, name: &str) {
        let key = pathsafe::fold(name);
        self.skills.retain(|s| pathsafe::fold(&s.name) != key);
    }

    /// Replaces the entry with the same name, or adds it, keeping the list sorted.
    pub fn upsert_skill(&mut self, skill: OwnedSkill) {
        self.remove_skill(&skill.name);
        self.skills.push(skill);
        self.skills.sort_by_cached_key(|s| pathsafe::fold(&s.name));
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    pub volume: Option<VolumeId>,
    pub repos: BTreeMap<String, RepoState>,
}

impl Default for State {
    fn default() -> Self {
        State { version: STATE_VERSION, volume: None, repos: BTreeMap::new() }
    }
}

pub struct Loaded {
    pub state: State,
    /// Set when the file was unreadable and a fresh state was started instead.
    pub warning: Option<String>,
}

impl State {
    /// Reads the file. A damaged one is moved aside (kept for inspection) and an empty state is returned.
    pub fn load(path: &Path) -> io::Result<Loaded> {
        State::load_with(path, true)
    }

    /// With `quarantine` false nothing on disk changes: a damaged file is reported and left in place. Used by
    /// read-only sessions, which must not write anything.
    pub fn load_with(path: &Path, quarantine: bool) -> io::Result<Loaded> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Loaded { state: State::default(), warning: None }),
            Err(e) => return Err(e),
        };
        match serde_json::from_slice::<State>(&bytes) {
            Ok(state) if state.version > STATE_VERSION => Err(io::Error::other(format!(
                "state.json was written by a newer version of the app (format {}, this app understands {STATE_VERSION}); not touching it",
                state.version
            ))),
            Ok(state) => Ok(Loaded { state, warning: None }),
            // Starting empty is safe: installed skills then look unmanaged and are protected, never deleted.
            Err(e) if !quarantine => Ok(Loaded {
                state: State::default(),
                warning: Some(format!("state.json is damaged ({e}); it was left in place and will be kept aside the next time the app writes")),
            }),
            Err(e) => {
                let keep = path.with_file_name(format!("state.json.damaged-{}", fsx::unique_suffix()));
                std::fs::rename(path, &keep)?;
                Ok(Loaded {
                    state: State::default(),
                    warning: Some(format!(
                        "state.json could not be read ({e}); it was kept as {} and the app started with an empty list of managed skills",
                        keep.display()
                    )),
                })
            }
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        fsx::write_atomic(path, &json)
    }

    pub fn repo(&self, id: &str) -> Option<&RepoState> {
        self.repos.get(id)
    }

    pub fn repo_mut(&mut self, id: &str) -> &mut RepoState {
        self.repos.entry(id.to_string()).or_default()
    }

    /// Case-folded skill names owned by repositories other than `repo_id`, with the owning repository.
    pub fn names_owned_elsewhere(&self, repo_id: &str) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (id, repo) in &self.repos {
            if id == repo_id {
                continue;
            }
            for s in &repo.skills {
                out.entry(pathsafe::fold(&s.name)).or_insert_with(|| id.clone());
            }
        }
        out
    }

    pub fn has_pending(&self) -> bool {
        self.repos.values().any(|r| r.skills.iter().any(|s| s.pending.is_some()))
    }

    /// Drops records whose names or transaction ids could point outside the skills or work folder, which only a
    /// damaged or hand-edited file can hold. A dropped record leaves its folder unmanaged, so it is protected,
    /// never deleted. Returns one sentence per dropped record.
    pub fn sanitize(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        for (repo_id, repo) in &mut self.repos {
            repo.skills.retain(|s| match check_record(s) {
                Ok(()) => true,
                Err(why) => {
                    notes.push(format!("ignored a damaged record of '{}' in {repo_id} ({why}); the folder, if there is one, is left alone", s.name));
                    false
                }
            });
        }
        notes
    }
}

fn check_record(skill: &OwnedSkill) -> Result<(), String> {
    pathsafe::check_component(&skill.name).map_err(|e| format!("name: {e}"))?;
    let pending_files = match &skill.pending {
        Some(Pending::Update { txn, files, .. }) => {
            check_txn(txn)?;
            files.as_slice()
        }
        Some(Pending::Delete { txn }) => {
            check_txn(txn)?;
            &[]
        }
        None => &[],
    };
    for file in skill.files.iter().chain(pending_files) {
        pathsafe::check_rel_path(&file.path).map_err(|e| format!("file '{}': {e}", file.path))?;
    }
    Ok(())
}

/// Transaction ids name folders in the work folder, so they must be plain letters and digits.
fn check_txn(txn: &str) -> Result<(), String> {
    if txn.is_empty() || txn.len() > 64 || !txn.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("transaction id".to_string());
    }
    Ok(())
}

pub fn state_path_in(home: &Path) -> PathBuf {
    home.join("state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, with_fp: bool) -> ManagedFile {
        ManagedFile {
            path: path.to_string(),
            blob: crate::hash::blob_hex(path.as_bytes()),
            size: 3,
            fp: with_fp.then_some(Fingerprint { file_id: [u64::MAX, 7], size: 3, write: 133_800_000_000_000_000, change: 133_800_000_000_000_001, usn: 4_096 }),
        }
    }

    fn skill(name: &str) -> OwnedSkill {
        OwnedSkill { name: name.to_string(), commit: "c".repeat(40), files: vec![file("SKILL.md", true), file("a/b.md", false)], pending: None }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tss-state-{name}-{}", fsx::unique_suffix()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn state_round_trips_with_large_numbers_intact() {
        let mut st = State { volume: Some(VolumeId { serial: u64::MAX - 1, journal: 133_800_000_000_000_000 }), ..State::default() };
        let repo = st.repo_mut("r");
        repo.etag = Some("W/\"abc\"".to_string());
        repo.last_check = Some(1_800_000_000);
        repo.ignored.push("mine".to_string());
        repo.upsert_skill(skill("alpha"));
        let mut pending = skill("beta");
        pending.pending = Some(Pending::Update { txn: "t1".into(), commit: "d".repeat(40), files: vec![file("SKILL.md", true)] });
        repo.upsert_skill(pending);
        repo.upsert_skill(OwnedSkill { pending: Some(Pending::Delete { txn: "t2".into() }), ..skill("gamma") });

        let json = serde_json::to_string(&st).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(back, st);
        assert_eq!(back.repos["r"].skills[0].files[0].fp.unwrap().file_id[0], u64::MAX);
        assert!(back.has_pending());
    }

    #[test]
    fn missing_file_gives_an_empty_state() {
        let dir = temp("missing");
        let loaded = State::load(&dir.join("state.json")).unwrap();
        assert_eq!(loaded.state, State::default());
        assert!(loaded.warning.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_then_load() {
        let dir = temp("save");
        let path = dir.join("state.json");
        let mut st = State::default();
        st.repo_mut("r").upsert_skill(skill("alpha"));
        st.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap().state, st);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_file_is_kept_and_a_fresh_state_starts() {
        let dir = temp("damaged");
        let path = dir.join("state.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded.state, State::default());
        assert!(loaded.warning.as_deref().unwrap().contains("damaged"));
        assert!(!path.exists());
        let kept: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].starts_with("state.json.damaged-"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn damaged_records_that_could_point_outside_the_folders_are_dropped() {
        let mut st = State::default();
        let repo = st.repo_mut("r");
        repo.upsert_skill(skill("fine"));
        repo.upsert_skill(skill("..\\..\\evil"));
        repo.upsert_skill(skill(".."));
        repo.upsert_skill(OwnedSkill { files: vec![file("../outside.md", false)], ..skill("bad-file") });
        repo.upsert_skill(OwnedSkill { pending: Some(Pending::Delete { txn: "..\\..\\x".into() }), ..skill("bad-txn") });
        repo.upsert_skill(OwnedSkill {
            pending: Some(Pending::Update { txn: "ab12".into(), commit: "c".repeat(40), files: vec![file("a/../../b", false)] }),
            ..skill("bad-pending-file")
        });
        repo.upsert_skill(OwnedSkill { pending: Some(Pending::Delete { txn: "ab12".into() }), ..skill("fine-pending") });

        let notes = st.sanitize();
        let kept: Vec<&str> = st.repos["r"].skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(kept, ["fine", "fine-pending"]);
        assert_eq!(notes.len(), 5, "{notes:?}");
        assert!(notes.iter().all(|n| n.contains("left alone")));
        assert!(State::default().sanitize().is_empty());
    }

    #[test]
    fn a_read_only_load_leaves_a_damaged_file_where_it_is() {
        let dir = temp("damaged-ro");
        let path = dir.join("state.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let loaded = State::load_with(&path, false).unwrap();
        assert_eq!(loaded.state, State::default());
        assert!(loaded.warning.as_deref().unwrap().contains("damaged"));
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json", "the file is untouched");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "nothing was created either");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_newer_format_is_refused_untouched() {
        let dir = temp("newer");
        let path = dir.join("state.json");
        std::fs::write(&path, br#"{"version": 99}"#).unwrap();
        assert!(State::load(&path).is_err());
        assert!(path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_are_matched_without_regard_to_case() {
        let mut repo = RepoState::default();
        repo.upsert_skill(skill("Alpha"));
        assert!(repo.skill("alpha").is_some());
        repo.upsert_skill(OwnedSkill { commit: "new".into(), ..skill("ALPHA") });
        assert_eq!(repo.skills.len(), 1);
        assert_eq!(repo.skill("alpha").unwrap().commit, "new");
        repo.ignored.push("Mine".into());
        assert!(repo.is_ignored("mine"));
        repo.remove_skill("alpha");
        assert!(repo.skills.is_empty());
    }

    #[test]
    fn owned_elsewhere_excludes_the_asking_repository() {
        let mut st = State::default();
        st.repo_mut("a").upsert_skill(skill("shared"));
        st.repo_mut("b").upsert_skill(skill("Other"));
        let seen_by_a = st.names_owned_elsewhere("a");
        assert_eq!(seen_by_a.get("other").map(String::as_str), Some("b"));
        assert!(!seen_by_a.contains_key("shared"));
    }
}
