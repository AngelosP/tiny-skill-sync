# Test memory

## Map

- [Tests](../../CONTRIBUTING.md#tests): test locations, throwaway folders and alternate drives.
- [Fixing a bug](../../CONTRIBUTING.md#fixing-a-bug) and [mutant checks](../../CONTRIBUTING.md#when-a-new-test-passes-at-once).
- [Live runs](../../CONTRIBUTING.md#live-end-to-end-runs) and [contributor rules](../../AGENTS.md): isolation and fixture boundaries.

## Commands

- `scripts\dev.ps1 test -Quick -Filter TEXT` for a relevant test-name filter.
- `scripts\dev.ps1 test` for the combined code-change gate.
- `scripts\dev.ps1 test -Root FOLDER` for an explicitly selected alternate test drive.
- Coordinate live runs and cleanup; do not run global cleanup while another worktree's tests are active.

## Learned

- `cargo test --lib <filter>` with TSS_NO_APP_UPDATES=1 and TSS_NO_SAVED_TOKEN=1 works from the repo root; cargo's "Finished" line on stderr makes PowerShell report exit 1 even when tests pass. Read the "test result" line, not the exit code.
- relaunch-smoke.ps1 -NoBuild runs in about 20 s and reports five PASS lines; it uses disposable %TEMP% roots.
- `-Filter` is one substring passed to `cargo test --no-fail-fast -- TEXT`. For Plugins (issue #7 F), `-Filter plugin` misses
  `win::copilot` (`-Filter copilot`), the runner tests in `win::proc` (`a_timed_out_child`, `cancelling_an_active_command`,
  `capturing_drains`) and `tests\plugin_manager.rs` (`fake_manager`). The Add source message tests (G) match `add_source`.
- `tests\cli.rs` `tss()` runs `cli::run` in-process with a TestHost and FakePluginManager; no exe is spawned.
- No test constructs the real `Copilot` manager; engine tests inject FakePluginManager via `with_plugin_manager`. An unscripted
  fake mutation panics; `Done(None)` means exit 0 with no change. Never mutate `copilot.rs` `launch()`: it could reach the real CLI.
- Smart App Control (os error 4551) blocks fresh build-script and test exes in a new CARGO_TARGET_DIR. Seed it with
  `robocopy target\<idle-dir> target\rev-test /E` and, if a test exe stays blocked, rebuild from a source copy in
  `target\rev-copy` (no `.cargo` folder there: a nested copy merges rustflags and rebuilds everything) with a salt comment
  added to that copy's test file. Mutants also go in that copy, so the shared tree other agents build is never touched.
- `FakeRemote::tree` ETags hash only the listed entries by default, as real GitHub answers HEAD and the branch by name with
  the same ETag. `set_branch_etags(repo, true)` makes them differ per ref, so a test can tell which ETag the engine kept.
- Released v0.7.0 has state format 1 and refuses format 2; format 2 first appeared unreleased (loose sources, then plugins).

## Coverage gaps seen (issue #7 F, uncommitted review)

- Untested at review time: remove `--uninstall-plugins` "held" branch (`engine\plugins.rs`), `launch()` isolation wiring and
  error mapping, mid-action timeout/cancel in a check, failed native probe before planning, plugin kind guards in `tss resolve`/`remove`.

## Coverage gaps seen (issue #8, uncommitted review)

- Fixed and covered: bundled or filtered items no longer cancel a lane's empty-listing guard; a finished history search keeps
  an unchanged mixed source at one request; E (HEAD ETag kept), type off/on holds an edit, two sources on one agent,
  `check_claims`, and the instructions-lane guard in a mixed source now have tests that killed their mutants.
- Still untested: the single confirm for a mass delete (`window.rs` `act`); only a live UI run can count the dialogs.

## Baselines

- Relaunch restart tests (proc, app, updater x2, shell) pass by filter; relaunch-smoke.ps1 -NoBuild: 5 PASS, exit 0 (one run observed by the test specialist).
- Issue #7 F (uncommitted): `-Filter plugin` 44 passed (23 lib, 1 app, 4 cli, 16 engine_plugins); `-Filter copilot` 4 passed;
  `add_source` 3 passed.
- Issue #8 (uncommitted, 2026-11-21, final gate): lib 620, app 44, cli 63, downtime 23, engine_failures 29, engine_lanes 28,
  engine_lifecycle 41, engine_plugins 43, engine_sources 9, engine_sync 57, http 21, plugin_manager 1, source_preview 3: all pass.
  Mutants killed: `root.rs` per-lane confirm, `cli.rs` F rule, `fresh_lane_ids` foreign-record check.
- Issue #8 UX tests (`issue8_` in `page.rs`, `control.rs`, `board.rs`) check the pure Surface and Board: the hero and tray
  tooltip when every type is off, What to sync layout at 344/420/560/900 px, the Edit-first empty preview, the source tooltip.
- The ui e2e uses most of the anonymous 60 requests/hour by itself. When the limit is used up, the app falls back to the
  `gh` token and its checks get slower; a lane turned back on shows its last "Up to date" while its new check runs, and
  settings edits are refused until it ends, so a scenario must wait for the `job-check` mark before the next edit (this
  was the repo-specific switch failures in rate-limited ui runs). `Get-PublicCommit`/`Get-PublicFiles` fall back to
  `gh api` on 403/429 the same way. Only scenarios that strip `gh` from the PATH (mixed, portable, the private step
  without sign-in) still need the anonymous quota.

## Section headers (review, 2026-11-22)

- Filters `section`, `what_to_sync`, `a_closed_type` (3+3+1 lib tests) cover `section_parts` and the header Looks; `--frozen` works.
- `kind_badge` already returns no warning when `k.on` is false, so the `on &&` in `sections()` warn only matters in a draft
  (`kind_is_on` uses draft settings); no test covers a draft-off type that needs attention.
- The e2e attention check reopens Skills through `Expand-SourceSections`, so its widened regex runs live (e2e ui 262/262).

## Open proposals

- CONTRIBUTING.md Tests: list the extra filters a Plugins change needs (copilot, proc runner, fake_manager), or prefix those tests `plugin_`.
