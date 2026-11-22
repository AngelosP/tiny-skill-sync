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

## Header band (review, 2026-11-22)

- Unit bar: theme.rs `the_band_stands_out_from_the_title_bar_...` (summed gap ≥ 30 to Bg and Layer, Text/Text2 ≥ 4.5:1,
  status circles ≥ 3:1, high-contrast edge differs); page.rs `the_diff_header_shows_its_whole_sentence_beside_or_under_the_choices`
  measures captions at 6 DIP per character (the shared 7-per-character `Fit` overstates Segoe UI, which is under 5.8).
- Live bar: e2e `Test-Band` (summed gap ≥ 24) in the ui scenario's light and dark restart pass, wide and 640-DIP-narrow
  Show changes (shots 18*, 19*). e2e ui baseline after the band: 302 checks, 0 failed.

## App-update token fallback (review, 2026-11-22, uncommitted)

- `-Filter release` matches `release::tests` (22), `win::release_hash::tests` (2) plus 4 unrelated lib tests, 1 cli and
  1 engine_plugins test: 30 passed. `-Filter updater`: 12 lib tests. Lib total 642, integration 367 (CONTRIBUTING says 991/624).
- release tests inject `NoLookup` (panics on lookup) or `Lookup(closure)`; `script_with_bearer` asserts URL, limit and bearer
  per request. `Updates` tests use `FeedDouble`, never `Official`. Every script sets `TSS_NO_APP_UPDATES=1`.
  `release_hash::request_headers` takes the `ReleaseRequest`, with unit tests per request kind; the ignored owner-run
  `live_metadata_bearer_reaches_github` proves the bearer reaches GitHub despite WINHTTP_DISABLE_AUTHENTICATION (passed
  2026-11-22). A boundary test pins `TOKEN_LOOKUP_BUDGET` (exactly the budget: no lookup; 1 ms more: one retry).
- Nested `powershell -File scripts\dev.ps1 test ... 2>&1` exits 1 on success; read "All checks passed." and the result lines.
  `CARGO_NET_OFFLINE=true` with the runner worked (already built; nothing fetched).

## UI polish (review, 2026-11-22)

- New unit bar in theme.rs: `entry_chip_inks_and_text_read_on_every_entry_background` (light, dark, forced high contrast),
  `the_warning_and_error_inks_read_on_every_page_tint_and_fill` (Warn and Err at least 4.6:1 on every page, tint and own
  fill, light and dark) and `the_focus_ring_stays_visible_on_every_entry_background`. page.rs
  `source_marks_use_the_soft_check_dot_only_when_synced` pins `source_mark`. `each_look` skips high contrast: test that
  look with `set(Some(Look { mode: Mode::Contrast, .. }))` and restore the saved look.
- Light `Warn` is `#905500` (was `#9D5D00`) and `Err` `#BD291B` (was `#C42B1C`), changed at the source so the words clear
  4.6:1 on a hovered or selected entry. Declined by design, with the light numbers: off-chip `Text3` 2.7 to 2.8:1 (an
  inactive control; the tooltip and accessible name carry the meaning), chip outline and fill 1.0 to 1.7:1, chip icon 6.47
  against the 7.0 aim, and the focus ring over the accent bar (as at HEAD).
- Live bar: e2e ui 306 checks, 0 failed (entry height `56 * dpi / 96`, a "type chips" label). `Read-WindowSizeFile` opens
  window.txt with `FileShare ReadWrite|Delete` because the app rewrites it while the script reads. The "choosing 12 hours
  in 'Check every'" check has an 8 s `Wait-Maybe` and failed once in four full runs (the worker's save queues behind a
  running check); 30 s like its siblings would settle it.
- Final gate: fmt and clippy clean (local clippy 0.1.100 matches CI), library 673 passed + 1 ignored, integration 367
  (app 49, cli 63, downtime 23, engine_failures 29, engine_lanes 28, engine_lifecycle 41, engine_plugins 43,
  engine_sources 9, engine_sync 57, http 21, plugin_manager 1, source_preview 3), 0 failed. CONTRIBUTING's count is 1,041.
- Smart App Control blocks random fresh exes (os error 4551). In the gate it hit `app`, `cli` and `engine_sources`: touch
  `src\lib.rs` and rerun only those with `cargo test --no-fail-fast --test NAME`. For the bench and e2e the verdict is per
  file hash: touch `src\bin\tss.rs`, `cargo build --release --bin tss` until `tss.exe --version` runs, then use
  `dev.ps1 bench -SkipBuild` and `dev.ps1 e2e -Only ui -SkipBuild`. Never probe the tray exe with `--version`: it ignores
  the flag and starts the tray (use throwaway TSS_ and COPILOT_ paths and stop only that PID).

## Open proposals

- CONTRIBUTING.md Tests: list the extra filters a Plugins change needs (copilot, proc runner, fake_manager), or prefix those tests `plugin_`.
