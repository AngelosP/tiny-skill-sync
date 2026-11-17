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

    pub fn has_pending(&self) -> bool {
        self.skills.iter().any(|s| s.pending.is_some())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    /// The skills folder the records describe. Files written before it was kept have none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills_dir: Option<String>,
    pub volume: Option<VolumeId>,
    pub repos: BTreeMap<String, RepoState>,
    /// The records of skills folders used before, most recent first, in case one is used again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub set_aside: Vec<SetAside>,
}

impl Default for State {
    fn default() -> Self {
        State { version: STATE_VERSION, skills_dir: None, volume: None, repos: BTreeMap::new(), set_aside: Vec::new() }
    }
}

/// The records of a skills folder the app no longer uses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetAside {
    pub skills_dir: String,
    pub volume: Option<VolumeId>,
    pub repos: BTreeMap<String, RepoState>,
}

/// How many skills folders used before are remembered. The one used longest ago is forgotten first.
pub const MAX_SET_ASIDE: usize = 4;

/// What `State::follow_folder` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Folder {
    /// The records describe this folder.
    Same,
    /// The records named no folder, so they are taken to describe this one, which is now written down.
    Recorded,
    /// The records described another folder, so they were set aside, and this folder's own records (from when it was
    /// used before, `back`) or empty ones took their place. `managed` says whether skills were installed in the old
    /// folder, `unfinished` names the skills whose changes there were interrupted.
    Moved { from: String, managed: bool, unfinished: Vec<String>, back: bool },
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
        self.repos.values().any(RepoState::has_pending)
    }

    /// Whether the records of a folder used before hold a change that was interrupted there.
    pub fn has_pending_set_aside(&self) -> bool {
        self.set_aside.iter().any(|a| a.repos.values().any(RepoState::has_pending))
    }

    /// Ties the records to the skills folder `dir`. Records made for another folder say nothing about this one: they
    /// are set aside as they are, for when that folder is used again, and this folder's own records from when it was
    /// used before take their place, or empty ones, so that its skills are installed afresh.
    pub fn follow_folder(&mut self, dir: &str) -> Folder {
        let Some(from) = self.skills_dir.clone() else {
            self.skills_dir = Some(dir.to_string());
            return Folder::Recorded;
        };
        let key = folder_key(dir);
        let from_key = folder_key(&from);
        if from_key == key {
            return Folder::Same;
        }
        let managed = self.repos.values().any(|r| !r.skills.is_empty());
        let unfinished = self.repos.values().flat_map(|r| &r.skills).filter(|s| s.pending.is_some()).map(|s| s.name.clone()).collect();
        let old = SetAside { skills_dir: from.clone(), volume: self.volume.take(), repos: std::mem::take(&mut self.repos) };
        let own = self.set_aside.iter().position(|a| folder_key(&a.skills_dir) == key).map(|i| self.set_aside.remove(i));
        self.set_aside.retain(|a| folder_key(&a.skills_dir) != from_key);
        if old.repos.values().any(|r| !r.skills.is_empty() || !r.ignored.is_empty()) {
            self.set_aside.insert(0, old);
            self.set_aside.truncate(MAX_SET_ASIDE);
        }
        let back = own.is_some();
        if let Some(own) = own {
            self.volume = own.volume;
            self.repos = own.repos;
        }
        self.skills_dir = Some(dir.to_string());
        Folder::Moved { from, managed, unfinished, back }
    }

    /// Forgets a repository's records, for this folder and for the folders used before. Says whether there were any.
    pub fn drop_repo(&mut self, id: &str) -> bool {
        let mut found = self.repos.remove(id).is_some();
        for a in &mut self.set_aside {
            found |= a.repos.remove(id).is_some();
        }
        self.set_aside.retain(|a| !a.repos.is_empty());
        found
    }

    /// Drops records whose names or transaction ids could point outside the skills or work folder, which only a
    /// damaged or hand-edited file can hold. A dropped record leaves its folder unmanaged, so it is protected,
    /// never deleted. Returns one sentence per dropped record.
    pub fn sanitize(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        let all = self.repos.iter_mut().chain(self.set_aside.iter_mut().flat_map(|a| a.repos.iter_mut()));
        for (repo_id, repo) in all {
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

/// Two spellings of one folder give the same key: `/` or `\`, any case, a trailing separator, `.` and `..`, trailing
/// dots and spaces, a `\\?\` prefix. Spellings it cannot match (a short 8.3 name, a mapped drive, a link) count as
/// another folder, whose records are then set aside: that costs a fresh look, never a file.
fn folder_key(dir: &str) -> String {
    let dir = dir.replace('/', "\\");
    let dir = match dir.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => dir.strip_prefix(r"\\?\").unwrap_or(&dir).to_string(),
    };
    let path = Path::new(&dir);
    // Only for a full path: a relative one would depend on the folder the program happened to start in.
    let full = if path.is_absolute() { std::path::absolute(path).map_or_else(|_| dir.clone(), |p| p.to_string_lossy().into_owned()) } else { dir.clone() };
    pathsafe::fold(full.trim_end_matches('\\'))
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
        let mut st = State {
            skills_dir: Some(r"C:\Users\me\.copilot\skills".to_string()),
            volume: Some(VolumeId { serial: u64::MAX - 1, journal: 133_800_000_000_000_000 }),
            ..State::default()
        };
        let repo = st.repo_mut("r");
        repo.etag = Some("W/\"abc\"".to_string());
        repo.last_check = Some(1_800_000_000);
        repo.ignored.push("mine".to_string());
        repo.upsert_skill(skill("alpha"));
        let mut pending = skill("beta");
        pending.pending = Some(Pending::Update { txn: "t1".into(), commit: "d".repeat(40), files: vec![file("SKILL.md", true)] });
        repo.upsert_skill(pending);
        repo.upsert_skill(OwnedSkill { pending: Some(Pending::Delete { txn: "t2".into() }), ..skill("gamma") });
        let mut old = RepoState::default();
        old.upsert_skill(skill("delta"));
        st.set_aside.push(SetAside {
            skills_dir: r"D:\skills".to_string(),
            volume: Some(VolumeId { serial: 5, journal: 6 }),
            repos: BTreeMap::from([("r".to_string(), old)]),
        });

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

    const HOME_SKILLS: &str = r"C:\Users\me\.copilot\skills";

    #[test]
    fn records_that_name_no_folder_are_taken_to_describe_the_current_one() {
        let mut st = State::default();
        st.repo_mut("r").upsert_skill(skill("alpha"));
        let repos = st.repos.clone();

        assert_eq!(st.follow_folder(HOME_SKILLS), Folder::Recorded);
        assert_eq!(st.skills_dir.as_deref(), Some(HOME_SKILLS));
        assert_eq!(st.repos, repos, "nothing is forgotten");
        assert_eq!(st.follow_folder(HOME_SKILLS), Folder::Same);
    }

    #[test]
    fn the_same_folder_written_another_way_is_the_same_folder() {
        let spellings = [
            HOME_SKILLS,
            r"c:\users\ME\.COPILOT\Skills",
            "C:/Users/me/.copilot/skills",
            r"C:\Users\me\.copilot\skills\",
            "C:/Users/me/.copilot/skills//",
            r"C:\Users\me\.copilot\skills\..\skills",
            r"C:\Users\me\.copilot\.\skills",
            r"\\?\C:\Users\me\.copilot\skills",
            r"C:\Users\me\.copilot\skills.",
            r"C:\Users\me\.copilot\skills ",
        ];
        for other in spellings {
            let mut st = State { skills_dir: Some(HOME_SKILLS.to_string()), ..State::default() };
            st.repo_mut("r").upsert_skill(skill("alpha"));
            let before = st.clone();
            assert_eq!(st.follow_folder(other), Folder::Same, "{other}");
            assert_eq!(st, before, "{other}");
        }
        let mut share = State { skills_dir: Some(r"\\server\share\skills".to_string()), ..State::default() };
        assert_eq!(share.follow_folder(r"\\?\UNC\server\share\skills\"), Folder::Same);
    }

    #[test]
    fn another_folder_sets_the_records_aside_and_hears_what_was_left_behind() {
        let volume = Some(VolumeId { serial: 1, journal: 2 });
        let mut st = State { skills_dir: Some(HOME_SKILLS.to_string()), volume, ..State::default() };
        let repo = st.repo_mut("r");
        repo.etag = Some("W/\"abc\"".to_string());
        repo.last_check = Some(1_800_000_000);
        repo.ignored.push("mine".to_string());
        repo.lists_nothing = true;
        repo.upsert_skill(skill("alpha"));
        repo.upsert_skill(OwnedSkill { pending: Some(Pending::Delete { txn: "t1".into() }), ..skill("beta") });
        let old = st.repos.clone();

        let moved = st.follow_folder(r"D:\skills");

        assert_eq!(moved, Folder::Moved { from: HOME_SKILLS.to_string(), managed: true, unfinished: vec!["beta".to_string()], back: false });
        let aside = vec![SetAside { skills_dir: HOME_SKILLS.to_string(), volume, repos: old }];
        assert_eq!(st, State { skills_dir: Some(r"D:\skills".to_string()), set_aside: aside, ..State::default() });
        assert!(!st.has_pending(), "the interrupted change is never finished in the new folder");
        assert!(st.has_pending_set_aside());

        // A folder whose name starts with the old one is still another folder, and records that hold nothing worth
        // keeping are not set aside.
        let mut quiet = State { skills_dir: Some(HOME_SKILLS.to_string()), ..State::default() };
        quiet.repo_mut("r").last_check = Some(1_800_000_000);
        let moved = quiet.follow_folder(&format!("{HOME_SKILLS}2"));
        assert_eq!(moved, Folder::Moved { from: HOME_SKILLS.to_string(), managed: false, unfinished: vec![], back: false });
        assert!(quiet.repos.is_empty());
        assert!(quiet.set_aside.is_empty());
    }

    #[test]
    fn going_back_to_a_folder_restores_its_records() {
        let mut st = State { skills_dir: Some(HOME_SKILLS.to_string()), volume: Some(VolumeId { serial: 1, journal: 2 }), ..State::default() };
        st.repo_mut("r").upsert_skill(skill("alpha"));
        let home = st.clone();
        st.follow_folder(r"D:\skills");
        st.volume = Some(VolumeId { serial: 3, journal: 4 });
        st.repo_mut("r").ignored.push("mine".to_string());
        let away = st.clone();

        let moved = st.follow_folder(&HOME_SKILLS.to_lowercase());

        assert_eq!(moved, Folder::Moved { from: r"D:\skills".to_string(), managed: false, unfinished: vec![], back: true });
        assert_eq!(st.volume, home.volume);
        assert_eq!(st.repos, home.repos);
        assert_eq!(st.skills_dir.as_deref(), Some(HOME_SKILLS.to_lowercase().as_str()));
        assert_eq!(
            st.set_aside,
            [SetAside { skills_dir: r"D:\skills".to_string(), volume: away.volume, repos: away.repos.clone() }],
            "only the folder just left is set aside"
        );

        assert!(matches!(st.follow_folder(r"d:\SKILLS\"), Folder::Moved { back: true, managed: true, .. }));
        assert_eq!(st.repos, away.repos);
        assert_eq!(st.set_aside.len(), 1);
        assert_eq!(st.set_aside[0].repos, home.repos);
    }

    #[test]
    fn only_a_few_folders_used_before_are_remembered() {
        let mut st = State { skills_dir: Some(r"C:\s0".to_string()), ..State::default() };
        for i in 1..=MAX_SET_ASIDE + 2 {
            st.repo_mut("r").upsert_skill(skill(&format!("skill{}", i - 1)));
            st.follow_folder(&format!(r"C:\s{i}"));
        }
        let kept: Vec<&str> = st.set_aside.iter().map(|a| a.skills_dir.as_str()).collect();
        assert_eq!(kept, [r"C:\s5", r"C:\s4", r"C:\s3", r"C:\s2"], "most recent first, the oldest forgotten");
        assert_eq!(st.set_aside[0].repos["r"].skills[0].name, "skill5");
    }

    #[test]
    fn set_aside_records_are_checked_and_forgotten_with_their_repository() {
        let mut st = State::default();
        st.repo_mut("r").upsert_skill(skill("fine"));
        st.repo_mut("other").upsert_skill(skill("kept"));
        let mut damaged = RepoState::default();
        damaged.upsert_skill(skill("..\\evil"));
        damaged.upsert_skill(skill("fine"));
        st.set_aside.push(SetAside { skills_dir: r"D:\skills".to_string(), volume: None, repos: BTreeMap::from([("r".to_string(), damaged)]) });

        let notes = st.sanitize();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(st.set_aside[0].repos["r"].skills.len(), 1);

        assert!(st.drop_repo("r"));
        assert!(!st.repos.contains_key("r"));
        assert!(st.set_aside.is_empty(), "records left with nothing in them are not kept");
        assert!(st.repos.contains_key("other"));
        assert!(!st.drop_repo("r"));
    }
}
