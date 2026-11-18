# tiny-skill-sync

A tiny Windows tray app that keeps your user-level Copilot skills in sync with GitHub repositories.

You tell it which repositories (or which folder of a repository) hold skills. It installs them into your Copilot skills
folder, checks GitHub on a schedule, and installs, updates and removes skills as they change upstream. It never
touches a skill it did not install, and it never overwrites or deletes a skill you edited. It removes from your PC only
what came from GitHub and was later removed there; files you add are yours, and the app does not even show them.

- **Small and quiet.** One exe of 0.9 MiB, no runtime to install. It starts in about 8 ms, uses about 1 MiB of
  memory while idle, and does no work at all between checks: no CPU, no disk, no network.
- **Cheap checks.** An unchanged repository costs one small "has anything changed?" request to GitHub, which reads and
  writes no files.
- **Public and private repositories.** Public ones need nothing. Private ones are read with the GitHub CLI's sign-in
  (`gh auth login`) or a token saved in Windows Credential Manager, without any setting to turn on.
- **A tray icon and a window** that shows every repository and skill, says in plain words what needs you, and shows
  line by line what is different between GitHub and your copy of a skill. It follows the Windows light or dark mode.
- **A command line** (`tss`) that does the same work one command at a time.

Windows 10 and 11, x64. Version 0.1: there is no installer yet, and the exe is not signed.

## Safety promises

These are the rules the app follows. Each one is covered by tests (see [CONTRIBUTING.md](CONTRIBUTING.md)).

1. **What you add is yours.** A folder in the skills folder that the app did not install is never changed or deleted,
   even if a repository has a skill with the same name. If every file upstream has is in it, the same, the app takes it
   over (anything else in it stays yours); if not, it asks you. A file you add inside a skill the app installed is
   yours too: it is never changed or deleted, never counted as an edit and never shown, and it stays when the skill is
   updated. The app removes only what came from GitHub and was later removed there: when upstream drops a skill, the
   files it installed go and the files you added stay. If upstream later adds a file with the same name as one of
   yours, GitHub's file goes in, yours is kept as a backup, and Activity says which file it was and where the backup
   is.
2. **Your edits win.** If you change or delete a file the app installed and upstream changes that skill too, the app
   changes nothing and asks you what to do. If you delete a whole skill, it does not put it back unless you ask. Other
   line endings and added or removed blank lines are not edits (see
   [What counts as the same](#what-counts-as-the-same)).
3. **A failure is never a deletion.** No network, a wrong token, a missing repository, a rate limit or a strange answer
   from GitHub only means "could not check". Skills are removed only when GitHub returned a complete listing in which
   they are gone. If a repository suddenly lists no skills at all, nothing is deleted until you confirm.
4. **Nothing is lost.** A skill is downloaded and checked in full beside the skills folder before it replaces the old
   one. If the app or the computer stops half way, the next start finishes or undoes the change. Whenever the app must
   set aside something you might want, it keeps it as a backup and never deletes it.
5. **Edits made while the app was not running are found.** See [How edits are found](#how-edits-are-found).
6. **Downloads are never run.** Skills often contain scripts. The app only copies them.

## Quick start

1. Download `tiny-skill-sync-VERSION-windows-x64.zip` from the
   [releases](https://github.com/AngelosP/tiny-skill-sync/releases), or let the GitHub CLI fetch the latest one (it
   also works while the repository is private to you):
   `gh release download --repo AngelosP/tiny-skill-sync --pattern *`. `SHA256SUMS.txt` holds the SHA-256 of the zip
   and of both exes. Or build them yourself (see [CONTRIBUTING.md](CONTRIBUTING.md)): `scripts\dev.ps1 build` makes
   them in `target\release`.
2. Put both exes, `tiny-skill-sync.exe` (the tray app) and `tss.exe` (the command line), in a folder of your choice, for
   example `%LOCALAPPDATA%\Programs\tiny-skill-sync`, and start `tiny-skill-sync.exe`. A round icon appears in the
   notification area. The first start of a new exe can take up to a second while Microsoft Defender scans it. The exes
   are not signed, so if a browser downloaded them, Windows may ask before the first start (**More info**, **Run
   anyway**).
3. Click the icon to open the window. Click **Add repository**, paste the address of a GitHub folder that holds skills,
   and click **Add**. For example:

   ```text
   https://github.com/anthropics/skills/tree/main/skills
   ```

4. The app installs the skills at once and then checks once a day.

To start the app when you sign in, put a shortcut to `tiny-skill-sync.exe` in your Startup folder (`Win+R`,
`shell:startup`). The app does not register itself.

To update it, quit the app (right-click the icon, **Quit**), replace both exes with the new ones and start it again.
The settings and the record of what it installed stay in `%LOCALAPPDATA%\tiny-skill-sync`.

## Which folders are skills

A skill is a folder with a `SKILL.md` file. For each repository you give the app one folder (the repository root if
you give only `owner/name`):

- If that folder has a `SKILL.md` itself, the whole folder is one skill, with everything under it.
- Otherwise each folder directly inside it that has a `SKILL.md` is a skill, with everything under it.

Folders whose name starts with a dot, loose files and folders without a `SKILL.md` are ignored.

> **Add the folder that holds the skills.** Many repositories keep their skills in a `skills` folder. If you add only
> `owner/name` for such a repository, the app finds no skills in the root and installs nothing. The repository then
> shows "No skills found", and the window and `tss status` say what to do: add the folder's address instead,
> `https://github.com/OWNER/NAME/tree/BRANCH/skills`.

A skill whose files include something unsafe on Windows (a link, a submodule, a reserved name such as `CON`, two names
that differ only in case, an over-long path) is not installed, and a version already installed is left as it is. The
reason is shown in the window and by `tss check` whenever a check reads the repository's listing: the first
check, each time the repository has changed, and `tss check --full-listing` (see [Limitations](#limitations)).

## The tray icon

| Icon | Meaning |
| --- | --- |
| Blue disc | All skills are up to date. |
| Blue ring | Checking for updates. |
| Grey ring | No repositories are set up. |
| Amber disc with `!` | A skill needs you (see below). |
| Red disc with `x` | A check failed. The tooltip and the window say why. |

The tooltip says what the state is. Click the icon to open the window, or right-click for **Open Tiny Skill Sync**,
**Sync now** and **Quit**. Starting `tiny-skill-sync.exe` again opens the window of the copy that already runs;
`tiny-skill-sync.exe --quit` closes it.

## The window

The band across the top says how everything is in one sentence ("All skills are up to date", "1 skill needs your
attention", "1 repository can't be read"). The line under it counts the skills and repositories and says when they were
checked and when the next check is; after something you asked for, it says what came of it ("Checked: skills were
updated", "Checked: nothing new on GitHub"). **Sync now** gets new and updated skills from every repository at once.
Skills you edited or deleted are never changed by it. Hover over it to read the same.

Down the left are your repositories, each with its state, and **Add repository**, **Activity** and **Settings**. When a
name is too long to show whole, rest the mouse on the list for a moment: it widens, up to half the window, taking the
room from the page beside it, and narrows again when the mouse leaves.

- **A repository:** its name (with a **Private** label when it is read with your token), the folder and branch its
  skills come from, and when it was last checked.
  - When it can't be read, a box says why, with **Try again** and **Open Settings** (where the GitHub sign-in is).
  - **Check every** sets how often it is checked: **Default** (the interval in Settings), a common interval, or
    **Custom…**.
  - **Keep in sync** off pauses it: it is not checked, and nothing in its skills is changed.
  - The **More** button holds **Sync this repository**, **Open on GitHub**, **Open skills folder** and **Remove
    repository…**. Removing asks whether to leave its skills installed (they become yours) or to delete them too
    (skills you edited are kept, and files you added stay where they are).
- **Its skills,** each with its state. A skill that needs you is open already; click any other to open it. An open
  skill says what is going on, and offers the choices that make sense (see below), **Show changes** and **Open folder**.
- **Show changes** turns the window into the differences between the GitHub version of the skill and the copy on this
  PC, shown as what taking the GitHub version would do to the copy on this PC. Nothing changes until you choose.
  - The files are listed on the left, each marked "Only on GitHub" (`+`, it would be added), "Only on this PC" (`−`,
    GitHub removed it since it was installed, so it would go), "Changed · 3 added, 1 removed", "Only invisible changes"
    (only a byte order mark is different) or "Different (not text)". Line endings and blank lines don't count here
    either: a file that differs only in them is not listed, and blank lines are not shown.
  - Only GitHub's files and the files the app installed are compared. Files you added are never listed: they are yours,
    and taking the GitHub version leaves them where they are.
  - Rest the mouse on the file list for a quarter of a second to widen it for long paths. It takes room from the
    comparison, without resizing the window, and narrows when the mouse leaves. Your selected file and scroll
    positions stay put.
  - The chosen file's lines are on the right: `+` on green for a line only on GitHub, `−` on red for a line only on this
    PC, with three lines around each change. Long unchanged stretches are folded.
  - The skill's choices are at the top. **Back** (or Esc) returns.
  - Only the files whose hash differs are read or downloaded, and nothing is written. A file over 1 MiB, files beyond
    4 MiB from either side, and links (never followed) are named but not shown.
- **Activity:** what the app did recently, newest first, and what went wrong.
- **Settings:**
  - **Check for updates every**: the default interval, for every repository set to **Default** (1 day at first).
  - **Skills folder** and **Backups**, each with **Open**.
  - **GitHub sign-in for private repositories**: **Save a token…**, and once one is saved, **Replace token…** and
    **Delete token**.
  - **About**.

Every change is saved at once; there is no OK button. Intervals are a number and a unit: `30m`, `6h`, `1d`, `2w` or
`1mo` (a calendar month), at least 5 minutes. **Custom…** also takes words, such as `45 minutes` or `3 days`.

The window follows the Windows light or dark mode, and high contrast. It can be resized and remembers its size
(`window.txt` in the app's folder). Esc closes it; the app keeps running in the tray. The window is created when it
opens and destroyed when it closes, so it costs nothing while closed.

### When a skill needs you

| Choice | What it does |
| --- | --- |
| Use the GitHub version | Install upstream's version. The files it replaces are kept as a backup; files you added stay. |
| Restore | Bring back a skill you deleted. |
| Keep my version | The folder is yours: the app never changes or removes it again. |
| Leave it deleted | A skill you deleted stays deleted. |
| Manage again | Undo **Keep my version**. |
| Delete them | Upstream now lists no skills at all: remove the ones the app installed. |

Backups are folders named `backup-<skill>-<number>` in the work folder (**Backups** in Settings opens it). The app never
deletes them; delete them yourself when you no longer need them.

## Private repositories

There is nothing to turn on. A repository is read without a token first, so a public one never makes the app look for
a token. When GitHub answers "not found" (the repository may be private) or the limit for requests without a token is
reached, the app reads it again with your token, and remembers that the repository needs it. Give the app a token in
one of two ways:

- Sign in to the GitHub CLI (`gh auth login`). When no token is saved, the app asks `gh auth token` for one. `gh` must
  be on the `PATH`. The app uses only the token; `gh` makes no requests for it.
- Or save a token in Settings (**Save a token…**) or with `tss token set`. It is kept in Windows Credential Manager,
  for your user on this computer only. A fine-grained token with read-only **Contents** access to the repositories is
  enough. A saved token is used before the GitHub CLI's.

Without either, a private repository shows "Can't read this repository" with what to do. To send the token with every
request to a repository, even the first, add it with `tss add --use-token` (or set `use_token` in `config.json`).

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
app's window.

Ctrl+C during `tss check` stops at once without a message. That is safe: the next start finishes or undoes a change that
was interrupted.

## Files

| What | Where |
| --- | --- |
| Settings | `%LOCALAPPDATA%\tiny-skill-sync\config.json` |
| What the app installed (the ownership record) | `%LOCALAPPDATA%\tiny-skill-sync\state.json` |
| The window's size | `%LOCALAPPDATA%\tiny-skill-sync\window.txt` |
| Skills | `%USERPROFILE%\.copilot\skills`, unless `skills_dir` is set in `config.json` |
| Work folder: downloads in progress and backups | `.tss-work` beside the skills folder (`%USERPROFILE%\.copilot\.tss-work`) |

The skills folder contains only skill folders; the app keeps its own files elsewhere so Copilot never sees them. A
damaged `state.json` is kept as `state.json.damaged-*` and the app starts over with an empty record (it then takes over
skills that hold every upstream file unchanged and asks about the rest).

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

An empty `branch` means the default branch, and an empty `path` the repository root. `use_token` sends the token with
every request to that repository; without it, a private repository is still read with the token (see
[Private repositories](#private-repositories)). Never change an `id`: the installed skills are recorded under it. When
the app or `tss` saves the settings, it writes only the fields above, so anything else you added by hand is dropped.

Environment variables, mainly for tests and scripts:

| Variable | Meaning |
| --- | --- |
| `TSS_HOME` | Use this folder instead of `%LOCALAPPDATA%\tiny-skill-sync`. One copy of the tray runs per folder. |
| `TSS_SKILLS_DIR` | Use this skills folder, whatever the settings say. |
| `TSS_BENCH_REPORT` | Write timing marks to this file (used by `scripts\bench.ps1`). |
| `TSS_API_BASE`, `TSS_RAW_BASE` | Other GitHub addresses (used by the HTTP tests). |

## How edits are found

When the app installs a skill, it records the Git hash of every file (and for a text file a hash of its lines that
count, see below) and what Windows reports about it: its file id, size, times and NTFS change number (USN). The change
number moves whenever a file's content changes, even if the size and every time stamp are put back. Only those files
are looked at: a file you add to a skill is never counted as an edit.

- **On NTFS with a change journal** (the normal case for `C:`), the app compares these facts when it starts, when
  something in the skills folder changes while it runs, and when upstream has changed. An unchanged install is
  confirmed without reading a single file, and an edit made while the app was not running is still found.
- **On drives that cannot vouch** (no change journal, such as a network share; a journal that was re-created; a drive
  that does not answer), the app reads and hashes every file instead.
- **Before any update or deletion** the app always reads and hashes the files, and checks the folder once more after the
  change. A file changed in the meantime cancels the change and keeps your version.

Not tested: ReFS, Dev Drive, FAT and exFAT drives. A change made through a memory-mapped view that has not been
written to disk yet is not visible until it is; the full read before an update or deletion still catches it.

### What counts as the same

Two copies of a text file are the same when they differ only in how lines end (`\r\n` or `\n`), in the line break at
the end of the file, or in blank lines (lines of nothing but spaces and tabs). A copy saved by an editor that changes
line endings, or with a blank line added or taken out, is still the same skill. This holds everywhere: finding edits,
updating and removing a skill, taking over a folder of yours, and **Show changes**.

- Spaces at the end of a line that has text, and a byte order mark, still count.
- A file that is not text (not UTF-8, or with a NUL character in it), or that is over 1 MiB, is compared byte for byte.
- In Markdown a blank line can separate two paragraphs; taking it out does not count as an edit either.
- Skills installed by version 0.2 or older are recorded without the hash of their lines. The app learns it at its first
  start, from each file that is still exactly as installed. A file already changed in any way by then counts as edited
  until you choose what to do with it.

## Measured cost

Measured with `scripts\bench.ps1` on a Ryzen 7 5800X3D with Windows 11, files in the cache, real installs of
[anthropics/skills](https://github.com/anthropics/skills) (19 skills, 442 files) and
[github/awesome-copilot](https://github.com/github/awesome-copilot) (463 skills, 1,555 files).

| Measure | 442 files | 1,555 files | Budget |
| --- | --- | --- | --- |
| Start to tray icon | 8.4 ms | 8.6 ms | 30 ms median |
| Start-up check of every installed file | 21 ms | 105 ms | 75 µs per file |
| Idle for 10 s: CPU, disk, page faults | 0, 0, 0 | 0, 0, 0 | 0 |
| Idle memory: trimmed working set / private | 1.0 / 1.9 MiB | 1.2 / 2.6 MiB | 2 / 4 MiB |
| Window: open to painted (median) | 41 ms | 43 ms | 100 ms |
| Window open: working set | 13.5 MiB | 14.4 MiB | 40 MiB |
| Window closed: working set (trimmed) | 1.5 MiB | 1.5 MiB | 2 MiB |
| Unchanged check: CPU, file reads and writes | 31 ms, 0, 0 | 16 ms, 0, 0 | 50 ms, 0 |
| Exe size | 0.91 MiB | 0.91 MiB | 1 MiB |

Windows counts a process's CPU time in ticks of 15.6 ms, so an unchanged check shows as one to three ticks (16 to
47 ms) from run to run. The exe is built for size: built for speed it would be 1.01 MiB, over its budget, and hash
files about 13% faster (384 instead of 339 MB/s). A list of 463 skills opens almost as fast as one of 19, because only
the rows within about two screens of what is shown are made at first; the rest are made as you scroll (with a row made
for every skill, it took 120 ms). Widening the list of repositories under the mouse takes 18 ms with those 463 skills
on the page.

Left running for 75 minutes against a test repository with a 5-minute interval and seven upstream changes
(`scripts\soak.ps1`), the app kept 6 threads and about 340 handles, used 297 ms of CPU in all (0.006 % of one core) and
ended at 3.2 MiB of private memory. Each change reached the disk within one interval (271 to 306 s). A second run of
22 minutes gave the same picture: 94 ms of CPU, 3.1 MiB private, changes on disk after 120, 60 and 305 s.

## Limitations

- No installer, no automatic start at sign-in, no signed exe, no automatic updates of the app itself.
- It is not checked yet whether a running Copilot session notices new or changed skills without a restart.
- A repository that is renamed or moved on GitHub is reported as a failure; update its address yourself.
- No icon in the exe file, and the tray icon is drawn for the screen scale at start.
- It uses the Windows proxy settings (WinHTTP's automatic proxy), but it has not been tested behind a proxy.
- A short network failure turns the icon red until the next successful check.
- `gh` is found only on the `PATH`; with several `gh` accounts, the active one is used.
- When Windows long paths are off, a folder you add inside a skill whose path is longer than 260 characters cannot be
  read by the app, so the files you added could not be kept with the skill. That skill is then not updated or removed:
  it is held, with "the skill folder could not be read".
- When upstream adds a file with the same name as one you added, the update goes ahead without asking. Your file is in
  the backup of that update, and Activity (and `tss check`) names it and the backup.
- Skills that were refused as unsafe are announced by the check that reads the repository's listing (the first check,
  each time the repository has changed, and `tss check --full-listing`). The app does not remember them: a later check
  that finds nothing new says "Up to date", and the window forgets a refusal when the app restarts. Nothing is
  installed or changed in the meantime. Folders of yours that were held because they differ from an upstream skill of
  the same name are different: until that is settled, the repository is read in full at every check, and once
  straight after the app starts, so the window always shows them.
- GitHub's servers can answer for up to a minute with a version of a public repository that is one change old. When
  two of its answers disagree (a file the listing names is different or cannot be found), the check stops with
  "Changed while checking" and changes nothing, and **Show changes** and **Use the GitHub version** say "the
  repository changed on GitHub while (the file) was being read; try again". The next try works.
- One check downloads at most 512 MiB (the rest waits for the next check), and a file over 64 MiB or a skill with more
  than 10,000 files is refused. There is no limit on what all the installed skills add up to.
- The window opens at 980 by 680 at 100% scale (smaller if the screen has less room) and can be made as small as 640
  by 480. It has been tried only at 100% scale.
- The window's controls are drawn by the app. Each is a real window with its text, and Tab, Enter, Space and Esc work,
  but they do not tell a screen reader what kind of control they are.
- Changing the skills folder does not move installed skills (see [Files](#files)). Once more than four other folders
  have been used, the oldest record is forgotten; going back to that folder then works like a damaged `state.json`.
  Two names for one folder that only the disk could tell apart (a short 8.3 name, a mapped drive, a link) count as two
  folders.

## More

- [CONTRIBUTING.md](CONTRIBUTING.md): building, testing and measuring.
- [ARCHITECTURE.md](ARCHITECTURE.md): how it works inside, with diagrams.
- [AGENTS.md](AGENTS.md): rules for AI agents and people changing the code.
- [spikes/README.md](spikes/README.md): the feasibility experiments the design is based on.
