# Security memory

## Map

- [Contributor rules](../../AGENTS.md): protected data, credentials, registry keys and file-safety invariants.
- [Reconciliation](../../ARCHITECTURE.md#deciding-the-reconcile-table) and [file changes](../../ARCHITECTURE.md#changing-the-skills-folder).
- [Private repositories](../../docs/GUIDE.md#private-repositories): authentication behavior.

## Commands

- `git --no-pager diff -- <assigned-paths>` for the assigned trust-boundary changes.
- `scripts\dev.ps1 test -Quick -Filter TEXT` for relevant existing safety tests.
- No external scanner is configured in the profile; do not install one during a feature run.

## Learned

- Tray relaunch after an app update (reviewed 2026-11-20, uncommitted tree): `replacement::verified_tray` holds
  `tiny-skill-sync.exe` through `win::install::open(path, false)` (GENERIC_READ, `FILE_SHARE_READ` only, no link, one
  hard link) and every ancestor folder through `Directory::pin` (`FILE_SHARE_READ | FILE_SHARE_WRITE`, never DELETE).
  `win::proc::start` passes the absolute path as `lpApplicationName` (no path search), no handle inheritance, `None`
  environment (inherits), no current-folder override. `cloud()` accepts Cloud Files reparse tags, so a placeholder
  is accepted by `plain()`.
- Debug seam: `TEST_RESTART`, `CMD_TEST_RESTART*` and the `on_command` arm are all `#[cfg(debug_assertions)]`; the
  release profile and the `size-*` profiles do not turn debug assertions on, `.cargo\config.toml` only sets crt-static,
  and `release.yml` builds with `scripts\build.ps1` (`cargo build --release --bins`). A UTF-8/UTF-16 byte search of
  `target\release\*.exe` for "Debug test" and "TEST_RESTART" finds nothing.
- `--restarted` and `--restarted-open` give a caller no privilege: they wait at most 120 s for the single-instance
  mutex, set a status line, and reopen the window (a caller can already post `CMD_SETTINGS`).
- PowerShell 5.1 has no `[Text.Encoding]::Latin1`; use `GetEncoding(28591)` for byte searches.

## Baselines

- 2026-11-20 relaunch review: `cargo test --lib only_a_tray_proven` and `cargo test --lib restarted` pass (debug);
  `cargo build --release --bins` was up to date. No scanner is in the profile; none was run. The e2e, soak and
  `relaunch-smoke.ps1` were not run by security. Source inspection is not scanner coverage.

## Open proposals

None.
