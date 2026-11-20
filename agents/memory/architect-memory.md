# Architect memory

## Map

- [Architecture](../../ARCHITECTURE.md): module boundaries, engine flow, threads and persistence.
- [Contributor rules](../../AGENTS.md) and [code style](../../CONTRIBUTING.md#code-style): design constraints.
- [User guide](../../docs/GUIDE.md): user-visible contracts.

## Commands

- `git --no-pager diff -- <assigned-paths>` for the assigned change.
- Choose proposed checks from [Contributing](../../CONTRIBUTING.md#before-a-change-is-done).

## Learned

Verified by reading code (2026-11, self-update relaunch review):

- `shell::run` takes the never-closed named mutex `Local\tiny-skill-sync-{instance_tag(home)}` before the worker, window
  or engine exist; a process that gets ERROR_ALREADY_EXISTS still holds a handle to the mutex, so a retry loop must
  `CloseHandle` each failed attempt or the name never frees.
- `shell::on_done` runs inside `with_shell` (a held `RefCell` borrow) from `WM_WORKER`; anything slow or re-entrant
  (launching a process, message loops) must be deferred out of that closure. Hashing belongs on the worker.
- `shutdown(clean)` guards itself with the `SHUT_DOWN` thread-local; `on_command` ignores every command (including a
  second `CMD_QUIT`/`--quit`) once it is set. Only `CMD_QUIT` calls `shutdown(true)`; `WM_CLOSE`/`WM_ENDSESSION` call
  `shutdown(false)`, which never installs on quit.
- `replacement::recover_locked` returns `Ok(false)` (does not error) when a `.tss-update-*.old` backup is still in use,
  and retries only on a later visit (next process start or next install), never with a timer. `Updates::initialize`
  skips recovery entirely when `TSS_NO_APP_UPDATES` is set.
- `Updates::commit` text "Running copies have not restarted." is shared by the tray and `tss update install`
  (`updater::manual`); `Updates::run` returns `Result<String, String>`, so there is no structured "installed" signal.
  A failed `ClockState::save` after a successful install turns the result into `Err`.
- `win::proc::create` is private; `explorer()` is the only public no-handle launcher. `win::install` doc promises it never
  runs a program. No `cfg(debug_assertions)` exists anywhere in `src`; `scripts\common.ps1::New-LiveRoot` copies only
  `target\release` exes, and `New-LiveStartInfo` always sets `TSS_NO_APP_UPDATES=1`.
- `bench::mark` appends, so one `TSS_BENCH_REPORT` file collects marks from successive processes.
- `app.started_by_windows` sets sign-in grace holds and defers the first look; it is the wrong semantics for a restart.

## Baselines

No review baseline recorded.

## Open proposals

None.
