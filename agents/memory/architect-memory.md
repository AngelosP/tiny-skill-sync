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

Verified by reading code (2026-11-20, custom-agents design review):

- Folder-per-item assumptions: `tree::discover` groups by top folder with `SKILL.md` (tree.rs:123-129);
  `Applier::live` = `skills_dir.join(name)` (apply.rs:298); whole-folder renames at apply.rs:588/597/702; `carry`
  apply.rs:393. `verify::verify_files(dir, files)` (verify.rs:172) only needs the root to be a Dir, then checks listed
  files: it is already file-granular inside one root.
- `reconcile::plan` is item-generic (keyed by `fold(name)`), but the mass-deletion guard infers "listing empty" from
  `desired`/`rejected` being empty (reconcile.rs:235); a second item kind derived from skills would trip it wrongly.
- `State` ties records to one `skills_dir` and one `volume` (state.rs:183-184, `follow_folder` state.rs:306);
  `Session::volume` probes only the skills folder (engine.rs:425); `apply::preflight` checks only skills+work volumes
  (apply.rs:191). `Paths::work_dir` is the skills folder's sibling (config.rs:494). Scripts set
  `TSS_SKILLS_DIR=$Root\skills` (scripts/common.ps1:87,124), so any folder derived as a sibling stays sandboxed.
- One `RepoConfig` = one `BRANCH:FOLDER?recursive=1` listing and ETag (engine.rs:1068 `list`); sibling folders are not in it.
- The tray watches only the skills folder (shell.rs:536, 610); AGENTS.md caps idle wake-ups at two folder notifications.
- Portable `Sources` has `deny_unknown_fields` (portable.rs:69) but `RepoConfig` (config.rs:180-182) and `RepoState`
  (state.rs:93-95) do not: older versions silently ignore new nested fields and drop them on save.

Verified by reading code (2026-11-22, source-page section headers review):

- `Look::Section { title, icon, open, marks, warn }` (control.rs:157) is built directly in `page.rs` (`what_to_sync`
  ~1102, `sections` ~1203, `never_synced` ~1241). Its geometry is the pure `section_parts` (control.rs:538): chevron
  x+4, icon x+28, title x+52 Font::Strong, marks right-aligned 22 each, warning after the title; the title is cut
  before the warning is dropped. Paint measures the title only when `warn` is set. Same precedent as pure
  `row_parts` (control.rs:497), which takes a measured width (`paint::measure(s, font, 0, dpi)`, DIPs).
  `Ctl.layout` is a Row-only `OnceCell<RowParts>`; `set` replaces `look` without clearing it.
- `board::preview_source_view` rows are always `Level::Ok` with no actions, so `page::kind_badge(k).1` is never true
  in a draft preview.
- `scripts/e2e-lib.ps1:974` `Expand-SourceSections` detects collapsed kind headers with the end-anchored
  `', collapsed\.( Needs your attention\.)?$'`; any other suffix added to kind header text must be added there too.

## Baselines

No review baseline recorded.

## Open proposals

- 2026-11-20: custom agents. Recommended "A-prime" (agents shipped at `<skill>/agents/<name>.agent.md`, installed as
  independent file-level items into the skills folder's sibling `agents`, reusing `reconcile::plan` and a new
  file-level transaction in `apply.rs`). Awaiting owner decision; Copilot CLI's agents-folder path is unverified.
