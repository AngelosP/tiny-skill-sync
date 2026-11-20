# Performance memory

## Map

- [Measuring](../../CONTRIBUTING.md#measuring): benchmark commands and budgets.
- [Measured cost](../../docs/GUIDE.md#measured-cost): existing measurements and their conditions.
- [Threads and waking up](../../ARCHITECTURE.md#threads-and-waking-up): worker and idle behavior.

## Commands

- `scripts\dev.ps1 bench` measures the release app; coordinate builds and desktop access first.
  It uses the network and throwaway data, not the real installation.
- Use the [benchmark documentation](../../CONTRIBUTING.md#measuring) rather than inventing thresholds.

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

## Baselines

- `bench-20261119-225701.md`: tray 7.9 ms median; idle threads/handles 5/149; after poll 9/340; exe 1,168,384 bytes.
- `bench-20261120-001349.md`: tray 8.1 ms median; idle threads/handles 5/154; after poll 9/345; exe 1,172,480 bytes
  (+4,096). The exe row is over its 1.1 MiB budget before and after this run; the user deferred size work.
- Handles grew by 5 between those runs (149 to 154 idle). Not attributed to the restart code; no measurement
  isolates it.

## Open proposals

- Replace the blocking `ended_within(1 s)` on the UI thread with a timer or a non-blocking check that pumps messages.
- Poll the old tray's exit at 250 ms, or wait on a signal, instead of 100 ms `CreateMutexW` polling.
- Update `ARCHITECTURE.md` "Threads and waking up" to describe the restart wait and retry timer.
