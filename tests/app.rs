//! The tray app's cycle without its windows: an `App` hands orders to a real engine on a fake GitHub and takes the
//! answers back, as the window and the worker thread do.

mod common;

use std::path::PathBuf;

use common::*;
use tiny_skill_sync::app::{self, Answer, App, BUSY_RETRY, Done, FAILURE_BACKOFF, Job, MIN_POLL_GAP, RESUME_GRACE, Settled};
use tiny_skill_sync::board::{Action, Health, Level, Row};
use tiny_skill_sync::config::Config;
use tiny_skill_sync::engine::{self, Engine, EngineError, Resolution, SyncReport};
use tiny_skill_sync::reconcile::Verdict;

const DAY: i64 = 24 * 60 * 60;

/// Runs every waiting job to the end. Returns what each one settled.
fn run(app: &mut App, engine: &mut Engine, now: i64) -> Vec<(Job, Settled)> {
    let mut out = Vec::new();
    while let Some(order) = app.next_order() {
        let done = app::perform(engine, &order, &cancel_flag());
        out.push((order.job, app.finish(done, now)));
    }
    out
}

/// What the last job settled.
fn last(mut settled: Vec<(Job, Settled)>) -> Settled {
    settled.pop().expect("a job ran").1
}

/// Answers the next job without an engine, as an engine that fails would.
fn answer(app: &mut App, result: Result<Answer, EngineError>, next_wake: Option<i64>, now: i64) -> Settled {
    let order = app.next_order().expect("a job is waiting");
    app.finish(Done { job: order.job, result, next_wake, last_checks: Vec::new(), kept: Vec::new(), stamp: None }, now)
}

fn broken() -> Result<Answer, EngineError> {
    Err(EngineError::Failed("cannot read state.json".to_string()))
}

fn checked_nothing() -> Result<Answer, EngineError> {
    Ok(Answer::Checked { verified: None, sync: SyncReport { repos: Vec::new(), notes: Vec::new(), warnings: Vec::new() } })
}

fn app_for(s: &Setup) -> App {
    App::new(s.cfg.clone(), s.w.skills.clone())
}

/// No engine behind it: for the rules about the queue and the timer.
fn bare() -> App {
    App::new(standard_config(), PathBuf::from("not used"))
}

/// The app has run its startup job.
fn started(s: &mut Setup) -> App {
    let mut app = app_for(s);
    run(&mut app, &mut s.engine, T0);
    assert!(app.started());
    app
}

fn row(app: &App, name: &str) -> Row {
    let view = app.board.views(app.cfg(), T0).remove(0);
    view.skills.into_iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no row for {name}"))
}

fn fx() -> String {
    "fx".to_string()
}

#[test]
fn the_first_start_looks_then_checks_then_sleeps_until_the_next_check_is_due() {
    let mut s = Setup::fresh("app-first");
    let mut app = app_for(&s);
    assert_eq!(app.queued(), &[Job::Startup]);
    assert_eq!(app.arm(T0), None, "the first job does not wait for a timer");

    let order = app.next_order().unwrap();
    assert_eq!(app.health(), Health::Working);
    let done = app::perform(&mut s.engine, &order, &cancel_flag());
    assert_eq!(app.finish(done, T0), Settled::default());
    assert!(app.started());
    assert_eq!(app.health(), Health::Ok, "never having checked is not a problem");
    assert_eq!(app.arm(T0), Some(T0), "a repository that was never checked is due now");

    app.on_timer(T0);
    assert_eq!(app.queued(), &[Job::Poll]);
    assert_eq!(last(run(&mut app, &mut s.engine, T0)), Settled::default(), "the timer's checks are not reported in the status line");
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);
    assert_eq!(app.tooltip(), "Tiny Skill Sync: all skills are up to date");
    assert_eq!(row(&app, "alpha").text, Verdict::InSync.message());
    assert_eq!(app.board.views(app.cfg(), T0)[0].last_check, Some(T0));
    assert_eq!(app.arm(T0), Some(T0 + DAY), "the next check is a day away");

    app.on_timer(T0 + 10);
    assert!(app.queued().is_empty(), "a timer that fires early (the clock was changed) checks nothing");
}

#[test]
fn a_job_that_is_already_waiting_covers_a_smaller_one() {
    let mut app = bare();
    assert!(!app.want(Job::Look), "starting up looks at the disk anyway");
    assert!(app.want(Job::Poll));
    assert!(!app.want(Job::Poll), "the same job twice is one job");
    assert!(app.want(Job::Check(Some(fx()))));
    assert!(app.want(Job::Check(None)));
    assert_eq!(app.queued(), &[Job::Startup, Job::Check(None)], "checking everything covers the rest");

    assert!(!app.want(Job::Poll));
    assert!(!app.want(Job::Check(Some(fx()))));
    assert!(!app.want(Job::Look));
    assert!(!app.want(Job::Plan), "every job works out when to wake up");
    let keep = Job::Resolve { repo: fx(), skill: "alpha".to_string(), how: Resolution::KeepMine };
    assert!(app.want(keep.clone()), "a choice the user made is never covered");
    assert_eq!(app.queued(), &[Job::Startup, Job::Check(None), keep]);

    let mut app = bare();
    app.next_order();
    assert!(app.want(Job::Look), "the look of the startup job is already under way");
    assert!(!app.want(Job::Look));
    assert!(app.want(Job::Check(Some(fx()))));
    assert!(!app.want(Job::Look), "a check looks at the disk first");
    assert!(app.want(Job::Startup), "a start that has to be tried again");
    assert_eq!(app.queued(), &[Job::Startup, Job::Look, Job::Check(Some(fx()))], "goes first");
}

#[test]
fn quitting_drops_what_is_waiting_and_saves_last() {
    let mut app = bare();
    app.want(Job::Check(None));
    app.quit();
    assert_eq!(app.queued(), &[Job::Flush]);
    assert!(!app.want(Job::Check(None)), "nothing new starts");
    assert_eq!(app.arm(T0), None);

    let order = app.next_order().unwrap();
    assert_ne!(app.health(), Health::Working, "saving is not checking");
    let done = Done { job: order.job, result: Ok(Answer::Flushed), next_wake: Some(T0), last_checks: Vec::new(), kept: Vec::new(), stamp: None };
    app.finish(done, T0);
    assert!(app.is_idle());
    assert_eq!(app.arm(T0), None, "no timer once quitting");
    app.on_timer(T0);
    assert!(app.queued().is_empty());
}

#[test]
fn a_busy_engine_is_tried_again_later_or_told_to_the_user() {
    let mut app = bare();
    app.want(Job::Check(None));
    assert_eq!(answer(&mut app, Err(EngineError::Busy), None, T0), Settled::default(), "starting up");
    assert!(!app.started());

    let told = answer(&mut app, Err(EngineError::Busy), None, T0);
    assert_eq!(told.feedback.as_deref(), Some("Another check is running right now; try again in a moment"));
    assert_eq!(app.board.messages().count(), 0, "the status line says it; it is not news worth keeping");

    assert!(app.want(Job::Look));
    assert_eq!(answer(&mut app, Err(EngineError::Busy), None, T0), Settled::default(), "the other program's change brings its own look");
    assert!(app.queued().is_empty());

    assert_eq!(app.arm(T0), Some(T0 + BUSY_RETRY), "starting up is tried again in a minute");
    app.on_timer(T0 + 10);
    assert!(app.queued().is_empty(), "not before its time");
    app.arm(T0 + 10);
    app.on_timer(T0 + BUSY_RETRY);
    assert_eq!(app.queued(), &[Job::Startup]);

    let mut s = Setup::fresh("app-busy-poll");
    let mut app = started(&mut s);
    app.on_timer(T0);
    assert_eq!(answer(&mut app, Err(EngineError::Busy), Some(T0), T0), Settled::default());
    assert_eq!(app.arm(T0), Some(T0 + BUSY_RETRY), "the timer's check too");
}

#[test]
fn an_engine_that_cannot_work_is_an_error_and_is_tried_less_often_until_it_works() {
    let mut s = Setup::fresh("app-broken");
    let mut app = started(&mut s);
    app.on_timer(T0);
    assert_eq!(answer(&mut app, broken(), Some(T0), T0), Settled::default());
    assert_eq!(app.health(), Health::Error);
    assert_eq!(app.tooltip(), "Tiny Skill Sync: cannot read state.json");
    assert_eq!(app.arm(T0), Some(T0 + FAILURE_BACKOFF), "still due, but not in a loop");

    let mut now = T0 + FAILURE_BACKOFF;
    for wait in [600, 1200, 2400, 3600, 3600] {
        app.on_timer(now);
        assert_eq!(app.queued(), &[Job::Poll]);
        answer(&mut app, broken(), Some(T0), now);
        assert_eq!(app.arm(now), Some(now + wait));
        now += wait;
    }
    let errors = app.board.messages().filter(|m| m.level == Level::Error).count();
    assert_eq!(errors, 1, "told once, not at every try");

    app.on_timer(now);
    run(&mut app, &mut s.engine, now);
    assert_eq!((app.health(), app.board.failure()), (Health::Ok, None), "it works again");
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);

    app.want(Job::Look);
    answer(&mut app, broken(), Some(now), now);
    assert_eq!(app.arm(now), Some(now + FAILURE_BACKOFF), "the count started again");

    app.want(Job::Check(None));
    let told = answer(&mut app, Err(EngineError::Failed("disk full".to_string())), Some(now), now);
    assert_eq!(told.feedback.as_deref(), Some("disk full"), "the user asked, so the user is told");
    assert_eq!(app.board.messages().last().map(|m| (m.level, m.text.as_str())), Some((Level::Error, "disk full")));
    assert_eq!(app.arm(now), Some(now + FAILURE_BACKOFF), "and the timer is not pushed back for it");
}

#[test]
fn two_checks_by_the_timer_are_never_right_after_each_other() {
    let mut s = Setup::fresh("app-gap");
    let mut app = started(&mut s);
    assert_eq!(app.arm(T0), Some(T0), "no gap after starting up");

    app.on_timer(T0);
    answer(&mut app, checked_nothing(), Some(T0), T0);
    assert_eq!(app.arm(T0), Some(T0 + MIN_POLL_GAP), "whatever the engine says is due");

    app.want(Job::Check(None));
    answer(&mut app, checked_nothing(), Some(T0), T0 + MIN_POLL_GAP);
    assert_eq!(app.arm(T0 + MIN_POLL_GAP), Some(T0 + MIN_POLL_GAP), "a check the user asked for adds no gap");
}

#[test]
fn a_timer_that_fires_long_after_its_time_waits_for_the_network() {
    let mut s = Setup::fresh("app-late");
    let mut app = started(&mut s);
    app.on_timer(T0);
    run(&mut app, &mut s.engine, T0);
    assert_eq!(app.arm(T0), Some(T0 + DAY));

    let woke = T0 + DAY + 61;
    app.on_timer(woke);
    assert!(app.queued().is_empty(), "the computer slept through the check");
    assert_eq!(app.arm(woke), Some(woke + RESUME_GRACE));
    app.on_timer(woke + RESUME_GRACE);
    assert_eq!(app.queued(), &[Job::Poll]);

    let mut app = started(&mut s);
    app.on_timer(T0);
    run(&mut app, &mut s.engine, T0);
    app.arm(T0);
    app.on_timer(T0 + DAY + 60);
    assert_eq!(app.queued(), &[Job::Poll], "a minute late is on time");

    let mut app = started(&mut s);
    let woke = T0 + DAY + 5;
    app.resumed(woke);
    assert_eq!(app.arm(woke), Some(woke + RESUME_GRACE), "waking up says the same");
}

#[test]
fn a_change_by_another_program_is_noticed_from_the_stamp_and_looked_at() {
    let mut s = Setup::installed("app-foreign");
    let mut app = started(&mut s);
    let file = s.w.state_file();
    assert_eq!(engine::stamp_of(&file), app.state_stamp(), "nothing changed since the last job");
    assert!(app.state_stamp().is_some());

    // `tss resolve fx alpha keep`
    let mut tss = s.restarted();
    tss.resolve(&s.cfg, "fx", "alpha", Resolution::KeepMine, &cancel_flag()).unwrap();
    assert_ne!(engine::stamp_of(&file), app.state_stamp());

    assert!(app.want(Job::Look));
    let order = app.next_order().unwrap();
    assert_ne!(app.health(), Health::Working, "a look is too quick to show");
    let done = app::perform(&mut s.engine, &order, &cancel_flag());
    app.finish(done, T0);
    assert_eq!(row(&app, "alpha").text, "Kept as yours");
    assert_eq!(row(&app, "alpha").actions, [Action::ManageAgain]);
    assert_eq!(engine::stamp_of(&file), app.state_stamp(), "seen");
}

#[test]
fn a_conflict_is_shown_and_settled_as_the_user_chose() {
    let mut s = Setup::installed("app-resolve");
    let mut app = started(&mut s);
    s.remote.put(REPO, "skills/alpha/SKILL.md", ALPHA_V2);
    s.w.write("alpha/SKILL.md", "my alpha\n");

    app.want(Job::Check(None));
    let told = last(run(&mut app, &mut s.engine, T0));
    assert_eq!(told.feedback.as_deref(), Some("Checked: some skills need your attention"));
    assert_eq!(app.health(), Health::Attention);
    assert_eq!(app.tooltip(), "Tiny Skill Sync: 1 skill needs attention");
    assert!(row(&app, "alpha").actions.contains(&Action::Overwrite));
    assert_eq!(s.w.read("alpha/SKILL.md"), "my alpha\n");

    app.want(Job::Resolve { repo: fx(), skill: "alpha".to_string(), how: Resolution::Overwrite });
    let told = last(run(&mut app, &mut s.engine, T0));
    assert_eq!(s.w.read("alpha/SKILL.md"), ALPHA_V2);
    assert!(told.feedback.unwrap().starts_with("'alpha' now matches the repository"));
    let news = app.board.messages().last().unwrap().text.clone();
    assert!(news.contains("The replaced copy is in "), "{news}");
    assert_eq!(app.health(), Health::Ok);
}

#[test]
fn skills_gone_upstream_all_at_once_are_deleted_only_when_the_user_says_so() {
    let mut s = Setup::installed("app-gone");
    let mut app = started(&mut s);
    for name in ["alpha", "beta", "gamma"] {
        s.remote.remove(REPO, &format!("skills/{name}"));
    }

    app.want(Job::Check(Some(fx())));
    run(&mut app, &mut s.engine, T0);
    assert_eq!(row(&app, "alpha").actions, [Action::RemoveThem, Action::KeepMine]);
    assert_eq!(s.w.skill_names(), ["alpha", "beta", "gamma"]);

    app.want(Job::DeleteGone(fx()));
    let told = last(run(&mut app, &mut s.engine, T0));
    assert!(s.w.skill_names().is_empty());
    assert_eq!(told.feedback.as_deref(), Some("Checked: skills were updated"));
    assert_eq!(app.health(), Health::Ok);
}

#[test]
fn a_removed_repository_lets_go_of_its_skills_before_it_leaves_the_settings() {
    let mut s = Setup::installed("app-remove");
    s.w.write("beta/SKILL.md", "my beta\n");
    let mut app = started(&mut s);

    app.want(Job::Remove { repo: fx(), delete_skills: true });
    let told = last(run(&mut app, &mut s.engine, T0));
    assert_eq!(told.removed.as_deref(), Some("fx"), "the window takes it out of the settings");
    let text = told.feedback.unwrap();
    assert!(text.starts_with("Removed o/fixtures : skills. Deleted: alpha, gamma. Kept as yours: beta ("), "{text}");
    assert_eq!(s.w.skill_names(), ["beta"], "the edited one stays");
    assert_eq!(app.board.messages().last().map(|m| m.level), Some(Level::Attention));
    assert!(app.board.views(app.cfg(), T0)[0].skills.is_empty(), "forgotten");

    app.set_cfg(Config::default(), s.w.skills.clone());
    run(&mut app, &mut s.engine, T0);
    assert_eq!(app.health(), Health::NotConfigured);
    assert_eq!(app.arm(T0), None, "nothing to wake up for");
}

#[test]
fn new_settings_are_planned_for_before_the_timer_is_set() {
    let mut s = Setup::installed("app-plan");
    let mut app = started(&mut s);
    assert_eq!(app.arm(T0), Some(T0 + DAY));

    let mut cfg = s.cfg.clone();
    cfg.repos.push(repo("two", "o/two", "skills"));
    app.set_cfg(cfg, s.w.skills.clone());
    assert_eq!(app.queued(), &[Job::Plan]);
    assert_eq!(app.arm(T0), None, "the plan comes first");
    assert_eq!(last(run(&mut app, &mut s.engine, T0)), Settled::default());
    assert_eq!(app.arm(T0), Some(T0), "the new repository is due now");
    assert_ne!(app.health(), Health::Working);
}
