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
- Released v0.7.0 has state format 1 and refuses format 2; format 2 first appeared unreleased (loose sources, then plugins).

## Coverage gaps seen (issue #7 F, uncommitted review)

- Untested at review time: remove `--uninstall-plugins` "held" branch (`engine\plugins.rs`), `launch()` isolation wiring and
  error mapping, mid-action timeout/cancel in a check, failed native probe before planning, plugin kind guards in `tss resolve`/`remove`.

## Baselines

- Relaunch restart tests (proc, app, updater x2, shell) pass by filter; relaunch-smoke.ps1 -NoBuild: 5 PASS, exit 0 (one run observed by the test specialist).
- Issue #7 F (uncommitted): `-Filter plugin` 44 passed (23 lib, 1 app, 4 cli, 16 engine_plugins); `-Filter copilot` 4 passed;
  `add_source` 3 passed.

## Open proposals

- CONTRIBUTING.md Tests: list the extra filters a Plugins change needs (copilot, proc runner, fake_manager), or prefix those tests `plugin_`.
