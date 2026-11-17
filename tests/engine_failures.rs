//! Whatever goes wrong, nothing the user owns changes because of it: errors, cancels, limits and folders in use.

mod common;

use std::os::windows::fs::OpenOptionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use tiny_skill_sync::config::RepoConfig;
use tiny_skill_sync::engine::{ApplyResult, Engine, EngineError, Failure, Outcome, RepoReport, Resolution, SyncOptions, Which};
use tiny_skill_sync::github::{FailureKind, Remote, RemoteError, RepoRef, TreeReply};
use tiny_skill_sync::reconcile::Verdict;
use tiny_skill_sync::secret::Secret;

const ALL_KINDS: [FailureKind; 10] = [
    FailureKind::Auth,
    FailureKind::NotFound,
    FailureKind::RateLimited,
    FailureKind::Server,
    FailureKind::Redirect,
    FailureKind::Protocol,
    FailureKind::Network,
    FailureKind::TooLarge,
    FailureKind::Changed,
    FailureKind::Local,
];

fn failure(repo: &RepoReport) -> &Failure {
    match &repo.outcome {
        Outcome::Failed(f) => f,
        other => panic!("expected a failed check, got {other:?}"),
    }
}

fn error(kind: FailureKind) -> RemoteError {
    RemoteError::new(kind, format!("{kind:?}, on purpose"))
}

/// A repository that is checked with the user's token.
fn private(id: &str, name: &str) -> RepoConfig {
    RepoConfig { use_token: true, ..repo(id, name, "skills") }
}

// ---------------------------------------------------------------------------------------------------------------------
// Errors from GitHub
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn no_error_from_github_is_ever_read_as_a_deletion_or_changes_anything() {
    for kind in ALL_KINDS {
        let mut s = Setup::installed(&format!("err-{kind:?}"));
        // Upstream really did remove one skill and change another, so that a wrong reading of the error would show.
        s.remote.remove(REPO, "skills/gamma");
        s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
        s.remote.fail_tree(REPO, Some(error(kind)));
        let before = s.w.snapshot();
        s.advance(3600);

        let report = s.sync();
        let repo = only(&report);

        assert_eq!(failure(repo).kind, kind);
        assert!(repo.skills.is_empty(), "{kind:?}: no decision was made");
        assert_eq!(s.w.snapshot(), before, "{kind:?}: not one file, and not the record, changed");
        let retry = s.engine.schedule().retry_of("fx").expect("the check is tried again later");
        let first_wait = if kind == FailureKind::Changed { 60 } else { 300 };
        assert_eq!((retry.failures, retry.not_before), (1, T0 + 3600 + first_wait), "{kind:?}");

        // The error goes away: what upstream really did is applied now, and only now.
        s.remote.fail_tree(REPO, None);
        let report = s.sync();
        assert_eq!(skill(only(&report), "gamma").result, ApplyResult::Done, "{kind:?}");
        assert_eq!(skill(only(&report), "alpha").result, ApplyResult::Done, "{kind:?}");
        assert!(s.engine.schedule().retry_of("fx").is_none(), "{kind:?}: a success ends the waiting");
    }
}

#[test]
fn a_rate_limit_is_waited_out_for_as_long_as_github_asks() {
    let mut s = Setup::installed("rate");
    s.advance(86_400);
    let until = T0 + 86_400 + 7200;
    s.remote.fail_tree(REPO, Some(RemoteError { retry_at: Some(until), ..error(FailureKind::RateLimited) }));

    let report = sync_which(&mut s.engine, &s.cfg, &Which::Due);
    assert_eq!(failure(only(&report)).retry_at, Some(until));
    assert_eq!(s.engine.next_wake(&s.cfg), Some(until));

    s.advance(7199);
    assert!(sync_which(&mut s.engine, &s.cfg, &Which::Due).repos.is_empty(), "GitHub said wait, so the app waits");
    assert_eq!(s.remote.calls().tree, 1, "not one more request was made");

    s.advance(1);
    s.remote.fail_tree(REPO, None);
    let report = sync_which(&mut s.engine, &s.cfg, &Which::Due);
    assert!(matches!(only(&report).outcome, Outcome::Unchanged), "{:?}", only(&report).outcome);
}

#[test]
fn a_folder_that_vanished_upstream_is_an_error_not_a_deletion() {
    let mut s = Setup::installed("vanished");
    // The repository is still there (README.md, docs/), but `skills/` was renamed or removed.
    s.remote.remove(REPO, "skills");
    let before = s.w.snapshot();

    let report = s.sync();

    assert_eq!(failure(only(&report)).kind, FailureKind::NotFound);
    assert_eq!(s.w.snapshot(), before, "three installed skills are not 'removed upstream' just because their folder is gone");

    // It comes back exactly as it was: the stored ETag still matches, and nothing needs doing.
    s.remote.seed_standard(REPO);
    let report = s.sync();
    assert!(matches!(only(&report).outcome, Outcome::Unchanged), "{:?}", only(&report).outcome);
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
}

#[test]
fn a_listing_that_github_cut_short_is_never_trusted() {
    let mut s = Setup::installed("cut-short");
    // Trusted, this listing would mean 'gamma was removed upstream' and 'alpha changed'.
    s.remote.remove(REPO, "skills/gamma");
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.remote.set_truncated(REPO, true);
    let before = s.w.snapshot();
    s.advance(3600);

    let report = s.sync();

    assert_eq!(failure(only(&report)).kind, FailureKind::TooLarge);
    assert!(only(&report).skills.is_empty(), "no decision was made");
    assert_eq!(s.w.snapshot(), before, "not one file, and not the record, changed");

    // Once the listing is whole again, what upstream really did is applied.
    s.remote.set_truncated(REPO, false);
    let report = s.sync();
    assert_eq!(skill(only(&report), "gamma").result, ApplyResult::Done);
    assert_eq!(skill(only(&report), "alpha").result, ApplyResult::Done);
    assert_eq!(s.w.skill_names(), ["alpha", "beta"]);
}

#[test]
fn an_overwrite_that_goes_wrong_leaves_the_edited_skill_as_it_was() {
    type Break = fn(&FakeRemote);
    let cases: [(&str, Break, &str); 5] = [
        ("cut-short", |r| r.set_truncated(REPO, true), "cut the listing short"),
        ("listing-error", |r| r.fail_tree(REPO, Some(error(FailureKind::Network))), "Network"),
        ("gone-upstream", |r| r.remove(REPO, "skills/alpha"), "cannot be installed"),
        ("no-commit", |r| r.fail_commit(REPO, Some(error(FailureKind::Server))), "Server"),
        ("file-fails", |r| r.fail_file(REPO, "skills/alpha/references/guide.md", Some(error(FailureKind::Network))), "Network"),
    ];
    for (name, break_it, words) in cases {
        let mut s = Setup::installed(&format!("overwrite-{name}"));
        s.w.write("alpha/SKILL.md", "my own words\n");
        break_it(&s.remote);
        let before = s.w.snapshot();

        let err = s.engine.resolve(&s.cfg, "fx", "alpha", Resolution::Overwrite, &cancel_flag()).unwrap_err();

        assert!(err.to_string().contains(words), "{name}: {err}");
        assert_eq!(s.w.snapshot(), before, "{name}: the edited skill, the record and the work folder are as they were");
    }
}

#[test]
fn losing_access_to_a_private_repository_removes_nothing() {
    let w = World::new("revoked");
    let remote = FakeRemote::new();
    remote.seed_standard("o/priv");
    remote.make_private("o/priv", "good");
    let tokens = FakeTokens::some("good");
    let mut cfg = config(vec![private("p", "o/priv")]);
    let mut engine = w.engine_with(&remote, &tokens);
    assert!(matches!(only(&sync_all(&mut engine, &cfg)).outcome, Outcome::Synced));
    let before = w.snapshot();

    // The token stops being accepted.
    remote.make_private("o/priv", "a different token");
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(failure(only(&report)).kind, FailureKind::Auth);
    assert_eq!(w.snapshot(), before);

    // Without any token GitHub answers 'not found' for a private repository.
    cfg.repos[0].use_token = false;
    let mut engine = w.engine_with(&remote, &FakeTokens::none());
    let report = sync_all(&mut engine, &cfg);
    assert_eq!(failure(only(&report)).kind, FailureKind::NotFound);
    assert_eq!(w.snapshot(), before);
}

// ---------------------------------------------------------------------------------------------------------------------
// Failures in the middle of a download
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_download_that_fails_halfway_installs_nothing_partial_and_the_next_check_finishes_the_job() {
    let mut s = Setup::fresh("halfway");
    s.remote.fail_file(REPO, "skills/beta/SKILL.md", Some(error(FailureKind::Network)));

    let report = s.sync();
    let repo = only(&report);

    assert_eq!(failure(repo).kind, FailureKind::Network);
    assert_eq!(skill(repo, "alpha").result, ApplyResult::Done, "what was complete before the failure stays");
    assert!(matches!(skill(repo, "beta").result, ApplyResult::Failed(_)));
    assert_eq!(skill(repo, "gamma").result, ApplyResult::NotTried, "after a network problem the rest is not attempted");
    assert_eq!(s.w.skill_names(), ["alpha"]);
    assert_eq!(s.w.files_of("alpha").len(), 3, "alpha is whole");
    assert!(s.w.work_leftovers().is_empty(), "{:?}", s.w.work_leftovers());
    let state = s.w.state();
    let rs = state.repo("fx").unwrap();
    assert_eq!(rs.skills.len(), 1);
    assert!(rs.etag.is_none(), "unfinished, so the next check must look at the listing again");
    assert!(rs.last_check.is_none());

    s.remote.fail_file(REPO, "skills/beta/SKILL.md", None);
    s.advance(300);
    s.remote.reset();
    let report = s.sync();
    let repo = only(&report);
    assert!(matches!(repo.outcome, Outcome::Synced), "{:?}", repo.outcome);
    assert_eq!(skill(repo, "alpha").verdict, Verdict::InSync);
    assert_eq!(skill(repo, "beta").result, ApplyResult::Done);
    assert_eq!(skill(repo, "gamma").result, ApplyResult::Done);
    assert_eq!(s.remote.calls().conditional, 0);
    assert!(s.w.work_leftovers().is_empty());
}

#[test]
fn a_problem_with_one_skill_on_this_computer_does_not_stop_the_others() {
    let mut s = Setup::fresh("local");
    // What a full disk or a locked file looks like to the engine.
    s.remote.fail_file(REPO, "skills/beta/SKILL.md", Some(error(FailureKind::Local)));

    let report = s.sync();
    let repo = only(&report);

    assert_eq!(failure(repo).kind, FailureKind::Local);
    assert!(failure(repo).message.contains("beta"), "{}", failure(repo).message);
    assert_eq!(skill(repo, "alpha").result, ApplyResult::Done);
    assert!(matches!(skill(repo, "beta").result, ApplyResult::Failed(_)));
    assert_eq!(skill(repo, "gamma").result, ApplyResult::Done);
    assert_eq!(s.w.skill_names(), ["alpha", "gamma"]);

    s.remote.fail_file(REPO, "skills/beta/SKILL.md", None);
    let report = s.sync();
    assert_eq!(skill(only(&report), "beta").result, ApplyResult::Done);
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::InSync);
}

#[test]
fn a_file_that_does_not_match_the_listing_is_never_installed() {
    let mut s = Setup::fresh("lie");
    // The same length but other bytes: only the hash can tell. This is what a push between listing and download is.
    s.remote.lie_about(REPO, "skills/alpha/references/guide.md", "tampered\n");

    let report = s.sync();

    assert_eq!(failure(only(&report)).kind, FailureKind::Changed);
    assert!(s.w.skill_names().is_empty(), "{:?}", s.w.skill_names());
    assert!(s.w.work_leftovers().is_empty(), "{:?}", s.w.work_leftovers());
    assert!(s.w.state().repo("fx").is_none_or(|r| r.skills.is_empty()));
    assert_eq!(s.engine.schedule().retry_of("fx").unwrap().not_before, T0 + 60, "a repository that changed under us is retried soon");

    // The push has landed: the listing and the files agree again.
    s.remote.put(REPO, "skills/alpha/references/guide.md", "tampered\n");
    s.advance(60);
    let report = s.sync();
    assert!(matches!(only(&report).outcome, Outcome::Synced), "{:?}", only(&report).outcome);
    assert_eq!(s.w.read("alpha/references/guide.md"), "tampered\n");
}

// ---------------------------------------------------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_repository_that_wants_a_token_fails_before_any_request_when_there_is_none() {
    let w = World::new("no-token");
    let remote = FakeRemote::new();
    remote.seed_standard(REPO);
    let tokens = FakeTokens::none();
    let mut engine = w.engine_with(&remote, &tokens);

    let report = sync_all(&mut engine, &config(vec![private("fx", REPO)]));

    let f = failure(only(&report));
    assert_eq!(f.kind, FailureKind::Auth);
    assert!(f.message.contains("token"), "{}", f.message);
    assert_eq!(remote.calls(), Calls::default(), "not a single request was sent");
    assert_eq!(tokens.asked(), 1);
    assert!(w.skill_names().is_empty());
}

#[test]
fn the_token_goes_only_to_the_repositories_that_ask_for_it_and_is_looked_up_once() {
    let w = World::new("token-scope");
    let remote = FakeRemote::new();
    remote.seed("o/pub", &[("skills/alpha/SKILL.md", "public alpha\n")]);
    remote.seed("o/priv", &[("skills/secret/SKILL.md", "private skill\n")]);
    remote.make_private("o/priv", "tok");
    let tokens = FakeTokens::some("tok");
    let cfg = config(vec![repo("pub", "o/pub", "skills"), private("priv", "o/priv")]);
    let mut engine = w.engine_with(&remote, &tokens);

    let report = sync_all(&mut engine, &cfg);

    assert_eq!(report.repos.len(), 2);
    assert!(report.repos.iter().all(|r| matches!(r.outcome, Outcome::Synced)), "{report:?}");
    assert_eq!(w.skill_names(), ["alpha", "secret"]);
    assert_eq!(tokens.asked(), 1, "looked up once per check, not once per request");
    let seen: Vec<(String, Option<String>)> = remote.log().into_iter().zip(remote.tokens_seen()).map(|(line, (_, token))| (line, token)).collect();
    assert!(seen.iter().any(|(line, _)| line.contains("o/pub")) && seen.iter().any(|(line, _)| line.contains("o/priv")));
    for (line, token) in &seen {
        if line.contains("o/priv") {
            assert_eq!(token.as_deref(), Some("tok"), "{line}");
        } else {
            assert_eq!(*token, None, "a public repository never receives the token: {line}");
        }
    }
}

#[test]
fn the_token_never_reaches_a_file_or_a_report() {
    const SECRET: &str = "ghp_0123456789abcdefSECRETSECRET";
    let w = World::new("secret");
    let remote = FakeRemote::new();
    remote.seed_standard(REPO);
    remote.make_private(REPO, SECRET);
    let tokens = FakeTokens::some(SECRET);
    let cfg = config(vec![private("fx", REPO)]);
    let mut engine = w.engine_with(&remote, &tokens);

    let first = sync_all(&mut engine, &cfg);
    remote.make_private(REPO, "something else");
    let second = sync_all(&mut engine, &cfg);
    engine.flush().unwrap();

    assert!(remote.tokens_seen().iter().any(|(_, t)| t.as_deref() == Some(SECRET)), "the token was in use");
    for (path, bytes) in w.snapshot() {
        assert!(!String::from_utf8_lossy(&bytes).contains(SECRET), "the token was written to {path}");
    }
    assert!(!format!("{first:?}{second:?}").contains(SECRET));
}

fn tree_without_token() -> (String, Option<String>) {
    ("tree".to_string(), None)
}

fn with_token(kind: &str, token: &str) -> (String, Option<String>) {
    (kind.to_string(), Some(token.to_string()))
}

#[test]
fn a_private_repository_is_read_with_the_token_without_being_told_and_that_is_remembered() {
    let w = World::new("auto-token");
    let remote = FakeRemote::new();
    remote.seed_standard(REPO);
    remote.make_private(REPO, "tok");
    let tokens = FakeTokens::some("tok");
    let cfg = standard_config();
    let mut engine = w.engine_with(&remote, &tokens);

    let report = sync_all(&mut engine, &cfg);

    assert!(matches!(only(&report).outcome, Outcome::Synced), "{:?}", only(&report).outcome);
    assert_eq!(w.skill_names(), ["alpha", "beta", "gamma"]);
    let seen = remote.tokens_seen();
    assert_eq!(seen[0], tree_without_token(), "the first try is without the token");
    assert_eq!(seen[1], with_token("tree", "tok"), "GitHub said 'not found', so the token is tried");
    assert!(seen[2..].iter().all(|(_, t)| t.as_deref() == Some("tok")), "the downloads use it too: {seen:?}");
    assert!(w.state().repo("fx").unwrap().needs_token, "remembered on disk");
    assert!(engine.needs_token("fx"));

    // The next check, even after a restart, goes straight to the token: one request, not two.
    remote.reset();
    let mut engine = w.engine_with(&remote, &tokens);
    let report = sync_all(&mut engine, &cfg);
    assert!(matches!(only(&report).outcome, Outcome::Unchanged), "{:?}", only(&report).outcome);
    assert_eq!(remote.tokens_seen(), [with_token("tree", "tok")]);
}

#[test]
fn a_public_repository_never_looks_for_the_token() {
    let w = World::new("public-no-token");
    let remote = FakeRemote::new();
    remote.seed_standard(REPO);
    let tokens = FakeTokens::some("tok");
    let mut engine = w.engine_with(&remote, &tokens);

    let report = sync_all(&mut engine, &standard_config());

    assert!(matches!(only(&report).outcome, Outcome::Synced), "{:?}", only(&report).outcome);
    assert_eq!(tokens.asked(), 0, "not looked up, so the GitHub CLI is not started");
    assert!(remote.tokens_seen().iter().all(|(_, t)| t.is_none()));
    assert!(!w.state().repo("fx").unwrap().needs_token);

    // Only 'not found' and the limit for anonymous requests are worth trying with the token; any other error is not.
    remote.fail_tree(REPO, Some(error(FailureKind::Server)));
    remote.reset();
    let report = sync_all(&mut engine, &standard_config());
    assert_eq!(failure(only(&report)).kind, FailureKind::Server);
    assert_eq!(remote.tokens_seen(), [tree_without_token()]);
    assert_eq!(tokens.asked(), 0);
}

#[test]
fn a_private_repository_without_any_sign_in_says_how_to_sign_in_and_changes_nothing() {
    let w = World::new("auto-no-token");
    let remote = FakeRemote::new();
    remote.seed_standard(REPO);
    remote.make_private(REPO, "tok");
    let tokens = FakeTokens::none();
    let mut engine = w.engine_with(&remote, &tokens);

    let report = sync_all(&mut engine, &standard_config());

    let f = failure(only(&report));
    assert_eq!(f.kind, FailureKind::NotFound, "still 'not found': the app cannot tell a private repository from a missing one");
    assert!(f.message.contains("gh auth login") && f.message.contains("save a token in Settings"), "{}", f.message);
    assert_eq!(remote.tokens_seen(), [tree_without_token()]);
    assert_eq!(tokens.asked(), 1);
    assert!(w.skill_names().is_empty());
    assert!(!engine.needs_token("fx"));
}

#[test]
fn a_limit_on_anonymous_requests_is_got_round_with_the_token_but_does_not_make_the_repository_private() {
    let w = World::new("auto-limit");
    let remote = FakeRemote::new();
    remote.seed_standard(REPO);
    remote.fail_anonymous(REPO, Some(error(FailureKind::RateLimited)));
    let tokens = FakeTokens::some("tok");
    let mut engine = w.engine_with(&remote, &tokens);

    let report = sync_all(&mut engine, &standard_config());

    assert!(matches!(only(&report).outcome, Outcome::Synced), "{:?}", only(&report).outcome);
    assert_eq!(&remote.tokens_seen()[..2], [tree_without_token(), with_token("tree", "tok")]);
    assert!(!w.state().repo("fx").unwrap().needs_token, "a public repository is not remembered as private");

    // Without a token the limit is waited out as before.
    let mut engine = w.engine_with(&remote, &FakeTokens::none());
    let report = sync_with(&mut engine, &standard_config(), SyncOptions { ignore_etag: true, ..SyncOptions::default() });
    assert_eq!(failure(only(&report)).kind, FailureKind::RateLimited);
}

#[test]
fn a_remembered_private_repository_is_tried_without_the_token_once_there_is_none() {
    let w = World::new("auto-forget");
    let remote = FakeRemote::new();
    remote.seed_standard(REPO);
    remote.make_private(REPO, "tok");
    let cfg = standard_config();
    sync_all(&mut w.engine_with(&remote, &FakeTokens::some("tok")), &cfg);
    assert!(w.state().repo("fx").unwrap().needs_token);
    let before = w.snapshot();

    // The user signed out while it is still private: the check fails and says how to sign in, and changes nothing.
    let tokens = FakeTokens::none();
    let mut engine = w.engine_with(&remote, &tokens);
    let f = failure(only(&sync_with(&mut engine, &cfg, SyncOptions { ignore_etag: true, ..SyncOptions::default() }))).clone();
    assert_eq!(f.kind, FailureKind::NotFound);
    assert!(f.message.contains("gh auth login"), "{}", f.message);
    assert_eq!(w.snapshot(), before, "nothing changed, and it is still remembered as private");

    // It was made public: read without a token, and no longer remembered as private.
    remote.make_public(REPO);
    remote.reset();
    let report = sync_all(&mut engine, &cfg);
    assert!(matches!(only(&report).outcome, Outcome::Unchanged), "{:?}", only(&report).outcome);
    assert!(remote.tokens_seen().iter().all(|(_, t)| t.is_none()));
    assert!(!w.state().repo("fx").unwrap().needs_token);
    assert_eq!(w.skill_names(), ["alpha", "beta", "gamma"]);
}

#[test]
fn replacing_an_edited_skill_of_a_private_repository_uses_the_token() {
    let w = World::new("auto-overwrite");
    let remote = FakeRemote::new();
    remote.seed_standard(REPO);
    remote.make_private(REPO, "tok");
    let cfg = standard_config();
    let mut engine = w.engine_with(&remote, &FakeTokens::some("tok"));
    sync_all(&mut engine, &cfg);
    w.write("alpha/SKILL.md", "my own words\n");
    remote.reset();

    let report = engine.resolve(&cfg, "fx", "alpha", Resolution::Overwrite, &cancel_flag()).expect("replaced");

    assert!(report.message.contains("now matches"), "{}", report.message);
    assert_eq!(w.read("alpha/SKILL.md"), ALPHA_V1);
    assert!(remote.tokens_seen().iter().all(|(_, t)| t.as_deref() == Some("tok")), "{:?}", remote.tokens_seen());
}

#[test]
fn a_dry_run_reads_a_private_repository_with_the_token_but_remembers_nothing() {
    let w = World::new("auto-dry");
    let remote = FakeRemote::new();
    remote.seed_standard(REPO);
    remote.make_private(REPO, "tok");
    let mut engine = w.engine_with(&remote, &FakeTokens::some("tok"));
    let before = w.snapshot();

    let report = sync_with(&mut engine, &standard_config(), SyncOptions { dry_run: true, ..SyncOptions::default() });

    assert!(matches!(only(&report).outcome, Outcome::WouldChange), "{:?}", only(&report).outcome);
    assert_eq!(w.snapshot(), before);
    assert!(!engine.needs_token("fx"), "not even in memory");
}

// ---------------------------------------------------------------------------------------------------------------------
// Cancelling
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_cancel_before_the_check_starts_does_nothing() {
    let mut s = Setup::fresh("cancel-first");
    let cancel = AtomicBool::new(true);

    let report = s.engine.sync(&s.cfg, &Which::All, &SyncOptions::default(), &cancel).unwrap();

    assert!(report.repos.is_empty());
    assert_eq!(s.remote.calls(), Calls::default());
    assert!(s.w.skill_names().is_empty());
}

#[test]
fn a_cancel_in_the_middle_of_a_download_leaves_only_whole_skills() {
    let mut s = Setup::fresh("cancel-mid");
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    s.remote.on_call(move |line| {
        if line.ends_with("skills/beta/SKILL.md") {
            flag.store(true, Ordering::Relaxed);
        }
    });

    let report = s.engine.sync(&s.cfg, &Which::All, &SyncOptions::default(), &cancel).unwrap();
    let repo = only(&report);

    assert!(matches!(repo.outcome, Outcome::Cancelled), "{:?}", repo.outcome);
    assert_eq!(skill(repo, "alpha").result, ApplyResult::Done);
    assert_eq!(skill(repo, "beta").result, ApplyResult::NotTried);
    assert_eq!(skill(repo, "gamma").result, ApplyResult::NotTried);
    assert_eq!(s.w.skill_names(), ["alpha"]);
    assert_eq!(s.w.files_of("alpha").len(), 3, "the one that was installed is whole");
    assert!(s.w.work_leftovers().is_empty(), "{:?}", s.w.work_leftovers());
    assert!(s.engine.schedule().retry_of("fx").is_none(), "a cancel is not a failure, so nothing is backed off");
    assert!(s.w.state().repo("fx").unwrap().etag.is_none(), "unfinished, so the next check looks again");
    assert!(s.w.state().repo("fx").unwrap().last_check.is_none(), "a cancelled check does not count as a check");

    s.remote.clear_hook();
    let report = sync_which(&mut s.engine, &s.cfg, &Which::Due);
    assert_eq!(report.repos.len(), 1, "still due, so the next tick finishes the job");
    assert_eq!(skill(only(&report), "alpha").verdict, Verdict::InSync);
    assert_eq!(skill(only(&report), "beta").result, ApplyResult::Done);
    assert_eq!(skill(only(&report), "gamma").result, ApplyResult::Done);
}

#[test]
fn a_cancel_while_listing_is_not_a_failure() {
    let mut s = Setup::installed("cancel-list");
    s.remote.fail_tree(REPO, Some(error(FailureKind::Cancelled)));
    let before = s.w.snapshot();

    let report = s.sync();

    assert!(matches!(only(&report).outcome, Outcome::Cancelled), "{:?}", only(&report).outcome);
    assert_eq!(s.w.snapshot(), before);
    assert!(s.engine.schedule().retry_of("fx").is_none());
}

// ---------------------------------------------------------------------------------------------------------------------
// Limits and odd answers
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_check_stops_at_the_download_limit_and_the_rest_waits_for_the_next_one() {
    let mut s = Setup::fresh("cap");

    let report = sync_with(&mut s.engine, &s.cfg, SyncOptions { max_download_bytes: 100, ..SyncOptions::default() });
    let repo = only(&report);

    assert_eq!(skill(repo, "alpha").result, ApplyResult::Done, "it fits");
    for name in ["beta", "gamma"] {
        assert!(matches!(&skill(repo, name).result, ApplyResult::Held(why) if why.contains("limit")), "{name}: {:?}", skill(repo, name).result);
    }
    assert!(matches!(repo.outcome, Outcome::NeedsAttention), "{:?}", repo.outcome);
    let alpha_bytes: usize = s.w.files_of("alpha").values().map(String::len).sum();
    assert_eq!(repo.downloaded, alpha_bytes as u64);
    assert_eq!(s.w.skill_names(), ["alpha"]);

    let report = s.sync();
    assert_eq!(skill(only(&report), "beta").result, ApplyResult::Done);
    assert_eq!(skill(only(&report), "gamma").result, ApplyResult::Done);
}

struct AlwaysNotModified;

impl Remote for AlwaysNotModified {
    fn tree(&self, _: &RepoRef, _: Option<&str>, _: Option<&Secret>, _: &AtomicBool) -> Result<TreeReply, RemoteError> {
        Ok(TreeReply::NotModified)
    }

    fn commit(&self, _: &RepoRef, _: Option<&Secret>, _: &AtomicBool) -> Result<String, RemoteError> {
        Err(error(FailureKind::Protocol))
    }

    fn file(&self, _: &RepoRef, _: &str, _: &str, _: u64, _: Option<&Secret>, _: &AtomicBool, _: &mut dyn FnMut(&[u8]) -> bool) -> Result<(), RemoteError> {
        Err(error(FailureKind::Protocol))
    }
}

#[test]
fn a_not_modified_answer_to_a_request_without_an_etag_is_a_protocol_error() {
    let w = World::new("304");
    let mut engine = Engine::new(w.paths(), Box::new(AlwaysNotModified), FakeTokens::none().provider());

    let report = sync_all(&mut engine, &standard_config());

    assert_eq!(failure(only(&report)).kind, FailureKind::Protocol);
    assert!(w.skill_names().is_empty());
    assert!(!w.state_file().exists(), "nothing was learned, so nothing was recorded");
}

#[test]
fn an_unknown_repository_is_an_error_and_a_paused_one_is_checked_only_by_name() {
    let mut s = Setup::fresh("which");
    let err = s.engine.sync(&s.cfg, &Which::One("nope".into()), &SyncOptions::default(), &cancel_flag()).unwrap_err();
    assert!(matches!(&err, EngineError::Failed(why) if why.contains("nope")), "{err}");
    assert_eq!(s.remote.calls(), Calls::default());

    s.cfg.repos[0].enabled = false;
    assert!(sync_which(&mut s.engine, &s.cfg, &Which::All).repos.is_empty(), "a paused repository is not part of 'check all'");
    assert!(sync_which(&mut s.engine, &s.cfg, &Which::Due).repos.is_empty());
    let report = sync_which(&mut s.engine, &s.cfg, &Which::One("fx".into()));
    assert!(matches!(only(&report).outcome, Outcome::Synced), "{:?}", only(&report).outcome);
}

// ---------------------------------------------------------------------------------------------------------------------
// Another program is using the folder
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_skill_folder_in_use_by_another_program_is_not_replaced_and_is_updated_later() {
    let mut s = Setup::installed("in-use");
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    // Opening a folder without FILE_SHARE_DELETE is what makes Windows refuse to rename it.
    let holder = std::fs::OpenOptions::new().read(true).custom_flags(0x0200_0000).share_mode(1 | 2).open(s.w.skills.join("alpha")).unwrap();
    let before = s.w.skills_snapshot();

    let report = s.sync();
    let repo = only(&report);

    assert!(matches!(&skill(repo, "alpha").result, ApplyResult::Held(why) if why.contains("in use")), "{:?}", skill(repo, "alpha").result);
    assert!(matches!(repo.outcome, Outcome::NeedsAttention), "{:?}", repo.outcome);
    assert_eq!(s.w.skills_snapshot(), before);
    assert!(s.w.work_leftovers().is_empty(), "{:?}", s.w.work_leftovers());

    drop(holder);
    let report = s.sync();
    assert_eq!(skill(only(&report), "alpha").result, ApplyResult::Done);
    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V2);
}
