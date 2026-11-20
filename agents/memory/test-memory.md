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

## Baselines

- Relaunch restart tests (proc, app, updater x2, shell) pass by filter; relaunch-smoke.ps1 -NoBuild: 5 PASS, exit 0 (one run observed by the test specialist).

## Open proposals

None.
