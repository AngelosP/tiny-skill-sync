# tiny-skill-sync

A tiny Windows tray app that keeps your user-level Copilot skills in sync with GitHub repositories.

You tell it which repositories (or which folder of a repository) hold skills. It installs them into your Copilot skills
folder, checks GitHub on a schedule, and installs, updates and removes skills as they change upstream. It never
touches a skill it did not install, and it never overwrites or deletes a skill you edited.

- **Small and quiet.** One 0.86 MiB exe, no runtime to install. It starts in about 8 ms, uses about 1 MiB of memory
  while idle, and does no work at all between checks: no CPU, no disk, no network.
- **Cheap checks.** An unchanged repository costs one small "has anything changed?" request to GitHub, which reads and
  writes no files.
- **Public and private repositories.** Public ones need nothing. Private ones use a GitHub token saved in Windows
  Credential Manager, or the token of the GitHub CLI (`gh auth login`).
- **A tray icon and a settings window**, plus a command line (`tss`) that does the same work one command at a time.

Windows 10 and 11, x64. Version 0.1: there is no installer yet, and the exe is not signed.

## Safety promises

These are the rules the app follows. Each one is covered by tests (see [CONTRIBUTING.md](CONTRIBUTING.md)).

1. **Your own skills are yours.** A folder in the skills folder that the app did not install is never changed or
   deleted, even if a repository has a skill with the same name. If its content is identical to upstream, the app takes
   it over; if not, it asks you.
2. **Your edits win.** If you change a skill the app installed and upstream changes too, the app changes nothing and
   asks you what to do. If you delete one, it does not put it back unless you ask.
3. **A failure is never a deletion.** No network, a wrong token, a missing repository, a rate limit or a strange answer
   from GitHub only means "could not check". Skills are removed only when GitHub returned a complete listing in which
   they are gone. If a repository suddenly lists no skills at all, nothing is deleted until you confirm.
4. **Nothing is lost.** A skill is downloaded and checked in full beside the skills folder before it replaces the old
   one. If the app or the computer stops half way, the next start finishes or undoes the change. Whenever the app must
   set aside something you might want, it keeps it as a backup and never deletes it.
5. **Edits made while the app was not running are found.** See [How edits are found](#how-edits-are-found).
6. **Downloads are never run.** Skills often contain scripts. The app only copies them.

## Quick start

1. Build it (see [CONTRIBUTING.md](CONTRIBUTING.md)): `scripts\dev.ps1 build`. This makes
   `target\release\tiny-skill-sync.exe` (the tray app) and `target\release\tss.exe` (the command line).
2. Copy both exes to a folder of your choice, for example `%LOCALAPPDATA%\Programs\tiny-skill-sync`, and start
   `tiny-skill-sync.exe`. A round icon appears in the notification area. The first start of a new exe can take up to a
   second while Microsoft Defender scans it.
3. Click the icon to open the settings window. Paste the address of a GitHub folder that holds skills into the box at
   the bottom of the repositories list, and click **Add**. For example:

   ```text
   https://github.com/anthropics/skills/tree/main/skills
   ```

4. The app installs the skills at once and then checks once a day.

To start the app when you sign in, put a shortcut to `tiny-skill-sync.exe` in your Startup folder (`Win+R`,
`shell:startup`). The app does not register itself.

## Which folders are skills

A skill is a folder with a `SKILL.md` file. For each repository you give the app one folder (the repository root if
you give only `owner/name`):

- If that folder has a `SKILL.md` itself, the whole folder is one skill, with everything under it.
- Otherwise each folder directly inside it that has a `SKILL.md` is a skill, with everything under it.

Folders whose name starts with a dot, loose files and folders without a `SKILL.md` are ignored.

> **Add the folder that holds the skills.** Many repositories keep their skills in a `skills` folder. If you add only
> `owner/name` for such a repository, the app finds no skills in the root and installs nothing. The repository then
> shows "No skills found", and the settings window and `tss status` say what to do: add the folder's address instead,
> `https://github.com/OWNER/NAME/tree/BRANCH/skills`.

A skill whose files include something unsafe on Windows (a link, a submodule, a reserved name such as `CON`, two names
that differ only in case, an over-long path) is not installed, and a version already installed is left as it is. The
reason is shown in the settings window and by `tss check` whenever a check reads the repository's listing: the first
check, each time the repository has changed, and `tss check --full-listing` (see [Limitations](#limitations)).

## The tray icon

| Icon | Meaning |
| --- | --- |
| Blue disc | All skills are up to date. |
| Blue ring | Checking for updates. |
| Grey ring | No repositories are set up. |
| Amber disc with `!` | A skill needs you (see below). |
| Red disc with `x` | A check failed. The tooltip and the settings window say why. |

The tooltip says what the state is. Right-click for **Open settings**, **Check now** and **Quit**. Starting
`tiny-skill-sync.exe` again opens the settings window of the copy that already runs; `tiny-skill-sync.exe --quit` closes
it.

## The settings window

- **Repositories:** each repository, its state and when it was last checked. For the selected one: how often to check
  it, **Use my token**, **Enabled**, **Check now** and **Remove…**. Removing asks whether to keep its skills (they become
  yours) or delete the ones you did not edit.
- **Add:** `owner/name` or the address of a GitHub folder. Tick **Private** to send your token.
- **Skills:** the skills of the selected repository and the state of each, with buttons for the choices that make sense.
  The box under the list explains the selected skill or, when none is selected, what went wrong with the repository
  (Tab moves to it, so a long text can be scrolled).
- **Default check interval**, the GitHub token (**Set…**, **Clear**) and the skills folder (**Open**, **Backups**).
- **Messages:** what changed recently, and what went wrong.

Every change is saved at once; there is no OK button. Intervals are a number and a unit: `30m`, `6h`, `1d`, `2w` or
`1mo` (a calendar month), at least 5 minutes. A repository without its own interval uses the default (1 day).

### When a skill needs you

| Choice | What it does |
| --- | --- |
| Replace with upstream | Install upstream's version. What is in the folder now is kept as a backup. |
| Restore | Bring back a skill you deleted. |
| Keep mine | The folder is yours: the app never changes or removes it again. |
| Leave it deleted | A skill you deleted stays deleted. |
| Manage again | Undo **Keep mine**. |
| Delete them | Upstream now lists no skills at all: remove the ones the app installed. |

Backups are folders named `backup-<skill>-<number>` in the work folder (the **Backups** button opens it). The app never
deletes them; delete them yourself when you no longer need them.

## Private repositories

Tick **Private** (or **Use my token**) for the repository, and give the app a token in one of two ways:

- Save a token in the settings window (**Set…**) or with `tss token set`. It is kept in Windows Credential Manager, for
  your user on this computer only. A fine-grained token with read-only **Contents** access to the repositories is
  enough.
- Or sign in to the GitHub CLI (`gh auth login`). When no token is saved, the app asks `gh auth token` for one. `gh` must
  be on the `PATH`. The app uses only the token; `gh` makes no requests for it.

The token is read when a check needs it. It is never written to the settings, the state or any message. Sending a token
also raises GitHub's rate limit: without one, every program on your network shares 60 requests an hour.

## The command line

`tss` does the same work as the tray, one command at a time, with the same safety rules. Run `tss help` for the details.

```text
tss status                                   what is installed and what needs attention (no network)
tss check [REPO] [--dry-run]                 check GitHub now and apply what is safe
tss add OWNER/NAME|URL [--every 6h] [--use-token]
tss remove REPO [--delete-skills]
tss resolve REPO SKILL replace|restore|keep|leave|manage
tss token [status|set|clear]                 gh auth token | tss token set
```

`REPO` is the id that `tss status` shows, or `owner/name`. Exit codes: 0 all fine, 1 something needs attention,
2 failed, 3 bad command line. The tray notices changes made with `tss` and shows them. Only one of them checks at a
time; the other one is told to try again in a moment.

Names and messages that come from a repository are shown with control and text-direction characters written out
(`\u{202e}`), in the window and in `tss`, so a name cannot hide part of a line or change how it reads. `tss` prints a
ready-to-paste `tss resolve` command only for a skill whose name is plain (letters, digits, spaces and a few marks such
as `-` and `_`), because Windows allows `$`, `;`, `%` and quotes in a folder name; for any other name it points to the
settings window.

Ctrl+C during `tss check` stops at once without a message. That is safe: the next start finishes or undoes a change that
was interrupted.

## Files

| What | Where |
| --- | --- |
| Settings | `%LOCALAPPDATA%\tiny-skill-sync\config.json` |
| What the app installed (the ownership record) | `%LOCALAPPDATA%\tiny-skill-sync\state.json` |
| Skills | `%USERPROFILE%\.copilot\skills`, unless `skills_dir` is set in `config.json` |
| Work folder: downloads in progress and backups | `.tss-work` beside the skills folder (`%USERPROFILE%\.copilot\.tss-work`) |

The skills folder contains only skill folders; the app keeps its own files elsewhere so Copilot never sees them. A
damaged `state.json` is kept as `state.json.damaged-*` and the app starts over with an empty record (it then takes over
skills that are identical to upstream and asks about the rest).

Pointing the app at another skills folder (`skills_dir`, or `TSS_SKILLS_DIR`) moves nothing. The new folder gets its own
copies at the next check, which the tray runs at once when the settings change; the skills in the old folder stay as
they are, are no longer kept up to date, and the app says so once. The record of the old folder is kept (for the last
four folders used), so going back to it carries on where it left off: edits made there in the meantime are still found
and kept, and a change that was interrupted there is finished then, not before. Until the app has taken in the new
folder (the tray does at once; with `tss` alone, run `tss check`), a choice about a skill or the removal of a repository
is refused with a request to check first.

`config.json` can be edited by hand while the app runs; it is picked up at once. An example:

```json
{
  "version": 1,
  "default_interval": "1d",
  "repos": [
    {
      "id": "anthropics-skills",
      "repo": "anthropics/skills",
      "branch": "main",
      "path": "skills",
      "use_token": false,
      "enabled": true
    },
    {
      "id": "me-private-skills",
      "repo": "me/private-skills",
      "branch": "",
      "path": "",
      "interval": "6h",
      "use_token": true,
      "enabled": true
    }
  ]
}
```

An empty `branch` means the default branch, and an empty `path` the repository root. Never change an `id`: the
installed skills are recorded under it. When the app or `tss` saves the settings, it writes only the fields above, so
anything else you added by hand is dropped.

Environment variables, mainly for tests and scripts:

| Variable | Meaning |
| --- | --- |
| `TSS_HOME` | Use this folder instead of `%LOCALAPPDATA%\tiny-skill-sync`. One copy of the tray runs per folder. |
| `TSS_SKILLS_DIR` | Use this skills folder, whatever the settings say. |
| `TSS_BENCH_REPORT` | Write timing marks to this file (used by `scripts\bench.ps1`). |
| `TSS_API_BASE`, `TSS_RAW_BASE` | Other GitHub addresses (used by the HTTP tests). |

## How edits are found

When the app installs a skill, it records the Git hash of every file and what Windows reports about it: its file id,
size, times and NTFS change number (USN). The change number moves whenever a file's content changes, even if the size
and every time stamp are put back.

- **On NTFS with a change journal** (the normal case for `C:`), the app compares these facts when it starts, when
  something in the skills folder changes while it runs, and when upstream has changed. An unchanged install is
  confirmed without reading a single file, and an edit made while the app was not running is still found.
- **On drives that cannot vouch** (no change journal, such as a network share; a journal that was re-created; a drive
  that does not answer), the app reads and hashes every file instead.
- **Before any update or deletion** the app always reads and hashes the files, and checks the folder once more after the
  change. A file changed in the meantime cancels the change and keeps your version.

Not tested: ReFS, Dev Drive, FAT and exFAT drives. A change made through a memory-mapped view that has not been
written to disk yet is not visible until it is; the full read before an update or deletion still catches it.

## Measured cost

Measured with `scripts\bench.ps1` on a Ryzen 7 5800X3D with Windows 11, files in the cache, real installs of
[anthropics/skills](https://github.com/anthropics/skills) (19 skills, 442 files) and
[github/awesome-copilot](https://github.com/github/awesome-copilot) (463 skills, 1,555 files).

| Measure | 442 files | 1,555 files | Budget |
| --- | --- | --- | --- |
| Start to tray icon | 8.0 ms | 8.3 ms | 30 ms median |
| Start-up check of every installed file | 23 ms | 100 ms | 75 µs per file |
| Idle for 10 s: CPU, disk, page faults | 0, 0, 0 | 0, 0, 0 | 0 |
| Idle memory: trimmed working set / private | 0.9 / 1.8 MiB | 1.2 / 2.5 MiB | 2 / 4 MiB |
| Settings window: open to painted (median) | 47 ms | 45 ms | 50 ms |
| Settings window open: working set | 11.5 MiB | 12.3 MiB | 20 MiB |
| Unchanged check: CPU, file reads and writes | 16 ms, 0, 0 | 31 ms, 0, 0 | 50 ms, 0 |
| Exe size | 0.87 MiB | 0.87 MiB | 1 MiB |

Windows counts a process's CPU time in ticks of 15.6 ms, so an unchanged check shows as one to three ticks (16 to
47 ms) from run to run. The settings window is the row with the least room (the budget is 50 ms, and the same build
varies by a few milliseconds from one day to the next), so run the bench again after any change to it. A list of 463
skills opens as fast as one of 19, because the list is filled with drawing turned off and painted once (filled row by
row, it took 68 ms).

Left running for 75 minutes against a test repository with a 5-minute interval and seven upstream changes
(`scripts\soak.ps1`), the app kept 6 threads and about 340 handles, used 297 ms of CPU in all (0.006 % of one core) and
ended at 3.2 MiB of private memory. Each change reached the disk within one interval (271 to 306 s). A second run of
22 minutes gave the same picture: 94 ms of CPU, 3.1 MiB private, changes on disk after 120, 60 and 305 s.

## Limitations

- No installer, no automatic start at sign-in, no signed exe, no automatic updates of the app itself.
- It is not checked yet whether a running Copilot session notices new or changed skills without a restart.
- A repository that is renamed or moved on GitHub is reported as a failure; update its address yourself.
- No dark mode, no icon in the exe file, and the tray icon is drawn for the screen scale at start.
- It uses the Windows proxy settings (WinHTTP's automatic proxy), but it has not been tested behind a proxy.
- A short network failure turns the icon red until the next successful check.
- `gh` is found only on the `PATH`; with several `gh` accounts, the active one is used.
- When Windows long paths are off, a folder you add inside a skill whose path is longer than 260 characters cannot be
  listed, so that skill counts as edited and is left alone.
- Skills that were refused as unsafe, and folders of yours that were held because they differ from an upstream skill of
  the same name, are announced by the check that reads the repository's listing (the first check, each time the
  repository has changed, and `tss check --full-listing`). The app does not remember them: a later check that finds
  nothing new says "Up to date", and the settings window forgets a refusal when the app restarts. Nothing is installed
  or changed in the meantime.
- GitHub's servers can answer for up to a minute with a version of a public repository that is one change old. When
  two of its answers disagree, the check stops with "Changed while checking" and changes nothing; the next check works.
- One check downloads at most 512 MiB (the rest waits for the next check), and a file over 64 MiB or a skill with more
  than 10,000 files is refused. There is no limit on what all the installed skills add up to.
- The settings window is 616 by 619 pixels at 100% scale and does not shrink. It has been tried only at 100%; on a small
  screen with a high scale (for example 1366 by 768 at 125%) part of it is off the screen.
- Changing the skills folder does not move installed skills (see [Files](#files)). Once more than four other folders
  have been used, the oldest record is forgotten; going back to that folder then works like a damaged `state.json`.
  Two names for one folder that only the disk could tell apart (a short 8.3 name, a mapped drive, a link) count as two
  folders.

## More

- [CONTRIBUTING.md](CONTRIBUTING.md): building, testing and measuring.
- [ARCHITECTURE.md](ARCHITECTURE.md): how it works inside, with diagrams.
- [AGENTS.md](AGENTS.md): rules for AI agents and people changing the code.
- [spikes/README.md](spikes/README.md): the feasibility experiments the design is based on.
