# Quality memory

## Map

- [Code style](../../CONTRIBUTING.md#code-style): formatting, user-facing text and dependencies.
- [Contributor rules](../../AGENTS.md): safety invariants and implementation constraints.
- [Where to change what](../../ARCHITECTURE.md#where-to-change-what): module responsibilities.

## Commands

- `git --no-pager diff --check -- <assigned-paths>` for whitespace errors in the assigned change.
- `scripts\dev.ps1 test` is the code-change gate: formatting, clippy and tests.
- `scripts\dev.ps1 test -Quick -Filter X` runs `cargo test --no-fail-fast -- X` with TSS_NO_APP_UPDATES=1, TSS_NO_SAVED_TOKEN=1 and TSS_TEST_ROOT set. Judge it by `All checks passed` and the exit code; PowerShell wraps cargo's stderr in a NativeCommandError banner even when the run passes.

## Learned

- Tray restart review (uncommitted, 2025 run): `restart()` in src/win/ui/shell.rs reads `quit_queued` only before the handoff. A Quit that arrives after its last check is dropped by the `SHUT_DOWN` early return in `on_command`. Verify before any fix.
- `WM_QUERYENDSESSION` in shell.rs returns 1 without setting a flag, so a restart timer can still start a new tray after a logoff query.
- `scripts/relaunch-smoke.ps1` depends on `New-LiveRoot <prefix> <build>` (common.ps1), `$UiId.AppStatus` (e2e-lib.ps1) and `$CMD_QUIT`, all of which exist.
- Plugins sources (issue #7 F, uncommitted at HEAD 893cebe):
  - Decisions are in the pure `plugins::plan`.
  - The only executor is `apply::plugins::execute`: snapshot, `ready`, one command, snapshot, `record_verified`.
  - The real CLI runs only through `win::copilot`, behind the `isolated()` guard. Tests inject `FakePluginManager`, so the test suite never launches Copilot.
- `-Filter plugin` misses the isolation-guard tests (`copilot_*`) and the Add-source G tests (`add_source_*`). Run those filters as well. The tests\plugin_manager.rs test is now `fake_plugin_manager_*`, so `-Filter plugin` covers it and `-Filter fake_manager` matches 0.
- Round-2 review of the Plugins fix waves (uncommitted, HEAD 893cebe). Verify each of these before fixing:
  - In `check_plugins_in` (src\engine\plugins.rs), the blocker and "choice no longer applies" paths `return Err` before the ETag, `needs_attention` and `plugin_attention_rows` bookkeeping.
  - `resolve_plugin` sets no retry.
  - `copilot::mask_encoded` treats `/`, `-` and `_` as encoded, so `AngelosP/tiny-skill-sync-fixtures-public` (exactly 40 characters) is masked.
  - `COPILOT_AUTO_UPDATE=false` is enforced only by scripts\e2e-lib.ps1:543. The `isolated()` guard does not check it, and does not check that HOME and CACHE differ.
  - proc.rs:171 still checks cancel after the child exits.
- `proc::capture` (src\win\proc.rs, about line 161) checks cancel again after the child has exited, so a finished command can still report Cancelled. When that happens, `apply::plugins::execute` skips its verification snapshot.
- `plugins::NameMap` rejects duplicate JSON keys on read, which ARCHITECTURE.md documents. `tss resolve` reads the action from the third word, so a skill named like a plugin action still parses.

- Issue #8: `Which::One(lane)` for a non-plugin lane still expands to the whole source (engine.rs `sync_in`). Round 2 scoped `confirm_mass_delete` to the named lane (root.rs `sync_source` `confirmed`; cli.rs `check` maps a lane-ID query to that lane). Check any new per-lane option the same way.
- Issue #8 round 2: `state.json` format 3 (`needs_format_3`: `lane_polls` or plugin approval `roots`); `seen::update` re-reads before merging and never overwrites a newer file; `catalog::Capabilities` is computed but read only by tests (board reads `inventory::PluginCapabilities`).
- Issue #8 lanes of one source run in order (skills, agents, instructions, plugins) after one shared root listing (src\engine\root.rs `sync_source`); a remote failure in a later lane stops only the lanes after it.

## Baselines

- `cargo test --lib win::proc` with TSS_NO_APP_UPDATES=1 and TSS_NO_SAVED_TOKEN=1: 9 passed.
- `cargo test --lib updater`: 12 passed.
- `cargo test --lib restart`: 5 passed.
- `git --no-pager diff --check` on the assigned paths: exit 0 (only CRLF warnings).
- Plugins work, uncommitted at HEAD 893cebe, 2026-11-20:
  - `-Filter plugin`: 44 passed (lib 23, app 1, cli 4, engine_plugins 16).
  - `-Filter copilot`: 4 passed (3 in lib, 1 in tests\cli.rs).
  - `-Filter fake_manager`: 1 passed. `-Filter add_source`: 3 passed.
  - All four exited with code 0.
- Plugins round-2 review, run through `scripts\dev.ps1 test -Quick -Filter X`; every run exited 0 with `All checks passed`:
  - `plugin`: 82 passed (38 + 3 + 11 + 29 + 1).
  - `copilot`: 17. `add_source`: 4. `question_height`: 1. `format`: 4.
- Re-check of fix round 1: `cargo test --lib restart` 6 passed; `a_restarted_tray_says_so_when_its_first_look_is_done` and `a_restarted_tray_is_told_apart_from_one_the_user_or_windows_started` each 1 passed. The PowerShell wrapper reported exit code 1 although each test reported ok; treat the exit status as unverified.

## Open proposals

None.
