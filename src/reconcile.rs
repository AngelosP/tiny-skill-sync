//! The decision table: given what is upstream, what we own, and what is on disk, what may we do?
//!
//! This is a pure function on purpose. Every rule that protects the user's files is here and is tested
//! without touching a disk or a network.
//!
//! Rules in one place:
//! * Delete or update only skills we own, and only when their folder is verified clean.
//! * Any local deviation in the files that came from GitHub (edited, missing, replaced, or the whole folder missing)
//!   HOLDs the change: nothing is touched. Files the user added are theirs and are not a deviation. An edit that is
//!   exactly what upstream has now is no longer a deviation: the record catches up and no file is written.
//! * A skill that is absent upstream is deleted only from a complete, valid snapshot. Errors never reach this code.
//! * A skill that exists upstream but fails validation is kept as it is.
//! * A listing with no skills at all, while we own some, is held for confirmation before anything is deleted.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::pathsafe::fold;
use crate::state::{ManagedFile, OwnedSkill};
use crate::tree::{Rejected, Skill, SkillFile};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirtyInfo {
    pub modified: Vec<String>,
    pub missing: Vec<String>,
}

impl DirtyInfo {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.missing.is_empty()
    }
}

impl fmt::Display for DirtyInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        for (label, list) in [("changed", &self.modified), ("missing", &self.missing)] {
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
    /// Every file upstream has is here and matches; anything else in the folder is the user's and stays. Safe to start
    /// managing without writing a byte.
    Identical,
    Historical,
    Searching,
    Paused,
    Different,
}

pub struct OwnedView<'a> {
    pub skill: &'a OwnedSkill,
    pub local: LocalState,
    /// The folder is what upstream has now, file for file (every file, under the usual text rules), although it
    /// differs from what was recorded. Only worked out where `needs_comparison` says so, and `false` otherwise.
    pub matches_upstream: bool,
}

pub struct Input<'a> {
    pub desired: &'a [Skill],
    pub rejected: &'a [Rejected],
    /// Names the listing has, but which are not to be installed (filtered by a folder rule, or bundled). Positive
    /// evidence: an owned one is kept as it is, never updated or removed. They do not count against the empty-listing
    /// guard: only what the lane would install (`desired`) or rejects (`rejected`) makes a listing non-empty.
    pub protected: &'a [Rejected],
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
    HistorySearching,
    HistoryPaused,
    MassDeletion,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skip {
    Ignored,
    Collision {
        owner: String,
    },
    Rejected {
        reason: String,
    },
    /// Listed, but not synced (a filter, a package or a type switch): kept as it is, with its files and ownership.
    Protected {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    InSync,
    Install,
    Update,
    /// Start managing an identical existing folder.
    Adopt,
    AdoptHistorical,
    Delete,
    /// We own it, it was edited here, and upstream has now moved to the same content: bring the record up to date.
    /// No file is written.
    CatchUp,
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
        matches!(self, Verdict::Install | Verdict::Update | Verdict::Adopt | Verdict::AdoptHistorical | Verdict::CatchUp | Verdict::Delete | Verdict::Forget)
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
            Verdict::AdoptHistorical => "An older copy from GitHub: will be adopted and updated".to_string(),
            Verdict::CatchUp => "Edited here, and GitHub has the same: marked as in sync".to_string(),
            Verdict::Delete => "Removed upstream: will be deleted".to_string(),
            Verdict::Forget => "Removed upstream and locally".to_string(),
            Verdict::ModifiedLocally(info) => format!("Edited locally ({info})"),
            Verdict::MissingLocally => "Deleted locally".to_string(),
            Verdict::Held(Hold::UpdateBlocked(info)) => format!("Update held: edited locally ({info})"),
            Verdict::Held(Hold::UpdateBlockedMissing) => "Update held: the folder was deleted locally".to_string(),
            Verdict::Held(Hold::DeleteBlocked(info)) => format!("Removal held: edited locally ({info})"),
            Verdict::Held(Hold::UnmanagedDiffers) => "Held: a folder with this name already exists and differs from upstream".to_string(),
            Verdict::Held(Hold::HistorySearching) => {
                "Searching earlier versions from GitHub; only this skill waits. Cancel to pause, or choose a version".to_string()
            }
            Verdict::Held(Hold::HistoryPaused) => "Earlier-version search paused; Resume to continue, or choose a version".to_string(),
            Verdict::Held(Hold::MassDeletion) => "Removal held: upstream lists no skills at all".to_string(),
            Verdict::Held(Hold::Interrupted) => "Held: an earlier change was interrupted and is not sorted out yet".to_string(),
            Verdict::Skipped(Skip::Ignored) => "Kept as yours".to_string(),
            Verdict::Skipped(Skip::Collision { owner }) => format!("Skipped: another repository ({owner}) already provides this name"),
            Verdict::Skipped(Skip::Rejected { reason }) => format!("Skipped: not safe to install ({reason})"),
            Verdict::Skipped(Skip::Protected { reason }) => format!("Kept as it is, not synced: {reason}"),
        }
    }
}

fn same_files(desired: &Skill, owned: &[ManagedFile]) -> bool {
    desired.files.len() == owned.len() && desired.files.iter().zip(owned).all(|(d, o)| d.path == o.path && d.blob == o.blob)
}

/// Whether the folder of a skill we own has to be compared with what upstream has now: it was edited here, and
/// upstream moved on as well. Without that, the answer is already known, and nothing is read or downloaded for it.
pub fn needs_comparison(desired: &Skill, owned: &OwnedSkill, local: &LocalState) -> bool {
    matches!(local, LocalState::Dirty(_)) && !same_files(desired, &owned.files)
}

fn gate(interrupted: bool, ignored: bool, owner: Option<&str>) -> Option<Verdict> {
    if interrupted {
        Some(Verdict::Held(Hold::Interrupted))
    } else if ignored {
        Some(Verdict::Skipped(Skip::Ignored))
    } else {
        owner.map(|o| Verdict::Skipped(Skip::Collision { owner: o.into() }))
    }
}

fn unmanaged_verdict(local: Option<UnmanagedState>) -> Verdict {
    match local {
        Some(UnmanagedState::Identical) => Verdict::Adopt,
        Some(UnmanagedState::Historical) => Verdict::AdoptHistorical,
        Some(UnmanagedState::Searching) => Verdict::Held(Hold::HistorySearching),
        Some(UnmanagedState::Paused) => Verdict::Held(Hold::HistoryPaused),
        Some(UnmanagedState::Different) => Verdict::Held(Hold::UnmanagedDiffers),
        None => Verdict::Install,
    }
}

fn remove_verdict(local: &LocalState, hold_empty: bool) -> Verdict {
    match local {
        LocalState::Clean if hold_empty => Verdict::Held(Hold::MassDeletion),
        LocalState::Clean => Verdict::Delete,
        LocalState::Dirty(info) => Verdict::Held(Hold::DeleteBlocked(info.clone())),
        LocalState::FolderMissing => Verdict::Forget,
    }
}

/// A rejected or protected name keeps an owned folder when Windows would treat the two as the same name.
pub(crate) fn listed_as<'r>(list: &'r [Rejected], name: &str) -> Option<&'r Rejected> {
    // Two ASCII names are the same to Windows exactly when their case folds are; only other names need to ask it.
    list.iter().find(|r| fold(&r.name) == fold(name) || ((!r.name.is_ascii() || !name.is_ascii()) && crate::win::fs::same_win_name(&r.name, name)))
}

pub fn plan(input: &Input) -> Vec<Decision> {
    let mut out = Vec::new();
    let desired_keys: HashSet<String> = input.desired.iter().map(|s| fold(&s.name)).collect();
    let owned_by_key: HashMap<String, &OwnedView> = input.owned.iter().map(|o| (fold(&o.skill.name), o)).collect();
    let interrupted_keys: HashSet<String> = input.interrupted.iter().map(|n| fold(n)).collect();

    for skill in input.desired {
        let key = fold(&skill.name);
        let verdict = if let Some(v) = gate(interrupted_keys.contains(&key), input.ignored.contains(&key), input.other_owners.get(&key).map(String::as_str)) {
            v
        } else if input.unmanaged.get(&key) == Some(&UnmanagedState::Paused) {
            Verdict::Held(Hold::HistoryPaused)
        } else if let Some(owned) = owned_by_key.get(&key) {
            owned_verdict(&owned.local, same_files(skill, &owned.skill.files), owned.matches_upstream)
        } else {
            unmanaged_verdict(input.unmanaged.get(&key).copied())
        };
        out.push(Decision { name: skill.name.clone(), verdict });
    }

    for (name, state) in input.unmanaged {
        if *state == UnmanagedState::Paused && !desired_keys.contains(name) {
            out.push(Decision { name: name.clone(), verdict: Verdict::Held(Hold::HistoryPaused) });
        }
    }

    let hold_all_deletions = input.desired.is_empty() && input.rejected.is_empty() && !input.owned.is_empty() && !input.confirm_mass_delete;
    for owned in input.owned {
        let key = fold(&owned.skill.name);
        if desired_keys.contains(&key) || input.unmanaged.get(&key) == Some(&UnmanagedState::Paused) {
            continue;
        }
        let verdict = if let Some(r) = listed_as(input.rejected, &owned.skill.name) {
            Verdict::Skipped(Skip::Rejected { reason: r.reason.clone() })
        } else if let Some(r) = listed_as(input.protected, &owned.skill.name) {
            Verdict::Skipped(Skip::Protected { reason: r.reason.clone() })
        } else {
            remove_verdict(&owned.local, hold_all_deletions)
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

fn owned_verdict(local: &LocalState, same: bool, matches_upstream: bool) -> Verdict {
    match (local, same) {
        (LocalState::Clean, true) => Verdict::InSync,
        (LocalState::Clean, false) => Verdict::Update,
        (LocalState::Dirty(info), true) => Verdict::ModifiedLocally(info.clone()),
        (LocalState::Dirty(_), false) if matches_upstream => Verdict::CatchUp,
        (LocalState::Dirty(info), false) => Verdict::Held(Hold::UpdateBlocked(info.clone())),
        (LocalState::FolderMissing, true) => Verdict::MissingLocally,
        (LocalState::FolderMissing, false) => Verdict::Held(Hold::UpdateBlockedMissing),
    }
}

/// Upstream evidence for exactly one loose file. No constructor treats a failed or partial
/// listing as absence. A removal requested by the user does not imply consent to discard an edit.
#[derive(Clone, Copy)]
pub struct LeafListing<'a> {
    pub(crate) name: &'a str,
    scope: Option<&'a crate::config::RepoConfig>,
    fact: LeafFact<'a>,
}

#[derive(Clone, Copy)]
enum LeafFact<'a> {
    Listed(&'a SkillFile),
    Rejected(&'a str),
    Protected(&'a str),
    Absent { hold_empty: bool },
}

impl<'a> LeafListing<'a> {
    /// Positive evidence only (also useful for a historical copy proven by the engine).
    pub fn listed(file: &'a SkillFile) -> Self {
        Self { name: &file.path, scope: None, fact: LeafFact::Listed(file) }
    }

    /// Only B's validated, fresh discovery of the recorded scope can authorize an inferred
    /// removal. A preview, failed request, partial listing or listing with refusals cannot.
    pub fn from_discovery(
        name: &'a str,
        discovery: &'a crate::tree::SourceDiscovery,
        recorded: &'a crate::config::RepoConfig,
        confirm_empty: bool,
    ) -> Result<Self, String> {
        if !discovery.covers_leaf(recorded, name) {
            return Err("the listing does not cover this name in the recorded source".into());
        }
        let crate::tree::SourceItems::Files(files) = discovery.items() else { return Err("expected a loose-file listing".into()) };
        let key = fold(name);
        let fact = if let Ok(i) = files.binary_search_by(|f| fold(&f.path).cmp(&key)) {
            let file = &files[i];
            if file.path == name || crate::win::fs::same_win_name(&file.path, name) {
                LeafFact::Listed(file)
            } else {
                LeafFact::Rejected("another spelling has the same folded name")
            }
        } else if let Some(r) = discovery.rejected().iter().find(|r| r.name == name || crate::win::fs::same_win_name(&r.name, name)) {
            LeafFact::Rejected(&r.reason)
        } else if let Some(r) = discovery.protected().iter().find(|r| r.name == name || crate::win::fs::same_win_name(&r.name, name)) {
            LeafFact::Protected(&r.reason)
        } else if discovery.confirms_leaf_absent(recorded, name) {
            // The empty-listing guard counts only what this lane would install or rejects; items it never syncs
            // (filtered or bundled) do not make the listing non-empty.
            LeafFact::Absent { hold_empty: files.is_empty() && discovery.rejected().is_empty() && !confirm_empty }
        } else {
            return Err("the listing does not prove this file is absent from the recorded source".into());
        };
        Ok(Self { name, scope: Some(recorded), fact })
    }

    pub fn confirmed_removal(name: &'a str) -> Self {
        Self { name, scope: None, fact: LeafFact::Absent { hold_empty: false } }
    }

    pub(crate) fn applies_to(self, source: &crate::config::RepoConfig) -> bool {
        self.scope.is_none_or(|s| s.id == source.id && s.kind == source.kind && s.repo == source.repo && s.branch == source.branch && s.path == source.path)
    }

    pub fn desired(self) -> Option<&'a SkillFile> {
        if let LeafFact::Listed(f) = self.fact { Some(f) } else { None }
    }
}

pub(crate) struct LeafFacts<'a> {
    pub owned: Option<(&'a ManagedFile, LocalState)>,
    pub exists: bool,
    pub matches_upstream: bool,
    pub interrupted: bool,
    pub ignored: bool,
    pub other_owner: Option<&'a str>,
}

/// File-level inputs feed the same table as folder-level inputs. A missing *file* remains a
/// conflict on removal; there is no directory disappearance from which to infer a forget.
pub(crate) fn plan_leaf(listing: LeafListing, facts: LeafFacts) -> Verdict {
    if let Some(v) = gate(facts.interrupted, facts.ignored, facts.other_owner) {
        return v;
    }
    match (listing.fact, facts.owned) {
        (LeafFact::Rejected(reason), _) => Verdict::Skipped(Skip::Rejected { reason: reason.into() }),
        (LeafFact::Protected(reason), _) => Verdict::Skipped(Skip::Protected { reason: reason.into() }),
        (LeafFact::Listed(f), Some((old, local))) => owned_verdict(&local, old.blob == f.blob, facts.matches_upstream),
        (LeafFact::Listed(_), None) => {
            unmanaged_verdict(facts.exists.then_some(if facts.matches_upstream { UnmanagedState::Identical } else { UnmanagedState::Different }))
        }
        (LeafFact::Absent { .. }, Some((_, LocalState::FolderMissing))) => {
            Verdict::Held(Hold::DeleteBlocked(DirtyInfo { missing: vec![listing.name.into()], ..Default::default() }))
        }
        (LeafFact::Absent { hold_empty }, Some((_, local))) => remove_verdict(&local, hold_empty),
        (LeafFact::Absent { .. }, None) => Verdict::InSync,
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
            updated: None,
            pending: None,
        }
    }

    fn dirty() -> LocalState {
        LocalState::Dirty(DirtyInfo { modified: vec!["SKILL.md".into()], ..DirtyInfo::default() })
    }

    struct Case {
        desired: Vec<Skill>,
        rejected: Vec<Rejected>,
        protected: Vec<Rejected>,
        owned: Vec<(OwnedSkill, LocalState)>,
        /// Case-folded names of owned skills whose folder was found to be what upstream has now.
        matching: HashSet<String>,
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
                protected: vec![],
                owned: vec![],
                matching: HashSet::new(),
                other: HashMap::new(),
                unmanaged: HashMap::new(),
                ignored: HashSet::new(),
                confirm: false,
                interrupted: vec![],
            }
        }

        fn run(&self) -> Vec<(String, Verdict)> {
            let views: Vec<OwnedView> =
                self.owned.iter().map(|(s, l)| OwnedView { skill: s, local: l.clone(), matches_upstream: self.matching.contains(&fold(&s.name)) }).collect();
            let input = Input {
                desired: &self.desired,
                rejected: &self.rejected,
                protected: &self.protected,
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
    fn a_listed_name_in_another_ascii_case_is_the_same_item() {
        let list = [Rejected { name: "Alpha".into(), reason: "'examples' is an ignored folder".into() }];
        assert_eq!(listed_as(&list, "alPHA").map(|r| r.name.as_str()), Some("Alpha"));
        assert!(listed_as(&list, "alpha2").is_none());
    }

    #[test]
    fn a_listed_item_that_is_not_synced_keeps_what_is_owned_but_does_not_count_against_the_empty_guard() {
        let off = || Rejected { name: "alpha".into(), reason: "'examples' is an ignored folder".into() };
        for local in [LocalState::Clean, dirty(), LocalState::FolderMissing] {
            let mut c = Case::new();
            c.owned = vec![(own("alpha", "v1"), local.clone())];
            c.protected = vec![off()];
            assert_eq!(c.only(), Verdict::Skipped(Skip::Protected { reason: off().reason }), "{local:?}");
            c.confirm = true;
            assert_eq!(c.only(), Verdict::Skipped(Skip::Protected { reason: off().reason }), "a confirmation removes nothing listed: {local:?}");
        }
        // Another owned item that is really gone is held: a listed item that is not synced (filtered, or bundled in a
        // package) does not make the listing any less empty for this type.
        let mut c = Case::new();
        c.owned = vec![(own("alpha", "v1"), LocalState::Clean), (own("beta", "v1"), LocalState::Clean)];
        c.protected = vec![off()];
        let kept = ("alpha".to_string(), Verdict::Skipped(Skip::Protected { reason: off().reason }));
        assert_eq!(c.run(), vec![kept.clone(), ("beta".into(), Verdict::Held(Hold::MassDeletion))]);
        c.confirm = true;
        assert_eq!(c.run(), vec![kept, ("beta".into(), Verdict::Delete)]);
        // Nobody owns it: nothing happens and nothing needs attention.
        let mut c = Case::new();
        c.protected = vec![off()];
        assert!(c.run().is_empty());
        assert!(!Verdict::Skipped(Skip::Protected { reason: String::new() }).needs_attention());
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
    fn historical_match_is_an_action_but_never_overrides_user_or_ownership() {
        let mut c = Case::new();
        c.desired = vec![up("alpha", "new")];
        c.unmanaged.insert("alpha".into(), UnmanagedState::Historical);
        assert_eq!(c.only(), Verdict::AdoptHistorical);
        assert!(c.only().is_action());
        c.ignored.insert("alpha".into());
        assert_eq!(c.only(), Verdict::Skipped(Skip::Ignored));
        c.ignored.clear();
        c.other.insert("alpha".into(), "other".into());
        assert_eq!(c.only(), Verdict::Skipped(Skip::Collision { owner: "other".into() }));
        c.interrupted.push("alpha".into());
        assert_eq!(c.only(), Verdict::Held(Hold::Interrupted));
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
    fn an_edit_that_upstream_now_has_too_is_caught_up_instead_of_held() {
        let mut c = Case::new();
        c.desired = vec![up("alpha", "v2")];
        c.owned = vec![(own("alpha", "v1"), dirty())];
        assert!(matches!(c.only(), Verdict::Held(Hold::UpdateBlocked(_))), "held until the folder is known to match");
        c.matching.insert("alpha".into());
        let verdict = c.only();
        assert_eq!(verdict, Verdict::CatchUp);
        assert!(verdict.is_action() && !verdict.needs_attention());
        assert!(verdict.message().contains("in sync"), "{}", verdict.message());
    }

    #[test]
    fn matching_upstream_matters_only_to_an_edited_skill_whose_upstream_moved() {
        let mut c = Case::new();
        c.matching.insert("alpha".into());
        c.desired = vec![up("alpha", "v1")];
        c.owned = vec![(own("alpha", "v1"), dirty())];
        assert!(matches!(c.only(), Verdict::ModifiedLocally(_)), "upstream did not move: still just an edit");
        c.desired = vec![up("alpha", "v2")];
        c.owned = vec![(own("alpha", "v1"), LocalState::Clean)];
        assert_eq!(c.only(), Verdict::Update);
        c.owned = vec![(own("alpha", "v1"), LocalState::FolderMissing)];
        assert_eq!(c.only(), Verdict::Held(Hold::UpdateBlockedMissing));
        c.desired = vec![];
        c.owned = vec![(own("alpha", "v1"), dirty())];
        assert!(matches!(c.only(), Verdict::Held(Hold::DeleteBlocked(_))), "gone upstream: never deleted over an edit");
    }

    #[test]
    fn only_an_edited_skill_whose_upstream_moved_needs_comparing() {
        let old = own("alpha", "v1");
        let new = up("alpha", "v2");
        assert!(needs_comparison(&new, &old, &dirty()));
        assert!(!needs_comparison(&new, &old, &LocalState::Clean));
        assert!(!needs_comparison(&new, &old, &LocalState::FolderMissing));
        assert!(!needs_comparison(&up("alpha", "v1"), &old, &dirty()));
        let mut more = up("alpha", "v1");
        more.files.push(SkillFile { path: "z.md".into(), blob: "0".repeat(40), size: 1 });
        assert!(needs_comparison(&more, &old, &dirty()), "a file added upstream is a move too");
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
        let info = DirtyInfo { modified: vec!["a".into(), "b".into(), "c".into(), "d".into()], missing: vec!["m".into()] };
        let text = Verdict::Held(Hold::UpdateBlocked(info)).message();
        assert!(text.contains("changed: a, b, c and 1 more"), "{text}");
        assert!(text.contains("missing: m"), "{text}");
        assert!(Verdict::Held(Hold::MassDeletion).needs_attention());
        assert!(!Verdict::InSync.needs_attention());
        assert!(Verdict::Update.is_action() && !Verdict::MissingLocally.is_action());
    }
}
