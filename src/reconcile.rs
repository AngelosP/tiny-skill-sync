//! The decision table: given what is upstream, what we own, and what is on disk, what may we do?
//!
//! This is a pure function on purpose. Every rule that protects the user's files is here and is tested
//! without touching a disk or a network.
//!
//! Rules in one place:
//! * Delete or update only skills we own, and only when their folder is verified clean.
//! * Any local deviation (edited, missing file, extra file, missing folder) HOLDs the change: nothing is touched.
//! * A skill that is absent upstream is deleted only from a complete, valid snapshot. Errors never reach this code.
//! * A skill that exists upstream but fails validation is kept as it is.
//! * A listing with no skills at all, while we own some, is held for confirmation before anything is deleted.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::pathsafe::fold;
use crate::state::{ManagedFile, OwnedSkill};
use crate::tree::{Rejected, Skill};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirtyInfo {
    pub modified: Vec<String>,
    pub missing: Vec<String>,
    pub extra: Vec<String>,
}

impl DirtyInfo {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.missing.is_empty() && self.extra.is_empty()
    }
}

impl fmt::Display for DirtyInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        for (label, list) in [("changed", &self.modified), ("missing", &self.missing), ("extra", &self.extra)] {
            if list.is_empty() {
                continue;
            }
            let shown: Vec<&str> = list.iter().take(3).map(String::as_str).collect();
            let more = if list.len() > 3 { format!(" and {} more", list.len() - 3) } else { String::new() };
            parts.push(format!("{label}: {}{more}", shown.join(", ")));
        }
        write!(f, "{}", parts.join("; "))
    }
}

/// What verification found for a skill we own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalState {
    Clean,
    Dirty(DirtyInfo),
    FolderMissing,
}

/// A folder with the same name as an upstream skill that nobody owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnmanagedState {
    /// Every file matches upstream and nothing else is in it: safe to start managing without writing a byte.
    Identical,
    Different,
}

pub struct OwnedView<'a> {
    pub skill: &'a OwnedSkill,
    pub local: LocalState,
}

pub struct Input<'a> {
    pub desired: &'a [Skill],
    pub rejected: &'a [Rejected],
    pub owned: &'a [OwnedView<'a>],
    /// Case-folded names owned by other repositories, with the owner's id.
    pub other_owners: &'a HashMap<String, String>,
    /// Case-folded names of existing folders that nobody owns (only names that matter here need to be listed).
    pub unmanaged: &'a HashMap<String, UnmanagedState>,
    /// Case-folded names the user chose to keep as their own.
    pub ignored: &'a HashSet<String>,
    /// The user has confirmed that removing every skill of this repository is intended.
    pub confirm_mass_delete: bool,
    /// Names of owned skills whose last change was interrupted and not yet sorted out. They are not in `owned`:
    /// nothing is decided about them until recovery has run.
    pub interrupted: &'a [String],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hold {
    UpdateBlocked(DirtyInfo),
    UpdateBlockedMissing,
    DeleteBlocked(DirtyInfo),
    UnmanagedDiffers,
    MassDeletion,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skip {
    Ignored,
    Collision { owner: String },
    Rejected { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    InSync,
    Install,
    Update,
    /// Start managing an identical existing folder.
    Adopt,
    Delete,
    /// We own it, it is gone locally and gone upstream: just drop the record.
    Forget,
    /// Upstream is unchanged but the local copy differs. Informational.
    ModifiedLocally(DirtyInfo),
    MissingLocally,
    Held(Hold),
    Skipped(Skip),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub name: String,
    pub verdict: Verdict,
}

impl Verdict {
    /// True when this verdict changes files or the ownership record.
    pub fn is_action(&self) -> bool {
        matches!(self, Verdict::Install | Verdict::Update | Verdict::Adopt | Verdict::Delete | Verdict::Forget)
    }

    /// True when the skill needs the user's attention.
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            Verdict::Held(_)
                | Verdict::ModifiedLocally(_)
                | Verdict::MissingLocally
                | Verdict::Skipped(Skip::Rejected { .. })
                | Verdict::Skipped(Skip::Collision { .. })
        )
    }

    pub fn message(&self) -> String {
        match self {
            Verdict::InSync => "In sync".to_string(),
            Verdict::Install => "New: will be installed".to_string(),
            Verdict::Update => "Update available".to_string(),
            Verdict::Adopt => "Already installed and identical: now managed".to_string(),
            Verdict::Delete => "Removed upstream: will be deleted".to_string(),
            Verdict::Forget => "Removed upstream and locally".to_string(),
            Verdict::ModifiedLocally(info) => format!("Edited locally ({info})"),
            Verdict::MissingLocally => "Deleted locally".to_string(),
            Verdict::Held(Hold::UpdateBlocked(info)) => format!("Update held: edited locally ({info})"),
            Verdict::Held(Hold::UpdateBlockedMissing) => "Update held: the folder was deleted locally".to_string(),
            Verdict::Held(Hold::DeleteBlocked(info)) => format!("Removal held: edited locally ({info})"),
            Verdict::Held(Hold::UnmanagedDiffers) => "Held: a folder with this name already exists and differs from upstream".to_string(),
            Verdict::Held(Hold::MassDeletion) => "Removal held: upstream lists no skills at all".to_string(),
            Verdict::Held(Hold::Interrupted) => "Held: an earlier change was interrupted and is not sorted out yet".to_string(),
            Verdict::Skipped(Skip::Ignored) => "Kept as yours".to_string(),
            Verdict::Skipped(Skip::Collision { owner }) => format!("Skipped: another repository ({owner}) already provides this name"),
            Verdict::Skipped(Skip::Rejected { reason }) => format!("Skipped: not safe to install ({reason})"),
        }
    }
}

fn same_files(desired: &Skill, owned: &[ManagedFile]) -> bool {
    desired.files.len() == owned.len() && desired.files.iter().zip(owned).all(|(d, o)| d.path == o.path && d.blob == o.blob)
}

pub fn plan(input: &Input) -> Vec<Decision> {
    let mut out = Vec::new();
    let desired_keys: HashSet<String> = input.desired.iter().map(|s| fold(&s.name)).collect();
    let owned_by_key: HashMap<String, &OwnedView> = input.owned.iter().map(|o| (fold(&o.skill.name), o)).collect();
    let rejected_by_key: HashMap<String, &Rejected> = input.rejected.iter().map(|r| (fold(&r.name), r)).collect();
    let interrupted_keys: HashSet<String> = input.interrupted.iter().map(|n| fold(n)).collect();

    for skill in input.desired {
        let key = fold(&skill.name);
        let verdict = if interrupted_keys.contains(&key) {
            Verdict::Held(Hold::Interrupted)
        } else if input.ignored.contains(&key) {
            Verdict::Skipped(Skip::Ignored)
        } else if let Some(owner) = input.other_owners.get(&key) {
            Verdict::Skipped(Skip::Collision { owner: owner.clone() })
        } else if let Some(owned) = owned_by_key.get(&key) {
            owned_verdict(skill, owned)
        } else {
            match input.unmanaged.get(&key) {
                Some(UnmanagedState::Identical) => Verdict::Adopt,
                Some(UnmanagedState::Different) => Verdict::Held(Hold::UnmanagedDiffers),
                None => Verdict::Install,
            }
        };
        out.push(Decision { name: skill.name.clone(), verdict });
    }

    let hold_all_deletions = input.desired.is_empty() && input.rejected.is_empty() && !input.owned.is_empty() && !input.confirm_mass_delete;
    for owned in input.owned {
        let key = fold(&owned.skill.name);
        if desired_keys.contains(&key) {
            continue;
        }
        let verdict = if let Some(r) = rejected_by_key.get(&key) {
            Verdict::Skipped(Skip::Rejected { reason: r.reason.clone() })
        } else {
            match &owned.local {
                LocalState::Clean if hold_all_deletions => Verdict::Held(Hold::MassDeletion),
                LocalState::Clean => Verdict::Delete,
                LocalState::Dirty(info) => Verdict::Held(Hold::DeleteBlocked(info.clone())),
                LocalState::FolderMissing => Verdict::Forget,
            }
        };
        out.push(Decision { name: owned.skill.name.clone(), verdict });
    }

    for name in input.interrupted {
        if !desired_keys.contains(&fold(name)) {
            out.push(Decision { name: name.clone(), verdict: Verdict::Held(Hold::Interrupted) });
        }
    }

    out.sort_by_key(|d| fold(&d.name));
    out
}

fn owned_verdict(desired: &Skill, owned: &OwnedView) -> Verdict {
    let same = same_files(desired, &owned.skill.files);
    match (&owned.local, same) {
        (LocalState::Clean, true) => Verdict::InSync,
        (LocalState::Clean, false) => Verdict::Update,
        (LocalState::Dirty(info), true) => Verdict::ModifiedLocally(info.clone()),
        (LocalState::Dirty(info), false) => Verdict::Held(Hold::UpdateBlocked(info.clone())),
        (LocalState::FolderMissing, true) => Verdict::MissingLocally,
        (LocalState::FolderMissing, false) => Verdict::Held(Hold::UpdateBlockedMissing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::SkillFile;

    fn up(name: &str, content: &str) -> Skill {
        Skill {
            name: name.into(),
            files: vec![SkillFile { path: "SKILL.md".into(), blob: crate::hash::blob_hex(content.as_bytes()), size: content.len() as u64 }],
        }
    }

    fn own(name: &str, content: &str) -> OwnedSkill {
        OwnedSkill {
            name: name.into(),
            commit: "c".repeat(40),
            files: vec![ManagedFile {
                path: "SKILL.md".into(),
                blob: crate::hash::blob_hex(content.as_bytes()),
                size: content.len() as u64,
                fp: None,
                canon: None,
            }],
            pending: None,
        }
    }

    fn dirty() -> LocalState {
        LocalState::Dirty(DirtyInfo { modified: vec!["SKILL.md".into()], ..DirtyInfo::default() })
    }

    struct Case {
        desired: Vec<Skill>,
        rejected: Vec<Rejected>,
        owned: Vec<(OwnedSkill, LocalState)>,
        other: HashMap<String, String>,
        unmanaged: HashMap<String, UnmanagedState>,
        ignored: HashSet<String>,
        confirm: bool,
        interrupted: Vec<String>,
    }

    impl Case {
        fn new() -> Case {
            Case {
                desired: vec![],
                rejected: vec![],
                owned: vec![],
                other: HashMap::new(),
                unmanaged: HashMap::new(),
                ignored: HashSet::new(),
                confirm: false,
                interrupted: vec![],
            }
        }

        fn run(&self) -> Vec<(String, Verdict)> {
            let views: Vec<OwnedView> = self.owned.iter().map(|(s, l)| OwnedView { skill: s, local: l.clone() }).collect();
            let input = Input {
                desired: &self.desired,
                rejected: &self.rejected,
                owned: &views,
                other_owners: &self.other,
                unmanaged: &self.unmanaged,
                ignored: &self.ignored,
                confirm_mass_delete: self.confirm,
                interrupted: &self.interrupted,
            };
            plan(&input).into_iter().map(|d| (d.name, d.verdict)).collect()
        }

        fn only(&self) -> Verdict {
            let mut r = self.run();
            assert_eq!(r.len(), 1, "{r:?}");
            r.remove(0).1
        }
    }

    #[test]
    fn an_interrupted_skill_is_held_whatever_upstream_says() {
        let mut c = Case::new();
        c.interrupted = vec!["Alpha".into()];
        // Still upstream, changed upstream, or gone upstream: nothing is decided until recovery has run.
        c.desired = vec![up("alpha", "v2")];
        assert_eq!(c.only(), Verdict::Held(Hold::Interrupted));
        c.desired = vec![];
        assert_eq!(c.only(), Verdict::Held(Hold::Interrupted));
        assert!(Verdict::Held(Hold::Interrupted).message().contains("interrupted"));
        assert!(Verdict::Held(Hold::Interrupted).needs_attention());
        assert!(!Verdict::Held(Hold::Interrupted).is_action());
    }

    #[test]
    fn an_interrupted_skill_does_not_hide_the_rest_or_trigger_the_mass_deletion_rule() {
        let mut c = Case::new();
        c.interrupted = vec!["stuck".into()];
        c.desired = vec![up("fresh", "f")];
        let r = c.run();
        assert_eq!(r, vec![("fresh".into(), Verdict::Install), ("stuck".into(), Verdict::Held(Hold::Interrupted))]);
    }

    #[test]
    fn new_skill_is_installed() {
        let mut c = Case::new();
        c.desired = vec![up("alpha", "v1")];
        assert_eq!(c.only(), Verdict::Install);
    }

    #[test]
    fn owned_clean_skill_is_in_sync_or_updated() {
        let mut c = Case::new();
        c.desired = vec![up("alpha", "v1")];
        c.owned = vec![(own("alpha", "v1"), LocalState::Clean)];
        assert_eq!(c.only(), Verdict::InSync);
        c.desired = vec![up("alpha", "v2")];
        assert_eq!(c.only(), Verdict::Update);
    }

    #[test]
    fn local_edits_hold_an_update_but_do_not_block_when_upstream_is_unchanged() {
        let mut c = Case::new();
        c.desired = vec![up("alpha", "v1")];
        c.owned = vec![(own("alpha", "v1"), dirty())];
        assert!(matches!(c.only(), Verdict::ModifiedLocally(_)));
        c.desired = vec![up("alpha", "v2")];
        assert!(matches!(c.only(), Verdict::Held(Hold::UpdateBlocked(_))));
    }

    #[test]
    fn a_folder_deleted_locally_is_never_reinstalled_silently() {
        let mut c = Case::new();
        c.desired = vec![up("alpha", "v1")];
        c.owned = vec![(own("alpha", "v1"), LocalState::FolderMissing)];
        assert_eq!(c.only(), Verdict::MissingLocally);
        c.desired = vec![up("alpha", "v2")];
        assert_eq!(c.only(), Verdict::Held(Hold::UpdateBlockedMissing));
    }

    #[test]
    fn upstream_removal_deletes_only_clean_owned_skills() {
        let mut c = Case::new();
        c.desired = vec![up("keep", "k")];
        c.owned = vec![(own("keep", "k"), LocalState::Clean), (own("gone", "g"), LocalState::Clean)];
        let r = c.run();
        assert_eq!(r, vec![("gone".into(), Verdict::Delete), ("keep".into(), Verdict::InSync)]);

        c.owned = vec![(own("keep", "k"), LocalState::Clean), (own("gone", "g"), dirty())];
        assert!(matches!(c.run()[0].1, Verdict::Held(Hold::DeleteBlocked(_))));

        c.owned = vec![(own("keep", "k"), LocalState::Clean), (own("gone", "g"), LocalState::FolderMissing)];
        assert_eq!(c.run()[0].1, Verdict::Forget);
    }

    #[test]
    fn a_skill_that_fails_validation_upstream_is_kept_untouched() {
        let mut c = Case::new();
        c.desired = vec![up("keep", "k")];
        c.rejected = vec![Rejected { name: "bad".into(), reason: "symbolic link".into() }];
        c.owned = vec![(own("keep", "k"), LocalState::Clean), (own("bad", "b"), LocalState::Clean)];
        let r = c.run();
        assert_eq!(r[0].0, "bad");
        assert!(matches!(&r[0].1, Verdict::Skipped(Skip::Rejected { reason }) if reason == "symbolic link"));
    }

    #[test]
    fn an_empty_listing_holds_every_deletion_until_confirmed() {
        let mut c = Case::new();
        c.owned = vec![(own("a", "1"), LocalState::Clean), (own("b", "2"), LocalState::Clean), (own("c", "3"), LocalState::FolderMissing)];
        let r = c.run();
        assert_eq!(r[0].1, Verdict::Held(Hold::MassDeletion));
        assert_eq!(r[1].1, Verdict::Held(Hold::MassDeletion));
        assert_eq!(r[2].1, Verdict::Forget);
        c.confirm = true;
        let r = c.run();
        assert_eq!((&r[0].1, &r[1].1), (&Verdict::Delete, &Verdict::Delete));
    }

    #[test]
    fn rejected_skills_are_not_an_empty_listing() {
        let mut c = Case::new();
        c.rejected = vec![Rejected { name: "a".into(), reason: "x".into() }];
        c.owned = vec![(own("a", "1"), LocalState::Clean), (own("b", "2"), LocalState::Clean)];
        let r = c.run();
        assert!(matches!(r[0].1, Verdict::Skipped(Skip::Rejected { .. })));
        assert_eq!(r[1].1, Verdict::Delete);
    }

    #[test]
    fn nothing_owned_and_nothing_upstream_is_quiet() {
        assert!(Case::new().run().is_empty());
    }

    #[test]
    fn existing_unmanaged_folders_are_adopted_only_when_identical() {
        let mut c = Case::new();
        c.desired = vec![up("alpha", "v1")];
        c.unmanaged.insert("alpha".into(), UnmanagedState::Identical);
        assert_eq!(c.only(), Verdict::Adopt);
        c.unmanaged.insert("alpha".into(), UnmanagedState::Different);
        assert_eq!(c.only(), Verdict::Held(Hold::UnmanagedDiffers));
    }

    #[test]
    fn ignored_and_colliding_names_are_skipped() {
        let mut c = Case::new();
        c.desired = vec![up("alpha", "v1")];
        c.ignored.insert("alpha".into());
        assert_eq!(c.only(), Verdict::Skipped(Skip::Ignored));
        c.ignored.clear();
        c.other.insert("alpha".into(), "other-repo".into());
        assert_eq!(c.only(), Verdict::Skipped(Skip::Collision { owner: "other-repo".into() }));
    }

    #[test]
    fn names_match_without_regard_to_case() {
        let mut c = Case::new();
        c.desired = vec![up("Alpha", "v1")];
        c.owned = vec![(own("alpha", "v1"), LocalState::Clean)];
        assert_eq!(c.only(), Verdict::InSync);
    }

    #[test]
    fn file_list_changes_count_as_updates_even_when_a_file_is_only_added() {
        let mut c = Case::new();
        let mut d = up("alpha", "v1");
        d.files.push(SkillFile { path: "z.md".into(), blob: "0".repeat(40), size: 1 });
        c.desired = vec![d];
        c.owned = vec![(own("alpha", "v1"), LocalState::Clean)];
        assert_eq!(c.only(), Verdict::Update);
    }

    #[test]
    fn messages_are_plain_and_mention_the_files() {
        let info = DirtyInfo { modified: vec!["a".into(), "b".into(), "c".into(), "d".into()], missing: vec!["m".into()], extra: vec![] };
        let text = Verdict::Held(Hold::UpdateBlocked(info)).message();
        assert!(text.contains("changed: a, b, c and 1 more"), "{text}");
        assert!(text.contains("missing: m"), "{text}");
        assert!(Verdict::Held(Hold::MassDeletion).needs_attention());
        assert!(!Verdict::InSync.needs_attention());
        assert!(Verdict::Update.is_action() && !Verdict::MissingLocally.is_action());
    }
}
