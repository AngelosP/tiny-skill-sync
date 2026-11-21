# Architecture

How tiny-skill-sync works inside. For the introduction, see [README.md](README.md); for how to use it, see
[the user guide](docs/GUIDE.md). For building and
testing, see [CONTRIBUTING.md](CONTRIBUTING.md).

## The big picture

There are two programs, built from one Rust library:

- `tiny-skill-sync.exe`, the tray app. It runs all the time and checks on a schedule.
- `tss.exe`, the command line. It does one thing and exits.

Both drive the same **engine**, so every safety rule is the same in both. The engine owns no window, timer or thread;
it is called, does one piece of work, and returns a report.

```mermaid
flowchart TD
    You((You)) -->|click| Tray[Tray icon and window]
    You -->|type| Tss[tss]
    Tray -->|orders| App[App: what runs next]
    App -->|one job at a time| Worker[Worker thread]
    Worker -->|calls| Engine
    Tss -->|calls| Engine
    Engine -->|asks| GitHub[GitHub client]
    Engine -->|decides with| Reconcile
    Engine -->|changes through| Apply
    Engine -->|looks with| Verify
    Engine -->|records in| State[(state.json)]
```

The code is split so that **the decisions are pure functions** (no disk, no network, no clock) and can be tested
case by case. Code that talks to Windows is kept in `src\win\`.

| Module | Job | Pure? |
| --- | --- | --- |
| `config` | Configuration model and local `config.json` parsing and writing. | file I/O |
| `portable` | Versioned settings and repository companion files beside the executables; portable skills-path expansion. | file I/O |
| `tree` | Reads a GitHub tree listing and finds entries of the selected type. | yes |
| `source` | Source kinds, preview data and per-machine selection of flat or repo-named file destinations. | yes, except root lookup |
| `pathsafe` | Rules for names and paths that come from GitHub (no `..`, no `CON`, no case clashes, length). | yes |
| `plugins::catalog`, `plugins::plan` | Match plugin manifests at a branch root; decide native actions from the catalog, local consent and Copilot's listings. | yes |
| `apply::plugins`, `win::copilot` | Run planned actions through `PluginManager` and verify the result with Copilot CLI. | Windows and file I/O |
| `reconcile` | The decision table: given upstream, the record and the disk, what may be done to each skill? | yes |
| `scheduler` | When is each repository due; how long to wait after a failure. | yes |
| `interval` | `30m`, `6h`, `1d`, `2w`, `1mo` and calendar-month arithmetic. | yes |
| `board` | What the tray and the window say: one line per repository and per skill, and the buttons. | yes |
| `app` | The tray app without its windows: which job runs next, when to wake up, and worker settings lifecycle. | state machine and worker I/O |
| `autostart` | Start with Windows: what the startup entry says, what it means, and which registry keys may be written. | yes |
| `diff` | What is different between the GitHub copy of a skill and the copy on disk, line by line ("Show changes"), and a skill's files whole ("Preview"). | yes |
| `text` | When two copies of a file are the same: line endings, the line break at the end and blank lines don't count. | yes |
| `logo` | The app's logo: its shapes, drawn to pixels at any size (the tray and window icons) and as SVG (see [below](#the-tray-and-the-window)). | yes |
| `state` | `state.json`, the ownership record: what the app installed, file by file. | file I/O |
| `verify` | Is a skill folder on disk still what was installed? | file I/O |
| `apply` | The only module that moves or deletes anything in the skills folder. | file I/O |
| `engine` | Puts it together: lock, load, ask GitHub, reconcile, apply, save. | uses all |
| `github` | List a folder, pin the commit, download a file, page through path history and read complete historical subtrees; the address of a skill on github.com (`web_url`). | network |
| `hash` | SHA-1 and Git blob hashes, in plain Rust. | yes |
| `cli` | `tss`. | uses engine |
| `win::fs` | File facts `std` does not give: file id, USN, volume, links; one-handle folder listing. | Windows |
| `win::http` | A small blocking HTTP client on WinHTTP. The only networking code. | Windows |
| `win::cred` | The token in Windows Credential Manager. | Windows |
| `win::proc` | Starts other programs with `CreateProcessW` (not `std::process`, which costs the tray exe about 60 KB): `gh auth token` for the GitHub CLI's token, File Explorer, and the tray an app update installed. | Windows |
| `win::clip` | Puts text on the clipboard (Copy path, Copy GitHub link). | Windows |
| `win::autorun` | The startup entry in the registry: read, written and deleted only when the user asks. | Windows |
| `win::ui` | The tray icon, the window, questions, and the controls they are drawn with (see [below](#the-tray-and-the-window)). | Windows |

## Threads and waking up

The tray app has two threads.

- The **UI thread** owns the hidden main window, the tray icon, the window you open and the message loop. It sleeps in
  `MsgWaitForMultipleObjectsEx` on window messages and three handles: a waitable timer set to the next check, a change
  notification on the local app's folder (`config.json`, `state.json`), and one on the skills folder. Nothing polls
  while idle. Two waits around an app restart do: a tray started to replace an older one tries the single-instance
  mutex every 100 ms, for up to 120 s, until that one has gone, and a pending restart tries again every 500 ms while a
  question, the tray menu or a sign-out holds it.
- The **worker thread** owns the engine and runs one job at a time: loading settings, the start-up look, a check, a
  resolution. Network calls, hashing, portable companion-file I/O and file changes never run on the UI thread.
  Settings load at startup and are reloaded before a poll or explicit check, and when the window opens; no
  shared-folder or per-source watcher is added. Loose-file status is refreshed by the existing local look, startup,
  an explicit check, or a fresh listing on a scheduled check. An unchanged conditional poll does not read the files.

```mermaid
sequenceDiagram
    participant UI as UI thread
    participant W as Worker thread
    participant E as Engine
    Note over UI: start: one copy per app folder (named mutex)
    UI->>UI: hidden window and tray icon (about 8 ms)
    UI->>W: Load settings
    W-->>UI: Settings
    Note over UI: started at sign-in: first look waits 30 s, unless you come
    UI->>W: Startup job
    W->>E: startup: finish interrupted changes, load the record, quick check
    E-->>W: report
    W-->>UI: done
    UI->>UI: update the icon, trim memory, set the timer
    Note over UI: asleep: 0 CPU, 0 I/O
    UI->>W: Check job (timer fired, or "Sync now")
    W->>E: sync
    E-->>W: report
    W-->>UI: done
```

Timed checks run in Windows' background mode (low I/O and CPU priority); the start-up look and checks you ask for run
at normal priority, except the start-up look after a start at sign-in (below). After the start-up job, after a timed
check and when the window closes, the app trims its working set so it holds about 1 MiB while idle.

`app.rs` guards against busy loops whatever the engine answers: a job that could not run at all is retried after
5 minutes, doubling up to an hour; a job that found `tss` working is retried after a minute; two timed checks are at
least 30 seconds apart; and after the computer wakes from sleep, the app waits 30 seconds for the network.

Started by Windows at sign-in (`--background`, see [Start with Windows](#start-with-windows)), the app shows its icon
and reads settings on the worker, but does not look at skills or GitHub for 30 seconds (`SIGN_IN_GRACE`), so that
Windows can finish starting and the network can come up: the start-up job is taken off the queue
(`App::started_by_windows`) and the timer queues it when the time is up. It then runs in background mode, followed by
the check of any repository that is due. Meanwhile the icon is a blue ring with the tooltip "waiting for Windows to
finish starting" (`Health::Waiting`). Someone coming ends the wait:
opening the window, or any job they ask for, queues the start-up job at once at normal priority (`App::user_here`);
timed checks still wait for the 30 seconds. A look the skills folder's notification asks for during the wait is
dropped, because the start-up job looks anyway.

A change in the skills folder only leads to a read-only look, two seconds after the last change, so the window shows
an edit without waiting for the next check. Safety never depends on it: every change starts by comparing
the disk with the record. Settings that name another skills folder lead to a start-up job instead, retried like the
first one, so the change of folder is recorded and reported at once (see [Files on disk](#files-on-disk)).

## A check

This is the Skills check. [Copilot CLI plugins](#copilot-cli-plugins) have their own rules for conditional requests.

```mermaid
flowchart TD
    Due[Repository due] --> Lock[Take engine.lock]
    Lock --> Ask[List the folder, with the saved ETag]
    Ask -->|304 not modified| Quiet[Done: no file read or written]
    Ask -->|error of any kind| Fail[Report it, try again later: nothing decided]
    Ask -->|200 listing| Whole{Complete listing?}
    Whole -->|no, truncated| Fail
    Whole -->|yes| Find[Find the skills]
    Find --> Look[Look at the disk]
    Look --> Decide[Reconcile each skill]
    Decide --> Act[Apply what is safe]
    Act --> Save[Save the record, keep the ETag only if all settled]
```

- **One request when nothing changed.** The app lists the configured folder with
  `GET /repos/OWNER/NAME/git/trees/BRANCH:FOLDER?recursive=1` and sends the ETag it saved last time. GitHub answers
  `304 Not Modified` with no body when nothing in that folder changed, even if other parts of the repository did.
- **Downloads are pinned.** When something changed, the app reads the commit the branch points at and downloads each
  needed file from `raw.githubusercontent.com` at that commit. Every file is checked against the Git blob hash in the
  listing. If the branch moved during the download, a hash does not match, nothing is installed, and the check is
  tried again a minute later. A file the listing names that is not found at that commit (a push landed between the
  two reads, or GitHub answered them from caches of different ages) is the same kind of failure (`Changed`, in
  `engine.rs`, `missing_means_changed`), for a check, **Show changes** and an overwrite alike: it never reads as a
  missing or private repository.
- **The ETag is kept only when everything settled or a completed/paused comparison is recorded.** If other work was held, failed or was cancelled, the next check
  lists again instead of getting a 304, so unfinished work is never forgotten. Historical comparison evidence is
  checked before reusing a comparison: a changed relevant local path or source requires a fresh listing. A choice about a skill drops the ETag
  too. The tray checks such a repository once in each run, as soon as it sees it (see [Scheduling](#scheduling)). A
  304 says nothing new about the skills, so its report carries over what the record knew: a repository whose last
  listing had no skills still reports so.
- **The token is sent only when it is needed** (`engine.rs`, `list`). A repository is listed without a token first, so
  a public one never makes the app look for one. When GitHub answers "not found" (it may be private) or the limit for
  anonymous requests is reached, it is listed once more with the token. A repository found to be private this way is
  remembered in the record (`needs_token`), so later checks send the token at once, until it can be read without one
  again. `use_token` in the settings sends it from the first request. Without a token, the failure is reported with
  what to do; as with every failure, nothing is decided from it.
- **`gh` is only a token source.** All requests are made by the app's own WinHTTP client.

## Deciding: the reconcile table

`reconcile.rs` turns three facts about each skill into a verdict: what upstream lists, what the record says the app
owns, and what is on disk. It is a pure function; every row below is a test.

| Upstream | Record | Disk | Verdict |
| --- | --- | --- | --- |
| has it | not owned | no folder | **Install** |
| has it | not owned | a folder holding every upstream file, the same | **Adopt** (record it as owned, change nothing) |
| has it | not owned | every file of one whole historical version matches | **Adopt historical**, then **Update** |
| has it | not owned | history search still active or paused | Hold only this skill; show search controls |
| has it | not owned | a folder, different | **Hold**: it may be yours |
| has it, same | owned | clean | In sync |
| has it, changed | owned | clean | **Update** |
| has it, same | owned | edited or deleted | Report only |
| has it, changed | owned | edited, and the folder is now what upstream has | **Catch up** (record it, change nothing) |
| has it, changed | owned | edited (not what upstream has) or deleted | **Hold** |
| gone | owned | clean | **Delete** |
| gone | owned | edited | **Hold** |
| gone | owned | no folder | Forget it |
| rejected (unsafe) | any | any | Leave it untouched, report why |
| any | owned by another repository | any | Skip: a name clash |
| any | "keep mine" | any | Skip |

**What the user added is theirs.** A skill folder the record does not own is adopted only when every file in the
current upstream version, or in one single past version from the configured branch's history, is present and the
same under the existing text comparison rules. A folder matching a past version may then be updated automatically;
otherwise it is held. A file the user added to a skill is never changed, deleted, reported or shown: an update or a
removal moves it along (`carry`), and when GitHub adds a file with the same name, the user's goes to `backup-*`.
An edit means a file GitHub put there was changed or deleted. If a
listing has no skills at all while some are owned, every delete is held until you confirm (the **mass-deletion
guard**): an empty listing is more often a mistake than a real removal.

"Identical", "same" and "clean" all mean the same as `text.rs` says: two copies of a text file are the same when only
their line endings, the line break at the end or blank lines differ. That holds for adopting a folder, for finding
edits, and for the proof `apply.rs` needs before it replaces or deletes a folder. Downloads are still checked byte for
byte against the Git blob hash.

**A failure never reaches this table.** `github::FailureKind` separates authentication, not found, rate limited, server
error, redirect, protocol, network, too large, cancelled, changed during download and local errors. Any of them ends
the check of that repository before anything is decided.

### An edit that GitHub has too

"Edited" in the table means the folder differs from what the app *recorded* when it installed the skill. **Show
changes** compares the folder with what GitHub has *now*. When an edit made here is later put on GitHub too, the two
disagree: the folder is not what was recorded, and yet it is exactly what GitHub has. Held for ever, the skill would
look edited while **Show changes** says there is nothing to show.

So before reconciling, for each owned skill that is edited **and** whose upstream changed (`reconcile::needs_comparison`:
a clean skill or an unchanged upstream costs nothing), the engine compares the folder with upstream
(`Engine::compare_owned`, `verify::compare_owned`). The folder counts as what upstream has only when every upstream
file is here and the same as `text.rs` says, **and** every file the record owns that upstream dropped is gone. Then the
verdict is **Catch up**: `Applier::catch_up` looks at the folder once more, records the current commit and files, and
writes not one byte to it. Any other difference keeps the hold, as before.

- **Reads are only for what the record cannot answer.** For every file GitHub did not change, the record's `canon`
  already says what GitHub has (`verify::known_canons`), so the common hold, "edited one file here while GitHub
  changed another", reads nothing from GitHub. Otherwise a text file whose bytes differ from GitHub's is read from
  GitHub, one file at a time, until a difference is proven or everything agrees. Cost: a held skill is listed again at
  every check (it already was), and this comparison runs again with it: at most one small download per file GitHub
  changed, usually one, and none once the skill settles.
- **A failed read ends the check** of the repository before anything is decided, like any failure. So a read that
  keeps failing for one edited skill also holds back the repository's other skills until it stops failing or that
  skill is resolved. That is the price of one simple rule: a failure decides nothing. Going past the download limit
  only leaves that skill held; it is never read as a match.

### Recognising older copies

`compare_unmanaged` compares the current version first. For a different copy, `Remote::history` requests commits
touching that skill's path, pinned to the configured branch's commit, in pages of 100. The pin's configured tree must
match the current listing. `Remote::historical_tree` walks complete parent trees and reads the skill subtree:
missing paths must be proven by a parent listing, never inferred from a 404. Truncated, malformed or unsafe trees
and failed downloads abort the repository before reconciliation. Downloads still prove their Git blob hashes.

The quick budget is **eight historical candidates per repository check**, shared across its unowned skills.
Remaining candidates continue in the same bounded slices, at least 30 seconds apart, on the existing worker. It is
not a total-depth limit: pages continue until a whole version matches or the actual history is exhausted. An active
slice is work, not an unchanged poll. A comparison that reaches the download limit is an error, never a no-match.
User resolutions run before queued checks; the shell cancels an ongoing check with a flag belonging to that order
alone, so cancellation before starting or just after finishing cannot cancel the following resolution.

`RepoState::searches` saves the source, current tree, pin, page/cursor, candidates checked, pause/completion flags,
volume/journal identity and local evidence for the union of paths actually compared. File fingerprints include the
USN; missing paths are probed again through `Spots`, including their parent paths. This detects repairs and nested
edits without reading unrelated extras. Without a trusted journal, content hashes are used instead. Root changes
follow the ordinary set-aside records. Source/tree changes or invalid evidence restart comparisons; none of this
cached evidence authorizes a file change. Completed/paused unchanged polls make one branch-commit request and
write nothing; last-check times remain lazy. Failures are not cached as no-match, and active-search retry times
survive restart. Pauses persist until Resume, even across a source change.
The eight-candidate quick budget took 11.4 seconds in the live unmanaged fixture check, including the initial
install and an older-copy update. It bounds candidates, not wall time; network latency still varies. The worker
checks cancellation before swapping a staged version, and retains the search until a historical update succeeds,
so a late Cancel can still pause the already adopted old baseline without losing its proof.

The branch-commit probe is needed even when the current tree is unchanged: a push followed by a revert can add a
matching past version without changing the tree's ETag. Ordinary owned-only polls still use the conditional tree
request. Both unchanged paths cost one request, zero file content reads and zero writes on NTFS.

Only a full matching version creates `UnmanagedState::Historical` and the distinct pure
`Verdict::AdoptHistorical`. The context keeps that old version and its text proof. `Applier::adopt` fully rechecks and
records **the old baseline**, then the ordinary update transaction stages the current version and rechecks again.
A stop after adoption leaves a valid owned old baseline. No unconditional backup is made; extras and collision
backups use the normal carry/recovery path. UI, CLI and Activity report historical update success only when apply
succeeds. Force/keep remove or supersede saved searches; no queued continuation has independent installation authority.

## Changing the skills folder

`apply.rs` is the only code that moves or deletes anything in the skills folder. An update goes like this:

```mermaid
stateDiagram-v2
    [*] --> Listed: the live folder can be listed
    [*] --> Held: it cannot (a path too long, no access)
    Listed --> Staged: download into stage-txn, check every hash
    Staged --> Checked: Full check of the files GitHub put there
    Checked --> Held: edited meanwhile
    Checked --> Marked: pending marker saved in state.json
    Marked --> Swapped: old folder renamed out, new one renamed in
    Swapped --> Carried: what the user added moved into the new folder
    Carried --> Audited: old folder checked once more
    Audited --> RolledBack: a late edit slipped in
    Audited --> Done: final record saved, marker cleared
    RolledBack --> Held
    Held --> [*]
    Done --> [*]
```

- The work folder (`.tss-work`) sits **beside** the skills folder, on the same drive. Copilot never sees a half-made
  skill, and the renames are atomic.
- The swap uses renames, so the skills folder never holds half a skill.
- **What the user added moves with the skill.** After the swap, `carry` moves every entry of the old folder that the
  old version did not list into the new folder, one at a time and never replacing anything. Something in the way of a
  file of the new version (the user's file of the same name) stays in the old folder, which then becomes a backup.
  `carry` returns those paths, so an update says `Applied::Displaced` and the engine adds a warning naming them and
  the backup (Activity shows it, and so does `tss`); a recovery that finishes such an update says the same in its
  note. An overwrite says nothing of its own, because it keeps the whole old folder and `resolve` reports that backup.
  A folder that cannot be listed (with Windows long paths off, one of the user's whose path is over 260 characters)
  holds the update before anything is downloaded, since what is in it could not be carried.
- A delete is the same without the download: Full check, marker, rename to `trash-txn`, audit, remove. When the folder
  holds anything else, an empty new version is swapped in instead, so the user's files are carried back and only the
  files GitHub put there go; the folder itself is removed at the end only if it is then empty.
- **A folder is deleted only after it was proven to equal a version the app installed or fully verified and adopted
  from upstream history** (equal as `text.rs` says, so a
  copy that differs only in line endings or blank lines is replaced or deleted like the original), and to hold nothing
  else. Anything else is kept as `backup-<skill>-<txn>` in the work folder, and backups are never deleted
  automatically.
- For Skills transactions, if the app or the computer stops between two steps, `recover` reads the pending marker at
  the next start and finishes or undoes the change, even if its source was removed or changed in settings. It finishes
  an update only when the new folder is a plain folder in place and the old one is still clean; otherwise it moves the
  user's files back (`uncarry`) and puts the old folder back. Tests stop the code at every step, including between two
  carried files, to prove it (`engine_lifecycle`, `apply` tests). Loose-file recovery uses the separate source-identity
  rule below.

### Loose files and reviewed changes

`engine::loose` checks Agents and Instructions sources with the same scheduler, but owns one file at a time.
Before accepting any absence it checks a complete, safe listing at the recorded source and pinned commit.
A refusal or failed lookup is not absence. The empty-listing confirmation is separate from the full local check.
Remote planning and downloads finish before changing files. After a local binding is recorded, a blocked leaf is
held without moving the binding or holding unrelated leaves. The alternate layout is still checked for that exact
name; an ambiguous unbound layout is never selected.

`apply::leaf` stages verified bytes in `.tss-leaf-work` on the destination volume, outside the installed files.
New work requires the directory's owner marker to match its physical identity and the settings home. The journal
and opened file identities authorize only its named live, staged and backup objects. Recovery restores a proven
original or finishes an already proven change; it never installs a stage on its own or sweeps unrelated contents.
At the next writable start, pending leaf work is recovered only when the active source has the same ID, kind,
repository, branch and path as its recorded origin. If the source was removed or rebound, its files, state and work
remain untouched until the original source configuration is restored. Recovery uses the transaction's immutable
destination binding, bounded paths and identity proofs; it does not choose a destination from current settings.
If the change is already committed and the old file cannot be retired, the result reports success with a retained
backup. Atomic settings/state writes flush, publish and clean up their own temporary file through its open handle.
The directories still rely on the caller's Windows permissions.

**Show changes** keeps the evidence for the displayed comparison in memory. A later overwrite must still match
the source, selected upstream file hashes, relevant local byte hashes and presence. The transaction rechecks the
local evidence before publication. A mismatch requires a new comparison and choice; a successful overwrite discards
the comparison, so it cannot authorize a later retry. This does not change the
text comparison rules used for ordinary sync, or make unrelated files part of a comparison.

### Copilot CLI plugins

`engine::plugins` polls the branch root even when the source selects a subfolder. A fresh listing must be complete,
and every downloaded manifest must match its Git blob hash. `plugins::catalog` selects the local entries in scope
and records what they declare. A preview stops here: it checks for the CLI executable but does not run it.

A complete check keeps the ETag even if some plugins need attention; `PluginPoll` saves `needs_attention` separately.
A timer poll answered with a 304 starts no process and writes no file. The tray keeps the last rows and choices.
The first timer check per app run for a source with saved attention skips the ETag if its rows have not been rebuilt
in that run. A blocker or a choice that no longer applies clears the ETag and marks an existing source as needing
attention, without changing its last check time or creating a new record. **Sync now**, `tss check` and plugin
choices always check in full. An update held while a plugin was turned off in Copilot can go through once it is on
again at the next full check.

`plugins::plan` takes the catalog, the local record and the native plugin and marketplace lists. It produces status
rows and the actions that are safe to attempt. `apply::plugins` checks the plan against the latest Copilot snapshot:
the one used for planning, or the one taken to verify the previous command. Adopt, Approve and Forget need no new
launch. Cancel stops a running command, but once the runner sees the child exit, a late cancel does not discard its
result. A finished command gets a fresh snapshot for verification; the engine saves state after each verified action.
A timeout stops the remaining actions until the next check. Upstream removal is a status, not an uninstall.
Read-only status uses the local record without launching Copilot.

Production uses `win::copilot` and a bounded process runner; tests inject a fake. Copilot CLI and `gh` start in their
own exe folders with `NoDefaultCurrentDirectoryInExePath=1`. Copilot runs at below-normal priority and with `NO_COLOR=1`;
the app adds no token. Under isolation, `COPILOT_HOME` and `COPILOT_CACHE_HOME` must be separate absolute drive paths,
without a UNC or device prefix or a component containing `~`. Neither may contain the other or overlap the real homes.
The child gets `COPILOT_AUTO_UPDATE=false` only under isolation. The app never writes Copilot's own files.
It proves marketplace origin from both Copilot's marketplace list and `settings.json`; an unknown field in the origin
makes the proof fail and blocks the source. Copilot errors mask tokens, URL credentials, Authorization and Bearer
values, `token=` and `password=` values, and likely hex or base64 runs of 40 or more characters. Ordinary URLs and paths
stay readable when they can be told apart from those long runs.

Consent (name and publisher) lives in the format-2 `plugin_sources` namespace in local `state.json`, bound to the
source, marketplace and Copilot home. Adding from a preview saves settings first, then records consent from the same
preview. IDs must not overlap any other state record. If the origin, marketplace or Copilot home changes while the
source still lists installed plugins, approval refuses to replace the record. Portable settings carry only the source,
not consent. Plugin records share an ordered name index with the catalog and plan. They use JSON objects on disk,
written in key order and read only if the keys are unique. State format 2 refuses unknown top-level keys; format 1
still ignores them.
The CLI follows a moving branch; the app cannot force it to install the exact commit used to build the catalog.

## Finding edits: quick and full checks

`verify.rs` has two strengths.

```mermaid
flowchart TD
    Start[A managed file] --> Can{Drive can vouch?}
    Can -->|no journal, other journal id, no answer| Hash[Read and hash it]
    Can -->|yes| Same{Fingerprint matches?}
    Same -->|yes| Clean[Clean, nothing read]
    Same -->|no| Hash
    Hash --> Cmp{Same Git hash, or same lines that count?}
    Cmp -->|yes| Learn[Clean: learn the new fingerprint]
    Cmp -->|no| Edited[Edited]
```

- The **fingerprint** is what Windows reports for a file through one handle: file id, size, last-write time, change
  time and the NTFS **USN** (update sequence number). The USN moves on every content change, even when the size and all
  the times are put back, so the quick check finds edits made while the app was not running. The app never reads the
  journal's history, so a journal that wrapped around cannot hide an edit; a re-created journal has a new id, and then
  nothing is trusted.
- The **quick check** is used for status: start-up, a look after the folder changed, and before deciding. When every
  fingerprint matches, it reads no file contents at all.
- The **full check** reads and hashes every file. It runs before any update or delete, because a write through a
  memory-mapped view that was not flushed yet does not move the USN.
- A file that changes while it is being checked counts as edited. When in doubt, a file is edited, never clean.
- A file is compared by its Git hash, and a text file also by the hash of its lines that count (`text::canon`, kept in
  the record as `canon`; empty for a file that is not text or is over 1 MiB, which is compared byte for byte). So
  another size rules a file out at once only when it is not text. Records written by version 0.2 or older have no
  `canon`: such a file is compared byte for byte, and is not vouched for by its fingerprint until a check has read it
  once and learned its `canon` from bytes equal to its Git hash.
- Each skill folder is opened once without following links, and listed through that same handle
  (`win::fs::Folder`, `GetFileInformationByHandleEx`). The first query restarts the listing, so asking twice gives the
  same answer, and "not found" means an empty folder only on that first query; later it is an error, never a complete
  listing. Subfolders are opened by their path, again without following a link at the end of the path, so a folder
  replaced by a link is reported, never followed. A listing only reads names; if a folder higher up is swapped for a
  link while it runs, the names may come from the link's target (a known limit of opening by path).
- "Edited" always means *different from what was recorded*. The other question, *is the folder what GitHub has now*,
  is `verify::compare_owned`, asked only of an edited skill whose upstream changed (see
  [An edit that GitHub has too](#an-edit-that-github-has-too)).

The evidence for this design, including the spikes that measured it, is in [spikes/README.md](spikes/README.md) and the
tests in `tests\downtime.rs`.

## Scheduling

### The app release schedule

`release` asks the official latest stable release endpoint. When installing, it also reads the installed tag's
official metadata: both existing files must match that tag, and **both** downloaded sizes and SHA-256 digests
must match the new tag. Metadata, redirects and both downloads share a 90-second deadline. The skill client's
redirect and token rules do not change. It distinguishes the running build from the pair on disk.

`update_clock` lives in the machine-local settings home, separate from preferences, skill timestamps and the
ownership record. The tray uses the earlier app or skill deadline on the **existing** timer. A release request,
verification and replacement all use the same serialized worker as skills, with separate failure/backoff state.
Queued automatic work reloads the active preferences. A preference change revokes a pending pair; no settings
or timing file is a deferred instruction to install code.

`replacement` coordinates writers at the executable folder, not the settings home, and rechecks the installed
version and digests there. Staging, backups and the shared receipt use the installation volume. A separate
witness in the initiating settings home corroborates recovery; a different home does not retire another
updater's backups. Neither record authorizes a future install.

An on-Quit pair is held only in worker memory (at most two 8 MiB payloads). Explicit Quit drains current work and
requires successful settings saves and an engine flush. Its two-second drain is a limit on starting new work,
not a promise to terminate an already active filesystem transaction in two seconds. Session end revokes an
uncommitted install. A filesystem lock coordinates one physical
directory, not independent cloud replicas: use one writer and allow replication to finish before using another PC.

Only two installs start a program afterwards: **Install now** in the tray and a scheduled Immediate install whose
preferences are still current. Each must really have replaced the pair. Then `replacement::verified_tray` opens the
new `tiny-skill-sync.exe` and its folder without delete sharing, and compares the file's SHA-256 with the release
digest. The tray starts that held file (`win::proc::start`: no inherited handles, the same environment and current
folder, so `TSS_HOME` and `TSS_SKILLS_DIR` carry over) with `--restarted`, or `--restarted-open` when its window was
open, and quits the normal way. The proof relies on the volume enforcing the held handle's share modes. The new copy
waits up to 120 s on the single-instance mutex, which the old one holds until its process ends, so it cannot take
over early. Quit, `--quit`, an On-Quit install, sign-out, a `WM_CLOSE` to the tray, a failed or unneeded install, a
stale schedule, an app clock that could not be saved and `tss update install` start nothing. A restart waits while a
question, the tray menu or a sign-out (`WM_QUERYENDSESSION` until `WM_ENDSESSION`) is pending. After the start, the
UI thread waits one second to see that the new copy keeps running, and the window does not answer meanwhile. If the
new file cannot be proven or started, or it stops within that second, the old tray keeps running and says why. A
Quit, a close or a sign-out that arrives during that second, or while the old tray finishes quitting, stops the new
copy, which is still waiting for the mutex, so the app stays closed. Only a `--quit` in the fraction of a second
between the old tray's window going and the new one's appearing finds no tray to close. `--restarted` and
`--restarted-open` only choose the first status line and whether the window opens; they grant nothing. The new
tray's status line says it restarted once its first look at the skills folder is done. Debug builds only have tray
commands 1098 and 1099, which fake such an install with a right or wrong digest for `scripts\relaunch-smoke.ps1`.

### The skill schedule

`scheduler.rs` is pure: the time is passed in.

- A repository that was never checked is due now; otherwise one interval after its last successful check (so
  restarting the app causes no extra requests).
- Except: a repository without an ETag (its last check held something, or a choice about one of its skills was made)
  is checked once in each run of the app, as soon as the app sees it so, usually straight after start. What a check
  held is known only to the run that found it, so without this the window would show no rows for those skills until
  the next interval. That is at most one request per run for each such repository (`Schedule::checked`).
- After a failure: 5 minutes, then 10, 20, 40 and so on, never later than the repository's own interval. A rate limit
  waits for the time GitHub named.
- Nothing runs more often than every 5 minutes. A month is a calendar month, clamped to the end of a shorter month.
- Active historical work is the exception: a bounded slice every 30 seconds, with normal failure backoff and saved
  rate-limit waits. Paused and completed searches use the ordinary repository interval.
- A last-check time in the future (the clock was set back) counts as now.
- The tray re-plans the timer when the clock changes (`WM_TIMECHANGE`) and when the computer wakes from sleep.

## Files on disk

```mermaid
flowchart TD
    Home["%LOCALAPPDATA%\tiny-skill-sync"] --> Config[config.json: what you set]
    Home --> StateFile[state.json: what the app owns]
    Home --> LockFile[engine.lock: one worker at a time]
    Exe["Executable folder (shared, optional)"] --> Settings[tiny-skill-sync.settings.json]
    Exe --> Sources[tiny-skill-sync.sources.json]
    Parent["%USERPROFILE%\.copilot"] --> Skills[skills: only skill folders]
    Parent --> Work[.tss-work: stage, old, trash, backups]
```

- In local mode, `config.json` is meant to be read and edited by hand. The tray reloads it when it changes.
- Portable mode is selected when either companion file exists beside the executable. `portable.rs` merges the two
  versioned portions into the existing `Config` model; a missing portion uses defaults, while a malformed or
  unsupported present file is an error. `tss portable` exports local settings into both files and refuses to replace
  either existing file.
- Companion files are read and written on the worker, without a new notification handle. The app reloads them before a
  portable poll or explicit check and when the window opens. Saves compare the changed portion with the version last
  loaded and write only that portion; separate files are not a cross-file transaction, so simultaneous writers are
  unsupported.
- A missing source `kind` is Skills, preserving old IDs and skill records. Typed local config, portable sources
  and state use format 2, which old Skills-only readers refuse; the portable settings companion stays separate.
  Binding and loose-file ownership are stored only in local state, not in portable sources.
- `state.json`, `engine.lock`, window size, the startup-offer marker, mutex and saved token stay per machine. Shared
  repository records include their enabled state and interval. A missing shared repository becomes an orphan in the
  local ownership record; it is not a request to delete its skills.
- The default skills folder is resolved from each machine's `%USERPROFILE%`. Portable `skills_dir` values may contain
  environment variables; a folder shared by machines is unsupported because each machine keeps its own ownership
  record and can otherwise race while changing the same skills.
- `state.json` holds, per repository: the ETag of the last fully applied listing, the last check time, whether the last
  listing had no skills in it at all, the "keep mine" list, and per owned skill its commit, its files (path, Git blob
  hash, size, fingerprint, and `canon`, the hash of the lines that count), when the app last installed, updated or took
  over the skill (`updated`, only ever shown; an older record has none until the skill's next update) and an optional
  pending marker. It also
  keeps resumable historical searches and their local evidence (above). It is written
  only when something changed, through a temporary file and a rename. A damaged one is kept as `state.json.damaged-*`;
  one written by a newer version is not touched.
- `state.json` also names the skills folder its records describe (`skills_dir`). Every call compares it with the folder
  in use before it reads a fingerprint or sorts out an interrupted change (`State::follow_folder`). On a change the
  records are set aside under the old folder's name (`set_aside`, the last four folders), and the new folder's own
  records from an earlier visit, or empty ones, take their place, so the old folder is never touched and its unfinished
  changes are never replayed in the new one. Only calls that report warnings take in such a change: start-up and check
  record it, and a read-only look (`tss status`) only reports it. The others (`resolve`, `remove_repo`, `flush`) leave
  the file alone and ask for a check first. `sweep` waits
  while a set-aside record holds an unfinished change, because two skills folders can share a work folder.
- `engine.lock` stops the tray and `tss` from working at the same time; the loser is told to try again.
- The app keeps nothing inside the skills folder, so Copilot only ever sees skill folders.

## The tray and the window

`board.rs` turns the engine's reports into what you see: a level per skill (fine, needs attention, error), a line of
text, and the buttons that make sense. It is pure, so every sentence and button is tested. The newest news about a
skill wins; a failed check or a 304 says nothing new about the skills, so the last real rows stay. A repository whose
last listing had no skills in it says "No skills found". That comes from the record, not from an empty list of rows,
because after a restart a repository whose skills were all held or skipped has no rows either.

The windows (`src\win\ui\`) are Win32 windows with no dialog templates, no COM and no UI library: the app draws
everything they show.

- **`control.rs`** has one window class for every kind of control (button, skill row, switch, choice, list, entry in the
  repository list, label) and one for a panel, which holds controls, scrolls them and draws what is behind them (cards,
  dividers, icons, plain text). A page is a `Surface`: its items, each a real child window with an id and the text it
  shows (so the tests find them), and its decorations. `set` matches a new surface with the controls already there by
  id, so the focus, the hover and the scroll position survive a refresh. A window costs about 0.15 ms to make, so only
  the controls within about two screens of what is shown are made initially (`NEAR`); the others are made as the page scrolls to
  them, or when the window asks for one by id (`child`). 463 skills open in about 43 ms; with a control made for every
  row it took 125 ms. The controls of a page never overlap: they are sibling windows that do not clip each other, so a
  control that paints again by itself (under the mouse, on a click) would draw over the part of a neighbour it covers.
  The `page.rs` tests and the e2e layout check enforce this. A skill row draws its action icons after the measured
  name, sharing their rectangles with hit testing. The focused row also offers a native menu with the command names.
  After the popup returns, the window looks up the repository and skill again before acting: a refresh can reorder
  the rows or remove a command while the menu is open. Keyboard focus returns by skill identity rather than by the
  old row index.
- **`paint.rs`** draws each paint into one off-screen bitmap and copies it to the screen at once, so nothing flickers.
  Rounded boxes are anti-aliased by hand (GDI cannot); text is drawn by GDI with ClearType. Positions are in DIPs and
  become pixels at the window's DPI. A control has no background of its own: it asks its parent to draw what is under
  it into the same bitmap (`WM_PAINTBG`), so its corners blend with the card beneath.
- **`theme.rs`** holds the Windows 11 light and dark colours and the high contrast colours. It follows the app mode the
  user chose in Windows (`AppsUseLightTheme`); `TSS_THEME=dark` or `light` overrides it, for screenshots. The title bar
  and menus are asked to match through DWM and uxtheme, both loaded only when a window opens.
- **`page.rs`** builds every page as a `Surface` from what the app knows. It calls no Windows function (text is
  measured through a `Fit` the window passes in), so every layout and sentence is tested without a screen.
- **`window.rs`** is the main window: the band at the top, the repositories down the left, the page on the right.
  "Show changes" turns the whole window into the differences of one skill until Back (or Esc); the worker reads them
  with `engine::diff`, which only reads, and `diff.rs` lines them up from this PC (`−`) to GitHub (`+`), so they read
  as what taking GitHub's version would do. Only upstream's files and the files the record says this repository put
  there are compared; files the user added are never read or shown. "Preview" uses the same view (`DiffView.preview`)
  with nothing to choose: `engine::preview` asks GitHub nothing, reads only the files the record lists through
  `win::fs` on the worker, with the same size limits, and `diff::whole` shows each one whole with one column of line
  numbers. The window is made when opened and destroyed when
  closed (its fonts and bitmap released, the memory trimmed), so it costs nothing while closed. Its size is saved in
  `window.txt` when you finish resizing it.
- **The list on the left widens under the mouse** when a repository's name or a diff file's path or summary is cut
  (`page::nav_width`, `page::diff_files_width`): after the pointer has rested on it for 250 ms, taking room from the
  adjacent page or comparison. The repository list stops at half the window; the diff list at half the space shared
  by its two cards, excluding the margins and gap. The cards, headers and contents use the same `DiffLayout`.
  Selection, opened unchanged lines and scroll positions survive the resize. The list narrows when the pointer leaves;
  entering or leaving the diff view resets the hover timer and width. Nothing polls: every
  control and panel posts `WM_HOVER` to the window when the pointer enters or leaves it (`control::track_hover`), and
  the window then asks Windows where the pointer is. One gap: leaving from the list's scroll bar straight across the
  window's border sends no message, so the list stays wide until the next mouse move over the window.
- **`dialogs.rs`** asks questions in a small modal window drawn the same way, with a native edit box for typing.
  The client fits the monitor's work area after allowing for the frame. The panel scrolls to show the rest of the
  question and brings the focused control into view. It is not `TaskDialogIndirect`, which lives only in comctl32
  version 6; the test executables do not load that, so importing it would stop them from starting.
- **The icons** are the logo, which `logo.rs` describes as three shapes in a square from -1 to 1: a rounded tile, two
  sync arrows (a circle cut to two arcs, a round tail, a triangle head) and a four-point spark (four parabolas through
  the centre). `logo::draw` decides each pixel from 16 samples (4 by 4) inside or outside the shapes, so the edges are
  smooth at any size, and gives straight-alpha pixels, which `tray::icon` copies into a 32-bit icon. Nothing is scaled:
  each icon is drawn at the exact size Windows asks for. The tray draws the icon at the small-icon size of the
  taskbar's DPI, one picture per `Health` made the first time it is shown (solid green, amber or red, or hollow blue
  or grey: the colour says how things are), and draws them again when the display or its scale changes
  (`tray::rescale`, on `WM_DISPLAYCHANGE`, `WM_DPICHANGED`, `WM_SETTINGCHANGE` and `TaskbarCreated`). The window
  draws its big and small icons at its own DPI when it opens and when it moves to a screen with another scale. The
  drawing uses no trigonometry (the arrows' directions are written out as numbers, checked by a test): with `atan2`,
  `sin` and `cos` the tray exe was 16 KB larger. The files in `assets\` are made from the same code by a test-only PNG and
  ICO writer (`src\logo\files.rs`): `logo.svg`, `logo-16.png` to `logo-256.png`, `favicon.ico` (16, 32 and 48) and
  `tiny-skill-sync.ico` (16 to 256), which `build.rs` puts into both exes as their icon (about 13 KB). A test fails
  when a file there is not what the code draws; `scripts\dev.ps1 logo` writes them again.

A window procedure can be entered again while it runs (a control notifies its parent from inside the call that changed
it). So the UI state lives in thread-local `RefCell`s borrowed only with `try_borrow_mut`, and no borrow is held across a
call that runs a message loop (a question, the tray menu, `DestroyWindow`).

The band at the top says how everything is in one title, the worst first: a problem of the app, repositories that
can't be read, skills that need you, repositories that need you, the first check still to come, or all up to date. The
line under it says where, when the last check was, and when the next one is (`page::hero_of`). While the app works, or
just after it told you something, that line says so instead. After a check you asked for, it is `summary` in `app.rs`:
a failed check puts the reason first, and a check that found nothing new says "Checked: nothing new on GitHub" unless
all is well. Each skill header paints only its name, actions and status badge. Its window text also carries the
explanation (`Row.text`, `board.rs`) and known facts for the row tooltip, whether open or closed; hovering an
action shows that action's tooltip instead.
Only attention/error rows with resolution choices expand. Routine commands remain beside the name even while
those choices are open. The facts (`Row.facts`) come from the record after every job
(`engine::facts`, which reads nothing from disk) and are worded by `board::facts_text`; a plain row ("Up to date",
"Installed") that has them leaves out its sentence, which says no more than its tag. A skill that needs you starts
open. Each skill has 16 ids (`page::id::STRIDE`): its row, its parts and its buttons, so `page::hit` tells the skill and
the button from the id alone. The links name the repository's branch, or `HEAD` when none is set (`github::web_url`).
Text that came from a repository passes through `printable` before it is shown, so a name cannot carry
control or text-direction characters.

## Start with Windows

Windows starts the app at sign-in from one `REG_SZ` value, `Tiny Skill Sync`, under the user's
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` key: `"<path of tiny-skill-sync.exe>" --background`. Windows reads
that key once the desktop is up, at a low priority and after a delay of its own. Task Manager's Startup apps page and
Settings > Apps > Startup switch an entry off with a binary value of the same name under
`...\Explorer\StartupApproved\Run`, whose first byte is odd then.

`autostart.rs` decides, and is pure. `Startup::decide` turns the entry and that mark into one of five states: off, on,
switched off in Windows (the app's own entry with an odd mark), another copy (the entry starts an exe at another
path) and too long. The entry is matched by the program it starts, found the way Windows finds it (`program`), with
no case and in either form of this exe's path. `win\autorun.rs` reads and writes the registry:

- It reads only when the Settings page is shown, before the one-time per-machine offer after the first repository is
  added or first shared repositories are opened, and in `tss startup`. Never at start or on a timer.
- It writes only when the user turns the switch, accepts the offer or runs `tss startup on|off`, and never puts back
  an entry the user deleted or switched off. Turning it on writes the entry and deletes Windows' mark; turning it off
  deletes both, but only when the entry starts this copy.
- Windows starts no command over 260 characters from the `Run` key (`MAX_COMMAND`), so the entry names the exe by its
  long path when the command fits (a path of at most 245 characters), else by its short (8.3) path from
  `GetShortPathNameW`. When neither fits, the state is too long and the switch is greyed out. The `\\?\` prefix is
  never written.
- When a running copy resolves to a UNC network path, `win\autorun.rs` prefers that path for the startup entry and
  `build.rs` links both executables with `/SWAPRUN:NET`. This permits Windows to load the image from a network share;
  it does not make an unavailable share or an unsynced OneDrive file available at sign-in.
- A copy with an app folder of its own (`TSS_HOME`: the scripts and tests) keeps its entry and mark under
  `HKCU\Software\tiny-skill-sync-test\<instance>\`, which Windows never reads. `Keys::allowed` and `autorun::guard`
  refuse to write any other key, and refuse Windows' own keys from the tests and from such a copy whatever keys it is
  handed. `scripts\dev.ps1 clean` deletes `HKCU\Software\tiny-skill-sync-test`.

`--background` is all a start at sign-in changes: no window (unless settings can't be read, which the window
says), the 30-second wait described in [Threads and waking up](#threads-and-waking-up), and a second start while a copy
runs ends at once with 0 instead of opening that copy's window. `build.rs` writes a version resource into both exes
(about 1 KiB), so Task Manager names the entry "Tiny Skill Sync" rather than the exe's file name, and the logo as their
icon (`RT_ICON` and `RT_GROUP_ICON` from `assets\tiny-skill-sync.ico`), so Settings, Explorer and a taskbar pin show it.

## Measuring

When `TSS_BENCH_REPORT` names a file, the tray app appends timing marks to it (`src\win\ui\bench.rs`): the tray icon
shown, each finished job (with how long it took), the window painted, the list on the left widened or narrowed (`nav`
for repositories, `files` for diff files),
quit and exit. `scripts\bench.ps1` reads the marks together with Windows' process counters and compares them with the
budgets. Without the variable, nothing is written. `scripts\e2e.ps1` and `scripts\soak.ps1` read the same marks to know
when a job has finished; the first also reads and drives the real windows with Win32 messages and measures whether
text fits (`scripts\e2e-lib.ps1`). See [CONTRIBUTING.md](CONTRIBUTING.md#live-end-to-end-runs).

## Where to change what

| To change... | Look in |
| --- | --- |
| Which folders count as skills | `tree.rs` |
| What is installed, updated, deleted or held | `reconcile.rs` (and its tests first) |
| How a change is made on disk | `apply.rs` |
| How edits are found | `verify.rs`, `win\fs.rs` |
| Requests to GitHub | `github.rs`, `win\http.rs` |
| When checks happen | `scheduler.rs`, `app.rs` |
| Start with Windows | `autostart.rs` (what is written and what it means), `win\autorun.rs` (the registry), `app.rs` (the wait at sign-in) |
| What the user reads | `board.rs` (tray and window), `win\ui\page.rs` (the window's pages), `cli.rs` (`tss`) |
| How two copies of a file are compared | `text.rs` (when they are the same), `diff.rs` (Show changes) |
| The windows | `win\ui\window.rs`, `win\ui\tray.rs`, `win\ui\dialogs.rs` |
| How the windows look | `win\ui\control.rs` (controls), `win\ui\paint.rs` (drawing), `win\ui\theme.rs` (colours) |
| The logo and the icons | `logo.rs` (the shapes; then `scripts\dev.ps1 logo`), `win\ui\tray.rs` (the look of each state), `build.rs` (the exes' icon) |
