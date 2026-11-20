# Quality memory

## Map

- [Code style](../../CONTRIBUTING.md#code-style): formatting, user-facing text and dependencies.
- [Contributor rules](../../AGENTS.md): safety invariants and implementation constraints.
- [Where to change what](../../ARCHITECTURE.md#where-to-change-what): module responsibilities.

## Commands

- `git --no-pager diff --check -- <assigned-paths>` for whitespace errors in the assigned change.
- `scripts\dev.ps1 test` is the code-change gate: formatting, clippy and tests.

## Learned

- Tray restart review (uncommitted, 2025 run): `restart()` in src/win/ui/shell.rs reads `quit_queued` only before the handoff. A Quit that arrives after its last check is dropped by the `SHUT_DOWN` early return in `on_command`. Verify before any fix.
- `WM_QUERYENDSESSION` in shell.rs returns 1 without setting a flag, so a restart timer can still start a new tray after a logoff query.
- `scripts/relaunch-smoke.ps1` depends on `New-LiveRoot <prefix> <build>` (common.ps1), `$UiId.AppStatus` (e2e-lib.ps1) and `$CMD_QUIT`, all of which exist.

## Baselines

- `cargo test --lib win::proc` with TSS_NO_APP_UPDATES=1 and TSS_NO_SAVED_TOKEN=1: 9 passed.
- `cargo test --lib updater`: 12 passed.
- `cargo test --lib restart`: 5 passed.
- `git --no-pager diff --check` on the assigned paths: exit 0 (only CRLF warnings).
- Re-check of fix round 1: `cargo test --lib restart` 6 passed; `a_restarted_tray_says_so_when_its_first_look_is_done` and `a_restarted_tray_is_told_apart_from_one_the_user_or_windows_started` each 1 passed. The PowerShell wrapper reported exit code 1 although each test reported ok; treat the exit status as unverified.

## Open proposals

None.
