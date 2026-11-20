//! Exact-path, read-only probes for a shared destination. Never list the containing folder.

use std::fs::File;
use std::io;
use std::path::Path;

use crate::reconcile::{DirtyInfo, LocalState};
use crate::source::Entry;
use crate::state::{DiskId, LooseSourceState, ManagedFile};
use crate::text;
use crate::win::fs::{FileHasher, Kind, LeafDirectory, check_leaf_path, kind_at, open_leaf};

#[derive(Debug)]
pub(crate) struct Proof {
    pub id: DiskId,
    pub blob: String,
    pub canon: String,
}

impl Proof {
    pub(crate) fn read(file: &File, hasher: &mut FileHasher) -> io::Result<Self> {
        let id = crate::win::install::id(file)?;
        let read = hasher.read_leaf(file)?;
        Ok(Self { id, blob: read.blob, canon: read.bytes.as_deref().map(text::canon).unwrap_or_default() })
    }

    pub(crate) fn matches(&self, file: &ManagedFile) -> bool {
        self.blob == file.blob || (!self.canon.is_empty() && file.canon.as_ref() == Some(&self.canon))
    }
}

pub(crate) fn optional<T>(value: io::Result<T>) -> io::Result<Option<T>> {
    match value {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        r => r.map(Some),
    }
}

/// Caller holds `dir` throughout the inspection. A file opened for a destructive action remains
/// pinned through its rename; its proof is not a fingerprint or a fact about a prior handle.
pub(crate) enum Found {
    Missing,
    Other,
    File(File, Proof),
}

impl Found {
    pub(crate) fn at(path: &Path, movable: bool, hasher: &mut FileHasher) -> io::Result<Self> {
        Ok(match kind_at(path)? {
            None => Self::Missing,
            Some(Kind::File) => {
                let f = open_leaf(path, movable)?;
                let proof = Proof::read(&f, hasher)?;
                Self::File(f, proof)
            }
            Some(_) => Self::Other,
        })
    }

    pub(crate) fn matches(&self, file: &ManagedFile) -> bool {
        matches!(self, Self::File(_, p) if p.matches(file))
    }

    pub(crate) fn local(&self, file: &ManagedFile) -> LocalState {
        if self.matches(file) {
            LocalState::Clean
        } else if matches!(self, Self::Missing) {
            LocalState::FolderMissing
        } else {
            LocalState::Dirty(DirtyInfo { modified: vec![file.path.clone()], ..Default::default() })
        }
    }
}

/// Placement probe for `resolve_destination`. Supply this named candidate's upstream and/or
/// installed versions only. A copy with different bytes needs the proven upstream text canon.
pub fn entry(path: &Path, known: &[ManagedFile], hasher: &mut FileHasher) -> Result<Entry, String> {
    let mut inspect = || -> io::Result<Entry> {
        check_leaf_path(path)?;
        let parent = path.parent().unwrap_or(path);
        let Some(_pin) = optional(LeafDirectory::pin(parent, false))? else { return Ok(Entry::Missing) };
        Ok(match kind_at(path)? {
            None => Entry::Missing,
            Some(Kind::Link) => Entry::Link,
            Some(Kind::Dir) => {
                LeafDirectory::pin(path, false)?;
                Entry::Directory
            }
            Some(Kind::File) => {
                let f = open_leaf(path, false)?;
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                let mut candidates = known.iter().filter(|v| crate::pathsafe::fold(&v.path) == crate::pathsafe::fold(name)).peekable();
                if candidates.peek().is_some() && {
                    let proof = Proof::read(&f, hasher)?;
                    candidates.any(|v| proof.matches(v))
                } {
                    Entry::CompatibleFile
                } else {
                    Entry::Conflict
                }
            }
        })
    };
    inspect().map_err(|e| format!("cannot inspect {}: {e}", path.display()))
}

/// Full, read-only status check. A missing file is distinct from an unreadable or redirected
/// path. Call only after checking the configured binding; no state or fingerprints are refreshed.
pub fn status(record: &LooseSourceState, current: &crate::source::DestinationBinding, name: &str, hasher: &mut FileHasher) -> Result<LocalState, String> {
    if record.binding() != current {
        return Err("the loose destination binding changed".into());
    }
    let leaf = record.leaf(name).ok_or("file is not recorded")?;
    let version = leaf.installed.as_ref().filter(|_| !leaf.interrupted()).ok_or("file has an interrupted change")?;
    let dir = current.directory();
    let pin = optional(LeafDirectory::pin(&dir, false)).map_err(|e| e.to_string())?;
    let Some(pin) = pin else { return Ok(LocalState::FolderMissing) };
    if Some(pin.id().map_err(|e| e.to_string())?) != record.directory_id {
        return Err("the recorded destination directory changed".into());
    }
    Found::at(&dir.join(&leaf.name), false, hasher).map(|p| p.local(&version.file)).map_err(|e| e.to_string())
}
