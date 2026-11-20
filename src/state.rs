//! The ownership manifest (`state.json`): which skills this app installed, what their files were,
//! and what the file system reported for each file at that moment.
//!
//! Only skills listed here are ever updated or deleted. Anything else in the skills folder is the user's.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::{RepoConfig, legacy_version};
use crate::fsx;
use crate::pathsafe;
use crate::source::{DestinationBinding, SourceKind};
use crate::tree::SkillFile;

pub const STATE_VERSION: u32 = 2;

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
    /// What the file is compared by ([`crate::text::canon`]): a hash of the lines that count, or `""` when only the
    /// exact bytes will do. `None` until the app has read the file once, which records written by older versions lack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canon: Option<String>,
}

impl From<&SkillFile> for ManagedFile {
    fn from(f: &SkillFile) -> Self {
        ManagedFile { path: f.path.clone(), blob: f.blob.clone(), size: f.size, fp: None, canon: None }
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
    /// When the app last installed, updated or adopted this skill. Records written by older versions have none until
    /// the skill's next update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
#[allow(clippy::manual_non_exhaustive)]
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
    /// GitHub showed the repository only to the user's GitHub sign-in: it is private, so the token is used from the
    /// start. Learned by the app; the user never sets it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub needs_token: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub searches: BTreeMap<String, HistorySearch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_retry_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_head: Option<String>,
    // This namespace, including `set_aside`, is permanently skills-only. Never extend Pending with loose-file operations.
    #[serde(skip_serializing, deserialize_with = "skills_marker")]
    kind: (),
}

fn skills_marker<'de, D: Deserializer<'de>>(d: D) -> Result<(), D::Error> {
    match SourceKind::deserialize(d)? {
        SourceKind::Skills => Ok(()),
        _ => Err(serde::de::Error::custom("loose-file records do not belong in skills state")),
    }
}

/// Progress, not authority to replace anything. Adoption always reads the complete matched version again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySearch {
    pub name: String,
    pub source: String,
    pub tree: String,
    pub pin: String,
    pub page: u32,
    pub commits: Vec<String>,
    pub offset: usize,
    pub checked: u64,
    pub complete: bool,
    pub paused: bool,
    pub volume: Option<VolumeId>,
    pub evidence: BTreeMap<String, LocalEvidence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalEvidence {
    Missing,
    File(Fingerprint),
    Content { fp: Fingerprint, blob: String },
    Other,
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
    version: u32,
    /// The skills folder the records describe. Files written before it was kept have none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills_dir: Option<String>,
    pub volume: Option<VolumeId>,
    pub repos: BTreeMap<String, RepoState>,
    /// The records of skills folders used before, most recent first, in case one is used again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub set_aside: Vec<SetAside>,
    // Outside BOTH v1 repo maps: that reader decodes all Pending values *before* checking the version.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "read_loose_sources")]
    loose_sources: BTreeMap<String, LooseSourceState>,
}

impl Default for State {
    fn default() -> Self {
        State { version: legacy_version(), skills_dir: None, volume: None, repos: BTreeMap::new(), set_aside: Vec::new(), loose_sources: BTreeMap::new() }
    }
}

/// A leaf journal never contains a path to move to or delete. The binding selects the directory,
/// the record selects ONE component, and the transaction ID selects two private work names.
pub type DiskId = [u8; 24]; // volume serial + 128-bit file ID

pub(crate) fn check_leaf_name(kind: SourceKind, name: &str) -> Result<(), String> {
    pathsafe::check_component(name).map_err(|e| e.to_string())?;
    let suffix = match kind {
        SourceKind::Agents => ".agent.md",
        SourceKind::Instructions => ".instructions.md",
        SourceKind::Skills => return Err("skills cannot own loose files".into()),
    };
    if !name.ends_with(suffix) || name.len() == suffix.len() {
        return Err(format!("expected a file named *{suffix}"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeafOrigin {
    repo: String,
    branch: String,
    path: String,
}

impl From<&RepoConfig> for LeafOrigin {
    fn from(s: &RepoConfig) -> Self {
        Self { repo: s.repo.clone(), branch: s.branch.clone(), path: s.path.clone() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LeafPoll {
    pub etag: Option<String>,
    pub last_check: Option<i64>,
    pub lists_nothing: bool,
    pub needs_token: bool,
    pub source: String,
    pub ignored: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeafVersion {
    pub file: ManagedFile,
    pub commit: String,
    pub updated: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LeafStamp {
    pub id: DiskId,
    /// Actual bytes, including any text-equivalent differences in the old copy.
    pub blob: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LeafTxn {
    pub txn: String,
    pub work: DiskId,
    pub before: Option<LeafStamp>,
    pub after: Option<(DiskId, LeafVersion)>,
    /// Explicit replace retains the original object, even if it wasn't owned.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_backup: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedLeaf {
    pub name: String,
    pub installed: Option<LeafVersion>,
    pub(crate) pending: Option<LeafTxn>,
}

impl OwnedLeaf {
    pub fn interrupted(&self) -> bool {
        self.pending.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LooseSourceState {
    binding: DestinationBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<LeafOrigin>,
    #[serde(default)]
    pub poll: LeafPoll,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) directory_id: Option<DiskId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) leaves: Vec<OwnedLeaf>,
}

impl LooseSourceState {
    /// Reconstruct the recorded scope (not the user's current settings) for recovery, checking a
    /// listing, or explicitly unmanaging an orphan. Binding-only format-2 records may predate this field.
    pub fn recorded_source(&self, id: &str) -> Option<RepoConfig> {
        let s = self.origin.as_ref()?;
        Some(RepoConfig {
            id: id.into(),
            kind: self.binding.kind(),
            repo: s.repo.clone(),
            branch: s.branch.clone(),
            path: s.path.clone(),
            ..Default::default()
        })
    }

    pub fn binding(&self) -> &DestinationBinding {
        &self.binding
    }

    pub fn leaves(&self) -> &[OwnedLeaf] {
        &self.leaves
    }

    pub fn leaf(&self, name: &str) -> Option<&OwnedLeaf> {
        self.leaves.iter().find(|r| pathsafe::fold(&r.name) == pathsafe::fold(name))
    }

    pub(crate) fn set_leaf(&mut self, name: &str, record: Option<OwnedLeaf>) {
        self.leaves.retain(|r| pathsafe::fold(&r.name) != pathsafe::fold(name));
        self.leaves.extend(record);
        self.poll.etag = None;
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        use crate::hash::is_sha1_hex;
        self.binding.validate()?;
        let valid_id = |id: &DiskId| id[8..].iter().any(|b| *b != 0);
        if self.directory_id.as_ref().is_some_and(|id| !valid_id(id)) || (!self.leaves.is_empty() && (self.directory_id.is_none() || self.origin.is_none())) {
            return Err("missing or invalid loose destination identity".into());
        }
        let mut names = std::collections::HashSet::new();
        for r in &self.leaves {
            check_leaf_name(self.binding.kind(), &r.name)?;
            if !names.insert(pathsafe::fold(&r.name)) || (r.installed.is_none() && r.pending.is_none()) {
                return Err("duplicate or empty loose file record".into());
            }
            for v in r.installed.iter().chain(r.pending.iter().filter_map(|p| p.after.as_ref().map(|(_, v)| v))) {
                if v.file.path != r.name
                    || !is_sha1_hex(&v.commit)
                    || !is_sha1_hex(&v.file.blob)
                    || v.file.size > crate::tree::MAX_FILE_BYTES
                    || !v.file.canon.as_deref().is_some_and(|c| c.is_empty() || is_sha1_hex(c))
                {
                    return Err("invalid loose file evidence".into());
                }
            }
            if let Some(p) = &r.pending {
                check_txn(&p.txn)?;
                if !valid_id(&p.work)
                    || (!p.keep_backup && p.before.is_some() != r.installed.is_some())
                    || (p.keep_backup && p.after.is_none())
                    || (p.before.is_none() && p.after.is_none())
                    || p.before.as_ref().is_some_and(|b| !valid_id(&b.id) || !is_sha1_hex(&b.blob))
                    || p.after.as_ref().is_some_and(|(id, _)| !valid_id(id) || p.before.as_ref().is_some_and(|b| b.id == *id))
                {
                    return Err("invalid loose transaction evidence".into());
                }
            }
        }
        Ok(())
    }
}

// A duplicate map key must not silently replace an immutable binding with the last one read.
fn read_loose_sources<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, LooseSourceState>, D::Error> {
    struct Sources;
    impl<'de> serde::de::Visitor<'de> for Sources {
        type Value = BTreeMap<String, LooseSourceState>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("loose source records keyed by unique ID")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut records = BTreeMap::new();
            while let Some((id, record)) = map.next_entry()? {
                if records.insert(id, record).is_some() {
                    return Err(serde::de::Error::custom("two loose sources share an ID"));
                }
            }
            Ok(records)
        }
    }
    d.deserialize_map(Sources)
}

#[derive(Deserialize)]
struct StateFormat {
    #[serde(default = "legacy_version")]
    version: u32,
    #[serde(default, deserialize_with = "present")]
    loose_sources: bool,
}

fn present<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(d).map(|_| true)
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
        let loaded = Self::load_sources_with(path, quarantine)?;
        if !loaded.state.loose_sources.is_empty() {
            return Err(io::Error::other("state.json contains loose-file sources; the skills engine cannot use it"));
        }
        Ok(loaded)
    }

    /// Typed data access. The skills engine uses `load_with`, which refuses an entire typed record,
    /// including when settings disappeared or a legacy caller supplies only an ID to remove/recover.
    pub fn load_sources_with(path: &Path, quarantine: bool) -> io::Result<Loaded> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Loaded { state: State::default(), warning: None }),
            Err(e) => return Err(e),
        };
        let decoded = match serde_json::from_slice::<StateFormat>(&bytes) {
            Ok(format) => {
                if format.version > STATE_VERSION {
                    return Err(io::Error::other(format!(
                        "state.json was written by a newer version of the app (format {}, this app understands {STATE_VERSION}); not touching it",
                        format.version
                    )));
                }
                if format.loose_sources && format.version < 2 {
                    return Err(io::Error::other("loose-file state requires state.json format 2"));
                }
                match serde_json::from_slice::<State>(&bytes) {
                    Ok(mut state) => {
                        state.check_bindings().map_err(io::Error::other)?;
                        state.version = state.wire_version();
                        Ok(state)
                    }
                    Err(e) if format.version >= 2 || (e.is_data() && Self::tagged_skill_records(&bytes)) => return Err(io::Error::other(e)),
                    other => other,
                }
            }
            Err(e) if e.is_data() => return Err(io::Error::other(e)),
            Err(e) => Err(e),
        };
        match decoded {
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
        self.check_bindings().map_err(io::Error::other)?;
        if self.version != self.wire_version() {
            return Err(io::Error::other("state format does not match the recorded sources"));
        }
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        fsx::write_atomic(path, &json)
    }

    fn wire_version(&self) -> u32 {
        if self.loose_sources.is_empty() { 1 } else { 2 }
    }

    fn has_skills_record(&self, id: &str) -> bool {
        self.repos.contains_key(id) || self.set_aside.iter().any(|a| a.repos.contains_key(id))
    }

    fn check_bindings(&self) -> Result<(), String> {
        for (id, record) in &self.loose_sources {
            if id.trim().is_empty() || self.has_skills_record(id) {
                return Err(format!("invalid or conflicting loose source ID '{id}' in state.json"));
            }
            record.validate()?;
        }
        Ok(())
    }

    /// An ID cannot borrow ownership, pending work, held edits or schedule from another kind.
    pub fn binding_for(&self, source: &RepoConfig) -> Result<Option<&DestinationBinding>, String> {
        if source.id.trim().is_empty() {
            return Err("a destination binding requires a stable source ID".into());
        }
        if !source.kind.is_skills() && self.has_skills_record(&source.id) {
            return Err(format!("source '{}' already has a skills record", source.id));
        }
        let Some(record) = self.loose_sources.get(&source.id) else { return Ok(None) };
        record.binding.check_source(source)?;
        if record.origin.as_ref().is_some_and(|o| o.repo != source.repo || o.branch != source.branch || o.path != source.path) {
            return Err(format!("source '{}' no longer describes the recorded repository, branch and folder", source.id));
        }
        Ok(Some(&record.binding))
    }

    /// Record the first resolved destination; subsequent checks may confirm it, never relocate it.
    pub fn bind_destination(&mut self, source: &RepoConfig, binding: DestinationBinding) -> Result<(), String> {
        binding.check_source(source)?;
        if let Some(old) = self.binding_for(source)? {
            return if *old == binding { Ok(()) } else { Err(format!("source '{}' already has a different destination", source.id)) };
        }
        self.loose_sources.insert(
            source.id.clone(),
            LooseSourceState { binding, origin: Some(source.into()), poll: LeafPoll::default(), directory_id: None, leaves: Vec::new() },
        );
        self.version = self.wire_version();
        Ok(())
    }

    /// Includes disabled sources and orphans. Settings are not an ownership index.
    pub fn loose_sources(&self) -> &BTreeMap<String, LooseSourceState> {
        &self.loose_sources
    }

    pub fn leaf_poll_mut(&mut self, source: &RepoConfig) -> Result<&mut LeafPoll, String> {
        Ok(&mut self.bound_loose_mut(source)?.poll)
    }

    pub(crate) fn last_check_mut(&mut self, id: &str) -> Option<&mut Option<i64>> {
        if let Some(r) = self.loose_sources.get_mut(id) { Some(&mut r.poll.last_check) } else { self.repos.get_mut(id).map(|r| &mut r.last_check) }
    }

    pub(crate) fn bound_loose_mut(&mut self, source: &RepoConfig) -> Result<&mut LooseSourceState, String> {
        self.binding_for(source)?;
        let record = self.loose_sources.get_mut(&source.id).ok_or("the loose destination is not bound")?;
        record.origin.get_or_insert_with(|| source.into());
        Ok(record)
    }

    /// Explicit unmanage only. Never release a journal's claim, even for an orphan.
    pub fn forget_loose_source(&mut self, source: &RepoConfig) -> Result<(), String> {
        if self.bound_loose_mut(source)?.leaves.iter().any(OwnedLeaf::interrupted) {
            return Err("finish the interrupted file change before forgetting this source".into());
        }
        self.loose_sources.remove(&source.id);
        self.version = self.wire_version();
        Ok(())
    }

    pub fn repo(&self, id: &str) -> Option<&RepoState> {
        self.repos.get(id)
    }

    /// Only on a failed v1 payload decode. A misplaced kind marker is not a damaged ownership
    /// record to quarantine; even another bad field earlier in the document must not hide it.
    fn tagged_skill_records(bytes: &[u8]) -> bool {
        use serde_json::Value;
        fn tagged(group: &Value) -> bool {
            group.get("repos").and_then(Value::as_object).is_some_and(|repos| repos.values().any(|r| r.get("kind").is_some()))
        }
        serde_json::from_slice::<Value>(bytes).is_ok_and(|v| tagged(&v) || v.get("set_aside").and_then(Value::as_array).is_some_and(|a| a.iter().any(tagged)))
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
        if old.repos.values().any(|r| !r.skills.is_empty() || !r.ignored.is_empty() || !r.searches.is_empty()) {
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
    /// A change that is not finished keeps its record, so that recovery can still finish or undo it (an unfinished
    /// change can hold files the user added in the work folder); the repository is then shown as one no longer in the
    /// settings until it is done.
    pub fn drop_repo(&mut self, id: &str) -> bool {
        fn forget(repos: &mut BTreeMap<String, RepoState>, id: &str) -> bool {
            let Some(repo) = repos.get_mut(id) else { return false };
            let unfinished: Vec<OwnedSkill> = repo.skills.iter().filter(|s| s.pending.is_some()).cloned().collect();
            if unfinished.is_empty() {
                repos.remove(id);
                return true;
            }
            let kept = RepoState { skills: unfinished, ..RepoState::default() };
            let changed = *repo != kept;
            *repo = kept;
            changed
        }
        let mut found = forget(&mut self.repos, id);
        for a in &mut self.set_aside {
            found |= forget(&mut a.repos, id);
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
            repo.searches.retain(|key, h| {
                pathsafe::check_component(&h.name).is_ok()
                    && *key == pathsafe::fold(&h.name)
                    && h.page > 0
                    && h.offset <= h.commits.len()
                    && h.commits.len() <= 100
                    && h.evidence.keys().all(|p| pathsafe::check_rel_path(p).is_ok())
                    && std::iter::once(&h.pin).chain(&h.commits).all(|c| c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit()))
            });
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
            canon: with_fp.then(|| "d".repeat(40)),
        }
    }

    fn skill(name: &str) -> OwnedSkill {
        OwnedSkill { name: name.to_string(), commit: "c".repeat(40), files: vec![file("SKILL.md", true), file("a/b.md", false)], updated: None, pending: None }
    }

    fn temp(name: &str) -> PathBuf {
        let base = std::env::var_os("TSS_TEST_ROOT").filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("tss-state-{name}-{}", fsx::unique_suffix()));
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
    fn the_time_of_a_skills_last_change_round_trips_and_older_records_have_none() {
        let older: OwnedSkill = serde_json::from_str(r#"{"name":"alpha","commit":"c","files":[]}"#).unwrap();
        assert_eq!(older.updated, None);
        assert!(!serde_json::to_string(&older).unwrap().contains("updated"), "nothing is written until it is known");
        let known = OwnedSkill { updated: Some(1_800_000_000), ..skill("alpha") };
        assert_eq!(serde_json::from_str::<OwnedSkill>(&serde_json::to_string(&known).unwrap()).unwrap(), known);
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

    // Frozen v1: the payload must decode, including every pending operation, to reach the version check.
    #[allow(dead_code)]
    mod v1_reader {
        use super::{BTreeMap, Deserialize, HistorySearch, Path, VolumeId};

        #[derive(Deserialize)]
        struct Record {
            #[serde(default = "one")]
            version: u32,
            skills_dir: Option<String>,
            volume: Option<VolumeId>,
            #[serde(default)]
            repos: BTreeMap<String, Repo>,
            #[serde(default)]
            set_aside: Vec<Aside>,
        }
        fn one() -> u32 {
            1
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Aside {
            skills_dir: String,
            volume: Option<VolumeId>,
            repos: BTreeMap<String, Repo>,
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Repo {
            etag: Option<String>,
            last_check: Option<i64>,
            ignored: Vec<String>,
            skills: Vec<Skill>,
            lists_nothing: bool,
            needs_token: bool,
            source: String,
            searches: BTreeMap<String, HistorySearch>,
            history_retry_at: Option<i64>,
            history_head: Option<String>,
        }
        #[derive(Deserialize)]
        struct Skill {
            name: String,
            commit: String,
            files: Vec<File>,
            updated: Option<i64>,
            pending: Option<Pending>,
        }
        #[derive(Deserialize)]
        struct File {
            path: String,
            blob: String,
            size: u64,
            fp: Option<[u64; 6]>,
            canon: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case")]
        enum Pending {
            Update { txn: String, commit: String, files: Vec<File> },
            Delete { txn: String },
        }
        pub fn load(path: &Path) -> &'static str {
            match serde_json::from_slice::<Record>(&std::fs::read(path).unwrap()) {
                Ok(record) if record.version > 1 => "refused",
                Ok(_) => "usable",
                Err(_) => {
                    std::fs::rename(path, path.with_file_name("state.json.damaged-v1")).unwrap();
                    "quarantined"
                }
            }
        }
    }

    #[test]
    fn loose_state_and_future_leaf_operations_do_not_enter_a_v1_pending_shape() {
        use crate::source::{Entry, resolve_destination};
        let dir = temp("v1-reader");
        let path = dir.join("state.json");
        let mut state = State::default();
        state.repo_mut("legacy").upsert_skill(OwnedSkill {
            pending: Some(Pending::Update { txn: "t1".into(), commit: "c".into(), files: vec![file("old", true)] }),
            ..skill("old")
        });
        state.save(&path).unwrap();
        assert_eq!(v1_reader::load(&path), "usable");
        let legacy = std::fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&legacy).contains("loose_sources"));
        let source = RepoConfig { id: "new".into(), repo: "o/r".into(), kind: SourceKind::Agents, ..Default::default() };
        let binding = resolve_destination(&source, &dir.join("agents"), &["tool.agent.md".into()], None, |_| Ok(Entry::Missing)).unwrap();
        state.bind_destination(&source, binding.clone()).unwrap();
        state.save(&path).unwrap();
        assert_eq!(State::load_sources_with(&path, true).unwrap().state.binding_for(&source).unwrap(), Some(&binding));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(v1_reader::load(&path), "refused");
        assert!(State::load(&path).is_err(), "even an orphan or a reused ID cannot reach skills recovery");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(wire["repos"], serde_json::from_slice::<serde_json::Value>(&legacy).unwrap()["repos"]);
        let operation = serde_json::json!({"op":"replace_leaf","txn":"a2"});
        wire["loose_sources"]["new"]["pending"] = operation.clone();
        let future = serde_json::to_vec(&wire).unwrap();
        std::fs::write(&path, &future).unwrap();
        assert_eq!(v1_reader::load(&path), "refused", "v1 skips the whole loose namespace");
        assert!(State::load_sources_with(&path, true).is_err(), "A does not understand leaf operations yet");
        assert_eq!(std::fs::read(&path).unwrap(), future);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        // Negative control: merely bumping the header and extending the old enum would have damaged this file.
        wire["repos"]["legacy"]["skills"][0]["pending"] = operation;
        std::fs::write(&path, serde_json::to_vec(&wire).unwrap()).unwrap();
        assert_eq!(v1_reader::load(&path), "quarantined");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bindings_are_local_immutable_and_cannot_reuse_an_old_id() {
        use crate::source::{Entry, resolve_destination};
        let dir = temp("bindings");
        let root = dir.join("agents");
        let source = RepoConfig { id: "id".into(), repo: "o/r".into(), kind: SourceKind::Agents, ..Default::default() };
        let fresh = |r: &Path| resolve_destination(&source, r, &["tool.agent.md".into()], None, |_| Ok(Entry::Missing)).unwrap();
        let binding = fresh(&root);
        let mut state = State::default();
        state.repo_mut("id").last_check = Some(50);
        assert!(state.bind_destination(&source, binding.clone()).is_err());
        let old = std::mem::take(&mut state.repos);
        state.set_aside.push(SetAside { repos: old, ..Default::default() });
        assert!(state.bind_destination(&source, binding.clone()).is_err());
        let mut state = State::default();
        state.bind_destination(&source, binding.clone()).unwrap();
        let before = state.clone();
        state.bind_destination(&source, binding).unwrap();
        assert!(state.bind_destination(&source, fresh(&dir.join("other"))).is_err());
        for kind in [SourceKind::Skills, SourceKind::Instructions] {
            assert!(state.binding_for(&RepoConfig { kind, ..source.clone() }).is_err());
        }
        assert_eq!(state, before);
        assert_eq!(State::default().binding_for(&source).unwrap(), None, "another machine is not bound");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn misplaced_or_unknown_typed_state_is_not_quarantined_or_reinterpreted() {
        let dir = temp("typed-errors");
        let path = dir.join("state.json");
        for bad in [
            r#"{"version":1,"loose_sources":{}}"#,
            r#"{"loose_sources":null}"#,
            r#"{"version":2,"loose_sources":{"s":{"binding":{"kind":null,"root":"C:\\isolated"}}}}"#,
            r#"{"version":2,"loose_sources":{"s":{"binding":{"kind":"plugins","root":"C:\\isolated"}}}}"#,
            r#"{"version":2,"loose_sources":{"s":{"binding":{"kind":"agents","root":"C:\\isolated","child":".."}}}}"#,
            r#"{"version":2,"repos":{"s":{}},"loose_sources":{"s":{"binding":{"kind":"agents","root":"C:\\isolated"}}}}"#,
            r#"{"version":2,"loose_sources":{"s":{"binding":{"kind":"agents","root":"C:\\a"}},"s":{"binding":{"kind":"instructions","root":"C:\\b"}}}}"#,
            r#"{"version":1,"repos":{"s":{"kind":null}}}"#,
            r#"{"version":1,"set_aside":[{"repos":{"s":{"kind":"instructions"}}}]}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            for quarantine in [false, true] {
                assert!(State::load_sources_with(&path, quarantine).is_err(), "{bad}");
                assert_eq!(std::fs::read(&path).unwrap(), bad.as_bytes());
            }
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
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
    fn future_state_is_refused_before_decoding_an_unrecognized_operation() {
        let dir = temp("future-op");
        let path = dir.join("state.json");
        let bytes = br#"{"repos":{"id":{"skills":[{"name":"mine","commit":"c","files":[],"pending":{"op":"future"}}]}},"version":99}"#;
        std::fs::write(&path, bytes).unwrap();
        let refused = State::load(&path).is_err();
        let kept = std::fs::read(&path).ok();
        let entries = std::fs::read_dir(&dir).unwrap().count();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(refused, "not a damaged v1 record");
        assert_eq!(kept.as_deref(), Some(bytes.as_slice()));
        assert_eq!(entries, 1);
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
    fn idless_legacy_settings_still_find_the_edited_owner_and_its_schedule() {
        use crate::config::Config;
        use crate::{reconcile, scheduler::Schedule, verify, win::fs::FileHasher};
        let dir = temp("legacy-owner");
        let path = dir.join("state.json");
        let config_path = dir.join("config.json");
        std::fs::write(&config_path, br#"{"repos":[{"repo":"O/R","path":"one","enabled":false},{"repo":"O/R","path":"two","interval":"6h"}]}"#).unwrap();
        let bytes = br#"{"version":1,"repos":{"o-r-2":{"etag":"tree","last_check":1000,"ignored":["mine"],"skills":[{"name":"old","commit":"c","files":[{"path":"SKILL.md","blob":"original","size":8}]}]}}}"#;
        std::fs::write(&path, bytes).unwrap();
        let cfg = Config::load(&config_path).unwrap();
        let st = State::load(&path).unwrap().state;
        let source = &cfg.repos[1];
        assert_eq!((&source.kind, source.id.as_str()), (&SourceKind::Skills, "o-r-2"));
        let record = st.repo(&source.id).unwrap();
        assert!(record.is_ignored("mine"));
        assert_eq!(Schedule::new().next_wake(&cfg, &st, 1001), Some(1000 + 6 * 3600));
        let owned = record.skill("old").unwrap();
        let folder = dir.join(&owned.name);
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("SKILL.md"), b"my edit").unwrap();
        let local = verify::verify_skill(&folder, owned, verify::Check::Full, &mut FileHasher::default()).local;
        let changes = reconcile::plan(&reconcile::Input {
            desired: &[],
            rejected: &[],
            owned: &[reconcile::OwnedView { skill: owned, local, matches_upstream: false }],
            other_owners: &Default::default(),
            unmanaged: &Default::default(),
            ignored: &Default::default(),
            confirm_mass_delete: true,
            interrupted: &[],
        });
        assert!(matches!(changes[0].verdict, reconcile::Verdict::Held(reconcile::Hold::DeleteBlocked(_))));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_typed_config_cannot_verify_or_release_an_old_skill_with_the_same_id() {
        use crate::config::{Config, Paths};
        use crate::engine::{Access, Engine, Resolution};
        use crate::github::{Remote, RemoteError, RepoRef, TokenProvider, TreeReply};
        use crate::secret::Secret;
        use crate::tree::Snapshot;
        use std::sync::atomic::AtomicBool;

        struct NoServices;
        impl TokenProvider for NoServices {
            fn token(&self) -> Result<Secret, String> {
                unreachable!()
            }
        }
        impl Remote for NoServices {
            fn tree(&self, _: &RepoRef, _: Option<&str>, _: Option<&Secret>, _: &AtomicBool) -> Result<TreeReply, RemoteError> {
                unreachable!()
            }
            fn commit(&self, _: &RepoRef, _: Option<&Secret>, _: &AtomicBool) -> Result<String, RemoteError> {
                unreachable!()
            }
            fn history(&self, _: &RepoRef, _: &str, _: &str, _: u32, _: Option<&Secret>, _: &AtomicBool) -> Result<Vec<String>, RemoteError> {
                unreachable!()
            }
            fn historical_tree(&self, _: &RepoRef, _: &str, _: &str, _: Option<&Secret>, _: &AtomicBool) -> Result<Option<Snapshot>, RemoteError> {
                unreachable!()
            }
            fn file(
                &self,
                _: &RepoRef,
                _: &str,
                _: &str,
                _: u64,
                _: Option<&Secret>,
                _: &AtomicBool,
                _: &mut dyn FnMut(&[u8]) -> bool,
            ) -> Result<(), RemoteError> {
                unreachable!()
            }
        }

        let dir = temp("reused-id");
        let path = dir.join("state.json");
        let bytes = br#"{"version":1,"repos":{"old":{"skills":[{"name":"mine","commit":"c","files":[]}]}}}"#;
        std::fs::write(&path, bytes).unwrap();
        let cfg = Config::parse(br#"{"version":2,"repos":[{"id":"old","repo":"o/r","kind":"agents"}]}"#).unwrap();
        let paths = Paths::new(dir.clone(), dir.join("skills")).with_file_roots(dir.join("agents"), dir.join("instructions"));
        let mut engine = Engine::new(paths, Box::new(NoServices), Box::new(NoServices));
        let verify = engine.verify(&cfg, Access::ReadOnly);
        let release = engine.resolve(&cfg, "old", "mine", Resolution::KeepMine, &AtomicBool::new(false));
        let after = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(verify.is_err() && release.is_err(), "do not enter the skills session with a typed ID");
        assert_eq!(after, bytes);
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

    #[test]
    fn forgetting_a_repository_keeps_the_changes_that_are_not_finished() {
        let mut st = State::default();
        let half = OwnedSkill { pending: Some(Pending::Update { txn: "t1".into(), commit: "d".repeat(40), files: Vec::new() }), ..skill("half") };
        st.repo_mut("r").upsert_skill(skill("done"));
        st.repo_mut("r").upsert_skill(half.clone());
        st.repo_mut("r").ignored.push("mine".into());
        let mut before = RepoState::default();
        before.upsert_skill(OwnedSkill { pending: Some(Pending::Delete { txn: "t2".into() }), ..skill("elsewhere") });
        before.upsert_skill(skill("gone"));
        st.set_aside.push(SetAside { skills_dir: r"D:\skills".to_string(), volume: None, repos: BTreeMap::from([("r".to_string(), before)]) });

        assert!(st.drop_repo("r"));
        assert_eq!(st.repos["r"], RepoState { skills: vec![half], ..RepoState::default() }, "recovery needs the marker to finish or undo the change");
        let names: Vec<&str> = st.set_aside[0].repos["r"].skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["elsewhere"]);
        assert!(!st.drop_repo("r"), "nothing more can be forgotten");
    }
}
