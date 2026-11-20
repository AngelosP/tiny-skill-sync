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

No additional findings recorded.

## Baselines

No test results recorded. Tool availability alone is not a passing test.

## Open proposals

None.
