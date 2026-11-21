# Performance memory

## Map

- [Measuring](../../CONTRIBUTING.md#measuring): benchmark commands and budgets.
- [Measured cost](../../docs/GUIDE.md#measured-cost): existing measurements and their conditions.
- [Threads and waking up](../../ARCHITECTURE.md#threads-and-waking-up): worker and idle behavior.

## Commands

- `scripts\dev.ps1 bench` measures the release app; coordinate builds and desktop access first.
  It uses the network and throwaway data, not the real installation.
- Use the [benchmark documentation](../../CONTRIBUTING.md#measuring) rather than inventing thresholds.
- `scripts\dev.ps1 test -Quick -Filter TEXT` runs only `cargo test -- TEXT` (no fmt or clippy). Run it nested from
  Windows PowerShell 5.1 and cargo's stderr turns into `NativeCommandError`; the tool may report exit 1 although the
  script printed "All checks passed." Capture `$LASTEXITCODE` explicitly (observed 0 on 2026-11-20).
- `-Filter plugin` skips `tests/plugin_manager.rs` (`fake_manager_distinguishes_false_success_failure_and_real_snapshot_change`).
- No approved size tool: `cargo-bloat` is installed in `~/.cargo/bin` but is not an approved check; no llvm-tools; no
  dumpbin on PATH. A no-op `cargo build --release` ("Finished" with no "Compiling") proves the exe matches the tree.
- Release profile (Cargo.toml:78-85): opt-level "z", fat LTO, codegen-units 1, panic abort, strip symbols.

## Learned

- Restart after an app update (verified tray, `--restarted`): the new tray polls `CreateMutexW` every 100 ms for up to
  120 s while the old tray holds the mutex (`src/win/ui/shell.rs` `run`). The old tray's `restart()` blocks the UI
  thread in `Started::ended_within(1 s)`; the new copy cannot exit in that second on the normal path, so the full
  second is always spent before `shutdown(false)`. Shutdown may add up to `QUIT_WAIT` (2 s) if a job is queued.
- Hashing for a restart (`replacement::verified_tray` -> `disk_hash`, streaming SHA-256, 16 KiB buffer) runs in
  `Updates::run`/`commit` on the worker thread (`spawn_worker`). The UI thread only moves the `Option<VerifiedTray>`.
- Retry timer `TIMER_RESTART` (500 ms) is set only while a pending restart meets an open question or menu, or a
  borrowed shell. It is not set otherwise.
- `ARCHITECTURE.md` "Threads and waking up" says nothing polls; the 100 ms mutex wait is a documented exception
  that is missing from the doc.
- Quit race (inspection): `quit_queued` runs only after the 1 s wait and before `shutdown(false)`. Messages posted
  between that check and `DestroyWindow` (e.g. `--quit` from the CLI) are dropped; the menu cannot open during the
  block.
- `cargo test --lib only_a_tray_proven_and_held` passed (0.31 s). It covers `proc::start`, `ended_within` (Some(7),
  then None while running), and `stop`.
- Intervals: `DEFAULT_INTERVAL` is 1 day (config.rs:20), not 6 h. Presets run from 30 min to 1 month (page.rs:35).
  The custom minimum is 5 min (`MIN_POLL_MINUTES`, interval.rs:12).
- The Engine is moved into the worker thread (`spawn_worker`, shell.rs:375), so the UI thread cannot call the engine
  or the Copilot CLI. The UI thread blocks on the worker only at Quit (`QUIT_WAIT` 2 s, after setting cancel).

### Plugins source (issue #7 F, inspection of the uncommitted tree on HEAD 893cebe)

- A timer poll that GitHub answers with 304 starts no process and does no file I/O. Test:
  `plugin_timer_304_is_one_root_request_with_no_manager_calls_or_state_write` (tests/engine_plugins.rs). On an
  explicit check, a 304 is followed by a second, unconditional root-tree request (no catalog is cached).
- `check_plugins_in` (engine/plugins.rs:249-264) stores no ETag while any row needs attention, meaning any status
  except UpToDate, HostedElsewhere or a completed Planned. `record_verified` (plan.rs:226) also clears it. The
  scheduler (`due_of`, scheduler.rs:124) checks such a source once in each app run (30 s after a sign-in start) and
  then at every interval, the same rule as a held Skills repository.
- One such poll costs: the root tree without ETag, a commit pin (2 API calls), 1+N raw manifest downloads,
  2 Copilot CLI launches, and a read of `COPILOT_HOME\settings.json` (whenever a non-default market exists).
- `decide` (plan.rs:402) returns TurnedOff before it checks the fingerprint. NeedsApproval has no decline choice.
  RemovedInCopilot offers only Reinstall. AlreadyInstalled, AlreadyTracked and Rejected have no choice. Each of
  these can leave a source unsettled indefinitely.
- `snapshot()` costs 2 launches. `execute()` costs 5 (before 2, command, after 2), or 4 for Adopt, Approve and
  Forget. A first check with N installs costs 7 + 5N. The engine lock is held throughout; another process waits
  `DEFAULT_LOCK_WAIT` (3 s, engine.rs:52) and then reports Busy.
- CLI timeouts (win/copilot.rs:19-20, 165): 30 s per probe, 180 s for add, install and update, 60 s for uninstall.
  Only cancel stops the action loop; a TimedOut moves on to the next action.
- `proc::capture`: a KILL_ON_JOB_CLOSE job, NUL stdin, 2 pipe readers (joined, 1 s deadline plus 100 ms after
  `CancelSynchronousIo`), and a 50 ms wait that polls cancel. The output cap is `MAX_OUTPUT`.
  - `create` sets no priority class, so CLI children run at normal priority even for background orders.
    `THREAD_MODE_BACKGROUND_BEGIN` applies only to the worker thread.
  - Since F, `gh auth token` (`run_capture`) also goes through `capture`.
- Bounds: `MAX_TREE_BYTES` is 16 MiB (github.rs:20), `MAX_MANIFEST_BYTES` 256 KiB, and `MAX_SETTINGS` 4 MiB. The
  plugin listing is always the whole branch root.

### Mixed sources (issue #8, inspection of the uncommitted tree, 2026-11-21)

- Smart App Control blocked every fresh build script in a new target dir (os error 4551 on proc-macro2, quote,
  serde_core), even after retries and `CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_DEBUG=1`. A new reviewer target dir may
  therefore build nothing; plan on inspection or reuse an existing reviewer dir.
- Bench copies `target\release\*.exe` (`common.ps1` `New-LiveRoot`); the exe row budget is 1.70 MiB (bench.ps1:433;
  raised from 1.40 by the coordinator with the owner's delegated authority for issue #8, 2026-11-21).
- One source = one conditional root listing (`engine/root.rs` `sync_source`/`root_listing`, always `path ""`,
  recursive). `shared_etag` (root.rs:134) is conditional only if every on-lane holds the same `etag#rules` tag.
  A 304 calls only `touch_check` (lazy time); `set_lane_poll` ignores time-only changes (state.rs:775).
  Tests: `an_unchanged_source_costs_one_request_and_writes_nothing...` (tests/engine_lanes.rs:346).
- Saved history searches: the mixed-source shortcut is fixed. When the commit is unchanged and every lane could
  skip the listing (`shared_etag`), it costs 1 commit request and makes no state write (root.rs:267-277; test
  `a_finished_history_search_keeps_an_unchanged_mixed_source_at_one_request`, engine_lanes.rs:400).
- `confirm_source_at` (github.rs:230) fetches `historical_tree(.., "")`, i.e. a full recursive `COMMIT:` tree, only
  to compare the root SHA for lane views. `RootListing::confirmed` dedups it across file lanes, not with skills.
- `lane_files` (loose.rs:82-89) scans every entry under the lane path (whole repo for `""`) against every leaf with
  `same_win_name` (2 UTF-16 allocs + FFI each); HEAD scanned only top-level entries.
- `lane_discovery` (tree.rs:231) re-runs `entry_index` and clones every scoped entry per lane call; `sync_files_in`
  calls it twice for a new file lane.
- Splitter: drag saves `window.txt` once at the end (window.rs:547); each arrow key reads and writes it on the UI
  thread (window.rs:566-571). Every drag move re-lays out hero, nav and page. A list timer was removed; no new timer.

## Baselines

- `bench-20261119-225701.md`: tray 7.9 ms median; idle threads/handles 5/149; after poll 9/340; exe 1,168,384 bytes.
- `bench-20261120-001349.md`: tray 8.1 ms median; idle threads/handles 5/154; after poll 9/345; exe 1,172,480 bytes
  (+4,096). The exe row is over its 1.1 MiB budget before and after this run; the user deferred size work.
- Handles grew by 5 between those runs (149 to 154 idle). Not attributed to the restart code; no measurement
  isolates it.
- `bench-20261120-122900.md` (before plugins): exe 1,287,680 bytes; tray median 9.3 ms; idle threads/handles 5/149;
  after a poll 9/340 (budget 10/350); an unchanged poll used 15.6 ms of CPU with 0 file I/O.
- Plugins F4 (verified by a no-op release build on 2026-11-20): tray exe 1,415,680 bytes, 104,960 over the
  1,310,720 budget; tss.exe 1,204,224. Plugins add about 127 KB. The coordinator's linker map put about 116 KB
  in plugins.
- Issue #8 (observed, not my build): `target\release` built 07:49 on 2026-11-21 from the feature tree (dep-info lists
  sources.rs and location.rs; no newer source): tray 1,707,520 bytes, tss.exe 1,482,240. That is 291,840 more than
    plugins F4. The coordinator's map: HEAD v0.8.0 1,434,112 bytes, +273,408, spread across modules
    (`target\size-diff.txt`).
  - `bench-20261121-082936.md` (issue #8, all rows within budget): tray 9.2 ms; startup job 8.54 ms for 5 files
    (5.7 ms before, 1.1 ms on 2026-11-20 00:13); open to painted 55.4 ms median, max 63.1 (37-47 before); unchanged
    poll 31.3 ms CPU, 0 I/O; idle 5/154; after a poll 9/345, 1.0/2.9 MiB; exe 1,707,520 against 1.70 MiB
    (75,059 bytes headroom). Bench now adds with `--yes` (bench.ps1:252), so the fixture source has all detected types,
    not skills only. Rows before and after are not like for like.
- No measurement exists of a Copilot CLI launch (wall time, CPU, memory) or of a plugin poll.

## Open proposals

- Replace the blocking `ended_within(1 s)` on the UI thread with a timer or a non-blocking check that pumps messages.
- Poll the old tray's exit at 250 ms, or wait on a signal, instead of 100 ms `CreateMutexW` polling.
- Update `ARCHITECTURE.md` "Threads and waking up" to describe the restart wait and retry timer.
- Plugins: treat stable attention as settled, keeping the ETag, or bound native rechecks for unsettled sources.
  Reuse the previous snapshot in `execute`. Stop the action loop after a TimedOut. Start CLI children below normal
  priority. Add bench rows for a plugin 304 poll and for an unsettled poll.
