# Tiny Skill Sync App: user guide

[Back to the short introduction](../README.md)

A tiny Windows tray app that keeps your user-level Copilot skills, agents, modular instructions and GitHub Copilot CLI
plugins in sync with GitHub.

You tell it which repositories (or which folder of a repository) hold skills. It installs them into your Copilot skills
folder, checks GitHub on a schedule, and installs, updates and removes skills as they change upstream. It adopts an
existing copy only after proving it matches upstream history, and never overwrites or deletes an edited skill without
your choice. It removes from your PC only
what came from GitHub and was later removed there; files you add are yours, and the app does not even show them.

- **Small and quiet.** One exe of about 1.4 MiB, no runtime to install. It starts in about 9 ms, uses about 1 MiB of
  memory while idle, and does no work at all between checks: no CPU, no disk, no network.
- **Starts with Windows, without slowing it down.** Turn on **Start with Windows** (the app offers it when you add your
  first repository). At sign-in it goes straight to the tray, reads settings on the worker, then waits 30 seconds
  before it looks at your skills or checks GitHub, and then does both at low priority.
- **Cheap checks.** An unchanged repository costs one small "has anything changed?" request to GitHub, which reads and
  writes no files.
- **Public and private repositories.** Public ones need nothing. Private ones are read with the GitHub CLI's sign-in
  (`gh auth login`) or a token saved in Windows Credential Manager, without any setting to turn on.
- **A tray icon and a window** that shows every repository and skill, says in plain words what needs you, and shows
  line by line what is different between GitHub and your copy of a skill. An open skill also shows its version and
  size, a preview of its files, its folder and its address on GitHub. It follows the Windows light or dark mode.
- **A command line** (`tss`) that does the same work one command at a time.

Windows 10 and 11, x64. There is no installer yet, and the exes are not signed.

## Safety promises

These are the rules the app follows. Each one is covered by tests (see [CONTRIBUTING.md](../CONTRIBUTING.md)).

1. **What you add is yours.** A folder the app does not own is adopted only when every file in the current upstream
   version, or in one single past version from the configured branch's history, is present and the same under the
   existing text comparison rules. A folder matching a past version may then be updated automatically; otherwise it
   is held. That past version may have fewer files than today's version; anything else in the folder stays yours.
   A file you add inside a skill the app installed is
   yours too: it is never changed or deleted, never counted as an edit and never shown, and it stays when the skill is
   updated. The app removes only what came from GitHub and was later removed there: when upstream drops a skill, the
   files it installed go and the files you added stay. If upstream later adds a file with the same name as one of
   yours, GitHub's file goes in, yours is kept as a backup, and Activity says which file it was and where the backup
   is.
2. **Your edits win.** If you change or delete a file the app installed and upstream changes that skill too, the app
   changes nothing and asks you what to do. If you delete a whole skill, it does not put it back unless you ask. Other
   line endings and added or removed blank lines are not edits (see
   [What counts as the same](#what-counts-as-the-same)). If you made your edit on GitHub too, so that your folder is
   exactly what GitHub has now, there is nothing to decide: the app marks the skill as in sync and writes nothing
   (see [An edit that GitHub has too](#an-edit-that-github-has-too)).
3. **A failure is never a deletion.** No network, a wrong token, a missing repository, a rate limit or a strange answer
   from GitHub only means "could not check". Skills are removed only when GitHub returned a complete listing in which
   they are gone. If a repository suddenly lists no skills at all, nothing is deleted until you confirm.
4. **Nothing is lost.** A skill is downloaded and checked in full beside the skills folder before it replaces the old
   one. If the app or the computer stops half way, the next start finishes or undoes the change. Whenever the app must
   set aside something you might want, it keeps it as a backup and never deletes it.
5. **Edits made while the app was not running are found.** See [How edits are found](#how-edits-are-found).

### Copies installed before the app

The app compares today's version first, then makes a small quick search through the skill's earlier versions on
the configured branch. One complete matching version is required: mixing files from different versions never counts.
A proven old copy is adopted at that old version and then updated with the normal full checks. No backup is needed
for those proven upstream files; your extra files still move with the skill, and a name collision keeps yours in a backup.
The window, command line and Activity say an older copy was adopted and updated only after the update succeeds.

Longer histories continue automatically in small pieces on the tray's existing worker, with **Searching history**
shown beside the skill. There is no total-history cutoff. Only that skill waits; other safe changes can proceed.
**Cancel search** pauses that skill persistently, even after restarting the app. **Resume search** continues it.
**Use the GitHub version** stops the search and replaces the copy with a backup; **Keep my version** stops the search
and leaves your copy alone. Network and sign-in failures stop the repository check, never count as a failed match,
and are retried after a wait.

Active searches resume after restarting. Finished or paused searches make no extra history requests while unchanged.
`tss check` does one piece of work and exits: it does not start a hidden background process. If the tray is not running,
run `tss check` again to continue. Removing a repository leaves its unowned copies alone; explicitly adding it again
allows the same current-or-historical adoption checks.

## Quick start

1. Download `tiny-skill-sync-VERSION-windows-x64.zip` from the
   [releases](https://github.com/AngelosP/tiny-skill-sync/releases), or let the GitHub CLI fetch the latest one (it
   also works while the repository is private to you):
   `gh release download --repo AngelosP/tiny-skill-sync --pattern *`. `SHA256SUMS.txt` holds the SHA-256 of the zip
   and of both exes. Or build them yourself (see [CONTRIBUTING.md](../CONTRIBUTING.md)): `scripts\dev.ps1 build` makes
   them in `target\release`.
2. Put both exes, `tiny-skill-sync.exe` (the tray app) and `tss.exe` (the command line), in a folder of your choice, for
   example `%LOCALAPPDATA%\Programs\tiny-skill-sync`, and start `tiny-skill-sync.exe`. Its icon, a tile with two
   sync arrows round a spark, appears in the
   notification area. The first start of a new exe can take up to a second while Microsoft Defender scans it. The exes
   are not signed, so if a browser downloaded them, Windows may ask before the first start (**More info**, **Run
   anyway**).
3. Click the icon to open the window. Click **Add source**, paste the address of the GitHub folder, choose
   **Skills** (the default), **Agents**, **Instructions** or **Plugins**, and click **Preview**. For example:

   ```text
   https://github.com/anthropics/skills/tree/main/skills
   ```

4. Check the type, matches and destination. **Cancel** saves and installs nothing. If the preview was rejected
   or found nothing, use **Retry** with the same details, or **Edit** to change them. If a source attempt is rejected
   (for example, because that source is already set up), the next **Add source** attempt keeps the address and type
   you entered. **Add and check**
   saves the source and starts the first check, then checks once a day by default. Esc does not cancel a save
   already in progress. The app also asks whether to start with Windows.

### Updating the app

**Sync now** and `tss check` update *skills*. **Settings → App updates** and `tss update` concern only the two programs.
While the tray runs it checks for a newer stable Windows x64 release from `AngelosP/tiny-skill-sync`, even if
there are no skill repositories. The default is every 24 hours; the alternatives are 7 and 30 days.

| What you want | Command |
|---|---|
| See preferences and installed version, without checking GitHub | `tss update status` |
| See whether a newer version is offered, without downloading the programs | `tss update check` |
| Check, verify and install both files now | `tss update install` |
| Turn automatic checks and pending automatic installs off or back on | `tss update off` / `tss update on` |
| Set the interval | `tss update every 1d`, `7d` or `30d` |
| Choose when to change the files | `tss update when immediate` or `tss update when quit` |

Manual check and install still work with automation off. When the tray installs an update, by **Install now** or
an Immediate install (the default), it restarts into the new version once both files are replaced and the new tray
matches its release digest. If the window was open, it opens again on its first page, and the line at the top says
the tray restarted. A question, the tray menu or a sign-out that is under way delays the restart until it is over;
Quit, `--quit` or signing out in the meantime cancels it, and the app stays closed. If the new tray cannot be started
or stops at once, the old one keeps running and says why.
`tss update install` changes only the files: a running tray keeps its old code until you quit and start it
yourself. In Quit mode the worker holds both verified files until you explicitly choose **Quit**, and the tray
stays closed afterwards. Closing Settings is not Quit, and signing out does not install a pending update.

The updater does not use a skill token or a configurable release feed. Both standalone files must have the expected
sizes and SHA-256 digests from GitHub before either is replaced. The installed pair must also match both digests
in its own official release: a custom build, edited file, or tag without standalone digests is left alone. These
checks detect changes; the programs are not publisher-signed. An unavailable share, access error or busy folder
is a failure, not an update. A lock on a share protects one folder; it cannot lock another PC's separate OneDrive
replica. Use a single writer and wait for both files to sync before using another PC. Live SMB and OneDrive
replacement have not been verified.

**First upgrade or manual recovery:** for 0.6.0 and older, quit the tray and close other copies of `tss` using this
folder. Put **both** exes from the same official release ZIP in their place and start the tray yourself. Leave
the companion settings, local skill records and startup entry alone. If an automatic replacement was interrupted,
let the same copy recover using its original, private settings home. If it reports a conflict, preserve the
installation folder and that home, including their recovery files; use a fresh folder with both programs from
the ZIP rather than guessing which file to overwrite. A skipped on-Quit install is not scheduled for later by a
disk record: the next session must obtain and verify a release again.

### Run from a shared folder

Both exes can live in a folder available to several Windows machines. On the machine whose local configuration you
want to copy, run:

```powershell
tss portable
```

This creates two files beside `tss.exe` and refuses to replace either one if it already exists:

- `tiny-skill-sync.settings.json` holds the default skill interval, optional skills folder and app-update preferences.
- `tiny-skill-sync.sources.json` holds the sources, including their IDs, type, branch, folder, enabled state,
  interval and `use_token` choice.

The files are deliberately separate, so you can copy or update one without replacing the other. The command does not
change the local `config.json`. To change shared settings later, edit the companion JSON files or copy in reviewed
versions; `tss portable` is an export, not a merge command. Each file has a format version, and an unreadable,
unsupported or malformed companion is reported rather than silently ignored.

The settings and sources are shared; `state.json`, `engine.lock`, window size, the one-time startup-offer marker,
mutex, saved GitHub token, destination bindings and work folders remain local to each machine. The default skills folder is resolved
separately on each machine as `%USERPROFILE%\.copilot\skills`. A portable `skills_dir` may use Windows environment
variables such as `%USERPROFILE%`; they are expanded locally and the stored value is not rewritten. `TSS_SKILLS_DIR`
still overrides it on that machine. Agents and Instructions use their own local Copilot folders, not paths derived
from `skills_dir`. Do not share destinations or work folders between machines. Skills-only data can remain in
format 1. Typed local settings, portable sources and their ownership records use format 2; upgrade older copies
before adding a typed source. Do not relabel format-2 files as format 1 to use an older copy.

The app reads and writes companion files on its worker, so a slow or unavailable share does not block the window.
Changes made on another machine are picked up at the next poll, **Sync now** or check, or when the window opens. There
is no cross-machine lock or transaction across the two files: avoid editing them from multiple machines at once, and
check for OneDrive conflict copies before continuing. If the share is unavailable, the check fails and installed
skills are left alone.

### Agents and modular instructions

Select one type per source. **Agents** looks only for regular `*.agent.md` files directly in the folder you gave it.
**Instructions** looks only for regular `*.instructions.md` files there. It does not import `AGENTS.md`,
`copilot-instructions.md`, unrelated Markdown or files in subfolders.

The default destinations are `%USERPROFILE%\.copilot\agents` and `%USERPROFILE%\.copilot\instructions`. The app
can reuse a single existing layout, either flat in that folder or in a child named after the GitHub repository.
When both layouts contain matching names, the app holds the source rather than making more copies. Once bound,
the source stays in that layout; a blocked file does not move it or stop safe sibling changes. A file is adopted
only when its contents match upstream under the normal text rules. It is not adopted just because its name matches.

Loose files are tracked individually. The app never removes their containing folder or an unrelated file. It
needs a complete, safe listing confirmed at a GitHub commit to remove an unedited managed file. A listing with no
matches still needs your confirmation. Removing the source from settings leaves loose files in place. There
is no watcher on the Agents or Instructions folders. Local status refreshes at startup, **Sync now**, or an
existing local look triggered by a settings/state or Skills folder notification. A scheduled check looks at these
files when it gets a fresh listing; opening the window reloads settings but does not itself rescan the files.

You may turn on **Start with Windows** with `tss startup on`. When Windows can resolve the executable's final network
path, the startup entry uses its UNC path; the executable also opts into Windows' network image-loading behavior.
Windows still needs access to the NAS or a fully synced OneDrive folder at sign-in. Startup from a real NAS or OneDrive
share has not been verified, so try it on your setup before relying on it.

### Plugins for GitHub Copilot CLI

Install **GitHub Copilot CLI** before adding a Plugins source. Choose **Plugins** in **Add source** and use a named
branch, for example `https://github.com/OWNER/NAME/tree/main` or a folder on that branch. Use letters, digits,
`.`, `_`, `/`, `+` or `-` in the branch name. The marketplace manifest must be at the top of the branch. A folder
limits the matches to plugins under that folder. Entries hosted elsewhere are shown but not handled.

**Preview** reads GitHub and does not start Copilot CLI. It shows the marketplace, plugin names, versions,
publishers and any declared hooks, MCP servers or LSP servers. Publishers are declared by the plugin or its
marketplace, not verified by GitHub. **Add and check** approves exactly the names and publishers you reviewed on this
PC. Plugins can run programs: approve only publishers and branches you trust. Updates for an approved plugin with
the same publisher install automatically. A new plugin, a changed publisher, or a source copied in through shared
settings needs local approval.

**Needs your approval**, **Removed from GitHub**, rejected or blocked plugins, plugins waiting for the marketplace,
unfinished planned changes and failures need attention. **Turned off in Copilot** and **Removed in Copilot** are
informational: the app respects your choice. It does not turn a plugin back on or reinstall it on its own. After you
turn a plugin back on in Copilot, its waiting update can install at the next full check (**Sync now**, `tss check`,
or after a change on GitHub). Already installed, already tracked by another source and hosted elsewhere are also
informational.

| Choice | What it does |
| --- | --- |
| **Approve** | Approve the shown name and publisher, including future versions from this branch. |
| **Reinstall** | Bring back a tracked plugin that was removed in Copilot. |
| **Uninstall** | Confirm in the window to remove a tracked plugin from Copilot after it was removed from GitHub. |
| **Forget** | Stop tracking a plugin removed from GitHub without uninstalling it. |

A plugin removed from GitHub is never uninstalled automatically. Plain **Remove** forgets the source's records and
leaves its plugins installed in Copilot, like **Forget** for each one. To **remove and uninstall**, use
`tss remove ID --uninstall-plugins`. It removes the tracked plugins from Copilot and verifies each uninstall. If any
uninstall fails, the command exits nonzero and keeps the source and remaining records. The marketplace stays registered.

Copilot can change between the app's check and its command. If the result does not match what the app expected, it
reports a failure and checks again. Copilot CLI also fetches the branch itself: if the branch moves and the publisher
changes in the meantime, the newer plugin may be installed once before the next check sees the change and holds it for
approval. Bump the plugin version when publishing a change: Copilot may skip an update without a new version. This works
only with Copilot CLI; the app does not drive VS Code's plugin installer.
The other source types do not need Copilot CLI.

### Start with Windows

With **Start with Windows** on (in Settings, or `tss startup on`), Windows starts the app when you sign in. Windows
starts such apps only once the desktop is up. The app then goes straight to the tray without opening its window and
reads settings on the worker. It waits 30 seconds before looking over the installed skills and checking GitHub, so
Windows can finish starting and the network can come up (the tooltip says "waiting for Windows to finish starting").
Then it looks over the skills and checks GitHub, at low priority, so it does not slow your sign-in down. If you open
the window before then, it looks over the skills at once; it checks GitHub before the 30 seconds are up only when you
ask it to (**Sync now**).

- The app offers this once per machine when you add your first repository, or open a machine with already-shared
  repositories (**Turn on** or **Not now**). It never turns it on by itself, and never turns it back on after you
  turned it off.
- It is the entry that Task Manager (**Startup apps**) and Windows Settings (**Apps** > **Startup**) list as Tiny Skill
  Sync. Turned off there, it stays off, and Settings in the app says so; turning it on in the app turns it on there too.
- The entry names the exe by its path. If you move the app to another folder, Settings says that Windows starts
  another copy, until you turn it on in the new folder. Before you delete the app, turn it off (or run
  `tss startup off`), so that no entry is left behind.
- Only readable settings can make it open its window at sign-in: when the local config or a portable companion cannot
  be read, nothing is checked until it is fixed, and the window says why.

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

The icon is the app's logo; its colour and fill say how things are.

| Icon | Meaning |
| --- | --- |
| Solid green | All skills are up to date. |
| Blue outline | Checking for updates, or, just after sign-in, waiting for Windows to finish starting. |
| Grey outline | No repositories are set up. |
| Amber | A skill needs you (see below). |
| Red | A check failed. The tooltip and the window say why. |

The tooltip says what the state is. Click the icon to open the window, or right-click for **Open Tiny Skill Sync**,
**Sync now** and **Quit**. Starting `tiny-skill-sync.exe` again opens the window of the copy that already runs;
`tiny-skill-sync.exe --quit` closes it. `tiny-skill-sync.exe --background` starts it the way Windows does at sign-in
(see [Start with Windows](#start-with-windows)); when a copy already runs, it does nothing. After installing an app
update, the tray starts its new version with `--restarted` (or `--restarted-open` when its window was open).

## The window

The band across the top says how everything is in one sentence ("All skills are up to date", "1 skill needs your
attention", "1 repository can't be read"). The line under it counts the skills and repositories and says when they were
checked and when the next check is; after something you asked for, it says what came of it ("Checked: skills were
updated", "Checked: nothing new on GitHub"). **Sync now** gets new and updated skills from every repository at once.
Skills you edited or deleted are never changed by it. Hover over it to read the same.

Down the left are your sources, each with its type and state, and **Add source**, **Activity** and **Settings**. When a
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
- **Its skills,** each on one line with its state. Rest the mouse on the name or status badge to see the explanation
  and any known version, file count, size and update time. Only a skill with a problem you can act on has an arrow: its choices start open.
  Click the row or press Enter on it to hide or show those choices.
  - For a skill the app installed, one line from its record: the version (the start of the commit it came from), how
    many files GitHub put there and their size, and when the app installed or last updated it ("Version 1a2b3c4 ·
    3 files, 12 KB · Updated 2 days ago"). Skills installed by an older version of the app say when only after their
    next update.
  - **Preview** shows the files GitHub put in the skill, each whole, as they are on this PC, in the same view as
    **Show changes** but with nothing to choose. Files you added are not shown, and nothing is written.
  - **Copy path** copies the path of the skill's folder. **Copy GitHub link** copies the address of the skill on
    GitHub, and the GitHub logo (**Open on GitHub**) opens it in your browser; the address names the repository's branch, or its default
    branch when none is set. The line under the band says what was copied.
  - **Open folder** opens the skill's folder in File Explorer.

  Each button shows only when it can work: no **Preview** for a skill the app did not install, no **Preview**, **Copy
  path** or **Open folder** for a skill whose folder is not on this PC, and no GitHub link for a skill GitHub no longer
  lists.
- **Rest the mouse on a skill or focus it with Tab** to see actions beside its name, starting with the folder icon
  when the folder is available. Rest on an icon to read what it does. In a narrow window, **More actions** gives
  access to commands that don't fit. Press the Menu key or Shift+F10 on a focused skill for a named list of actions.
  A skill **Kept as yours** stays closed; choose **Manage again** beside its name to resume syncing.
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
  - If local or upstream evidence changed since **Show changes**, the app does not replace the copy. Open
    **Show changes** again and confirm only after reviewing the new comparison. If a local file cannot be verified,
    the app leaves it alone.
  - Only the files whose hash differs are read or downloaded, and nothing is written. A file over 1 MiB, files beyond
    4 MiB from either side, and links (never followed) are named but not shown.
- **Activity:** what the app did recently, newest first, and what went wrong.
- **Settings:**
  - **Check for skill updates every**: the default interval, for every repository set to **Default** (1 day at first).
  - **App updates**: manual check and install, plus when the tray checks for and installs new versions of the two programs.
  - **Start with Windows**: a switch, and a line under it that says how it is, also when it was turned off in Task
    Manager or when Windows starts another copy of the app (see [Start with Windows](#start-with-windows)).
  - **Skills folder** and **Backups**, each with **Open**.
  - **GitHub sign-in for private repositories**: **Save a token…**, and once one is saved, **Replace token…** and
    **Delete token**.
  - **About**.

Every change is saved at once; there is no OK button. **Skill** intervals are a number and a unit: `30m`, `6h`, `1d`, `2w` or
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
| Cancel search | Pause this skill's earlier-version search until you choose Resume search. |
| Resume search | Continue the saved search, rechecking local evidence first. |

Skill backups are folders named `backup-<skill>-<number>` in the skills work folder (**Backups** in Settings
opens it). Loose-file backups are named `backup-leaf-*.old` in `.tss-leaf-work` beside that type's destination.
Activity and `tss` report the path when an operation finishes with a retained backup. The app never sweeps retained
backups; delete them yourself when you no longer need them.

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
tss update status                            app versions and preferences (not a skill check)
tss check [REPO] [--dry-run]                 check GitHub now and apply what is safe
tss add OWNER/NAME|URL [--type skills|agents|instructions|plugins] [--preview] [--every 6h] [--use-token]
tss remove REPO [--delete-skills|--uninstall-plugins]
tss resolve REPO ITEM replace|restore|keep|leave|manage|pause|resume
tss resolve REPO PLUGIN approve [PUBLISHER]
tss resolve REPO PLUGIN reinstall|uninstall|forget
tss token [status|set|clear]                 gh auth token | tss token set
tss startup [status|on|off]                  start the tray app when you sign in
tss portable                                 export local settings to companion files
```

Use the source ID that `tss status` shows for `REPO` when one repository has several sources. `tss check` (also
`tss sync`) checks sources, not the app. Apply what is safe: install, update, remove.

Private repositories use the saved token or the GitHub CLI sign-in when needed. `--use-token` sends the token for
every request. `tss add ... --type agents --preview` checks the source without saving. Omit `--preview` for the
confirmation prompt; confirming saves settings, then `tss check ID` installs. A bare `tss add` still saves a Skills
source without a prompt or a download. `--delete-skills` and earlier-version searches are only for Skills sources.

For Plugins, `tss add --type plugins URL --preview` previews without starting Copilot CLI. Omit `--preview` and
answer `yes` to save the source and record local approval. Then run `tss check ID` (also `tss sync ID`) to install.
`tss status` shows the last recorded versions without starting Copilot. `tss check` shows current plugin status
and the choices that apply. For `approve`, give the publisher as one argument exactly as shown, for example
`tss resolve ID example approve "Example Team"`. Omit it only when the publisher is not declared.

Exit codes: 0 all fine, 1 something needs attention,
2 failed, 3 bad command line. The tray notices changes made with `tss` and shows them. Only one of them checks at a
time; the other one is told to try again in a moment.

`tss startup` works on the tray app in the same folder as `tss.exe` (see [Start with Windows](#start-with-windows)).
It changes only Windows' startup entry, and says how it is in two lines: `on`, `off` or `not possible`, then why.

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
| Local settings | `%LOCALAPPDATA%\tiny-skill-sync\config.json` |
| Local app-release schedule (not shared preferences) | `%LOCALAPPDATA%\tiny-skill-sync\update-clock.json` |
| Shared settings and repositories (portable mode) | `tiny-skill-sync.settings.json` and `tiny-skill-sync.sources.json` beside the exes |
| What the app installed (the ownership record) | `%LOCALAPPDATA%\tiny-skill-sync\state.json` |
| The window's size | `%LOCALAPPDATA%\tiny-skill-sync\window.txt` |
| Skills | `%USERPROFILE%\.copilot\skills`, unless `skills_dir` is set in `config.json` |
| Agents and modular instructions | `%USERPROFILE%\.copilot\agents` and `%USERPROFILE%\.copilot\instructions` |
| Work folders: downloads in progress and backups | `.tss-work` beside Skills; `.tss-leaf-work` beside the loose-file destinations |

Use private, user-owned destinations and work folders; the app does not change Windows permissions.
If a loose-file change is interrupted, keep its state and work files together for recovery. Do not reuse a foreign
or unmarked `.tss-leaf-work` folder for new work. Recovery waits until the exact source configuration that started
the change is active again: the same source ID, type, repository, branch and path. Removing a source with interrupted
work is refused; if settings remove or rebind it, its files, state and work remain untouched until the original
configuration is restored. Recovery uses the destination recorded for the change, not a destination selected from
current settings. If an earlier build left one, let that copy's recorded recovery finish first and keep any remaining
work or backups somewhere private before the next change.

The skills folder contains only skill folders; the app keeps its own files elsewhere so Copilot never sees them. A
damaged `state.json` is kept as `state.json.damaged-*` and the app starts over with an empty record (it then takes over
skills that match a whole current or historical upstream version and asks about the rest).

Pointing the app at another skills folder (`skills_dir`, or `TSS_SKILLS_DIR`) moves nothing. The new folder gets its own
copies at the next check, which the tray runs at once when the settings change; the skills in the old folder stay as
they are, are no longer kept up to date, and the app says so once. The record of the old folder is kept (for the last
four folders used), so going back to it carries on where it left off: edits made there in the meantime are still found
and kept, and a change that was interrupted there is finished then, not before. Until the app has taken in the new
folder (the tray does at once; with `tss` alone, run `tss check`), a choice about a skill or the removal of a repository
is refused with a request to check first.

The optional `app_update` settings object contains `enabled`, `interval` (`daily`, `weekly`, `monthly` meaning
30 days) and `timing` (`immediate`, `on_quit`). Upgrade older copies before sharing nondefault preferences:
an old portable reader can reject the field, and an old local writer can omit it when saving.

In local mode, `config.json` can be edited by hand while the app runs; it is picked up at once. An example:

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
[Private repositories](#private-repositories)). A missing `kind` means Skills; use `"agents"` or `"instructions"`
for loose-file sources, or `"plugins"` for a Plugins source, in format 2. An unknown or null kind is an error. Never change an `id`: the installed
items are recorded under it. Unknown local settings fields are not preserved when the settings are saved.

In local `config.json`, `skills_dir`, when set, must be a full path such as `D:\skills`. In portable settings it may
also contain `%VARIABLE%` references, which are expanded on each machine; unknown variables and relative results are
errors. The default remains the per-machine `%USERPROFILE%\.copilot\skills`.

Environment variables, mainly for tests and scripts:

| Variable | Meaning |
| --- | --- |
| `TSS_HOME` | Use this folder instead of `%LOCALAPPDATA%\tiny-skill-sync`. One copy of the tray runs per folder. Such a copy keeps its Start with Windows entry under `HKCU\Software\tiny-skill-sync-test`, which Windows does not read. |
| `TSS_SKILLS_DIR` | Use this skills folder, whatever the settings say. |
| `TSS_AGENTS_DIR`, `TSS_INSTRUCTIONS_DIR` | Explicit disposable destinations for isolated copies syncing either loose-file type. Set them outside the active profile's `.copilot` folder. |
| `TSS_BENCH_REPORT` | Write timing marks to this file (used by `scripts\bench.ps1`). |
| `TSS_NO_SAVED_TOKEN` | Skip reading the saved app token (isolated live tests); GitHub CLI sign-in still works. |
| `TSS_COPILOT_CLI` | Optional full path to `copilot.exe`. Otherwise the app checks `%LOCALAPPDATA%\GitHubCopilotCLI\copilot.exe` and then absolute folders on `PATH`. |
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

### An edit that GitHub has too

An edit means "different from what the app installed". **Show changes** answers another question: "different from
what GitHub has now". They can disagree. If you edit a skill here and later put the same edit on GitHub, your folder
is exactly what GitHub has, yet it still differs from what the app installed, so it would look edited for ever while
**Show changes** says the two are identical.

The app handles this at the next check. For a skill you edited whose GitHub version changed, it compares your folder
with GitHub's version. When every file is the same (see [What counts as the same](#what-counts-as-the-same)) and no
file GitHub removed is still in your folder, it only updates its own record, and Activity says your edits are on
GitHub too and the skill is marked as in sync. Not one file in your folder is touched. Any other difference keeps
the skill held, as before.

To compare, the app may read from GitHub a text file that you and GitHub both changed. It reads one file at a time,
and not at all for a file GitHub did not change. A skill that really differs and stays held is compared again at
each check; that is at most one small download for each file GitHub changed. If a read fails, the check of that
repository ends and nothing is decided. If a check reaches its download limit, the skill is simply left held.

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

With Plugins sources added, the tray exe measured **1,434,112 bytes** (1.37 MiB), within its 1.40 MiB budget. On the
three-skill, five-file public fixture, the tray appeared in 8.5 ms median, Settings painted in 37.4 ms median, an
unchanged poll used 15.6 ms of CPU (one tick) with no file read or written, and ordinary Quit took at most 1.1 ms. Ten
idle seconds showed zero CPU, I/O and page faults, with a trimmed working set of 1.0 MiB. The
benchmark suppressed release checks; it does not measure retaining an offered update or installing on a slow share.

Measured with `scripts\bench.ps1` on a Ryzen 7 5800X3D with Windows 11, files in the cache, real installs of
[anthropics/skills](https://github.com/anthropics/skills) (19 skills, 442 files) and
[github/awesome-copilot](https://github.com/github/awesome-copilot) (463 skills, 1,555 files). The 442-file run includes
historical adoption; the larger install is an earlier baseline.

| Measure | 442 files, before app updates | 1,555 files, earlier | Budget |
| --- | --- | --- | --- |
| Start to tray icon | 7.7 ms | 8.6 ms | 30 ms median |
| Start-up check of every installed file | 20.1 ms | 105 ms | 75 µs per file |
| Start at sign-in, its first 3 s: CPU, file I/O | 15.6 ms median, 46.9 ms max; 241 bytes read | under 16 ms, 251 bytes | 300 ms, 300 KB |
| Idle for 10 s: CPU, disk, page faults | 0, 0, 0 | 0, 0, 0 | 0 |
| Idle memory: trimmed working set / private | 1.0 / 1.9 MiB | 1.2 / 2.6 MiB | 2 / 4 MiB |
| Window: open to painted (median) | 37.0 ms | 43 ms | 100 ms |
| Window open: working set | 13.6 MiB | 14.4 MiB | 40 MiB |
| Window closed: working set (trimmed) | 1.5 MiB | 1.5 MiB | 2 MiB |
| Unchanged check: CPU, file reads and writes | 0 ms, 0, 0 | 16 ms, 0, 0 | 50 ms, 0 |
| Exe size | 1,057,280 bytes (1.01 MiB) | 0.93 MiB | 1.40 MiB |

Every budget row passed in those earlier runs. The 442-file startup check took 48.2 microseconds per file; an unchanged poll took 438 ms
wall time and changed no files. The eight-candidate historical quick slice took 11.4 seconds in the live fixture
check, including an initial install and an older-copy update; its time depends on the network.

Windows counts a process's CPU time in ticks of 15.6 ms, so an unchanged check shows as zero to three ticks (0 to
47 ms) from run to run. The exe is built for size: in the earlier comparison, building for speed made the 0.89 MiB exe 1.01 MiB,
and hashed files about 13% faster (384 instead of 339 MB/s). A list of 463 skills opens almost as fast
as one of 19, because only
the rows within about two screens of what is shown are made at first; the rest are made as you scroll (with a row made
for every skill, it took 120 ms). Widening the list of repositories under the mouse takes 18 ms with those 463 skills
on the page.

The start at sign-in (`--background`) is measured against what Task Manager calls a "low" startup impact: under 300 ms
of CPU and under 300 KB of disk I/O. It was measured later, when the second repository had 464 skills and 1,556 files.
For its first 30 seconds the app only shows its icon and reads settings on the worker; it does not look at skills or
GitHub. In the first 3 seconds, that read used 241 bytes in 2 operations, with no writes or file changes, and the only
job was loading settings. After the wait, the worker looks at the skills, then checks GitHub when a repository is due.
That look costs what the start-up check costs, and nearly
all of its reads are `state.json`, about 500 bytes for each installed file. A repository that is due is checked after
it, at the cost of an unchanged check. These are warm runs: at a real sign-in the files are not in the cache yet.
Task Manager rates startup apps from a trace Windows records at each start, and it is not known whether that trace
still runs 30 seconds in (see
[Limitations](#limitations)).

Left running for 75 minutes against a test repository with a 5-minute interval and seven upstream changes
(`scripts\soak.ps1`), the app kept 6 threads and about 340 handles, used 297 ms of CPU in all (0.006 % of one core) and
ended at 3.2 MiB of private memory. Each change reached the disk within one interval (271 to 306 s). A second run of
22 minutes gave the same picture: 94 ms of CPU, 3.1 MiB private, changes on disk after 120, 60 and 305 s.

## Limitations

- No installer or publisher signature. App installs replace both files on disk; only the tray restarts itself
  afterwards, not `tss update install` and not an install at Quit.
- Start with Windows names the exe by its path, and Windows starts no command over 260 characters, so the path can be
  at most 245 characters (the rest is the quotes and `--background`). In a deeper folder the app names itself by its
  short (8.3) path, such as `C:\Users\you\AppData\Local\TINY-S~1\TINY-S~1.EXE`, which works only on drives that keep
  short names (most `C:` drives do). When even that is too long, the switch is greyed out, and the line under it says
  to move the app to a shorter folder.
- Start with Windows is tested by starting the app with the command Windows reads at sign-in, the way Windows starts
  it. The tests cannot sign in to Windows themselves.
- Task Manager's rating of the start at sign-in has not been seen at a real sign-in. The app waits 30 seconds before it
  looks at the skills; if Task Manager still counts that look, more than about 600 installed files can make it say
  "medium" impact rather than "low" (see [Measured cost](#measured-cost)).
- It is not checked yet whether a running Copilot session notices new or changed skills without a restart.
- A repository that is renamed or moved on GitHub is reported as a failure; update its address yourself.
- A change of display scale while the app runs is followed by the tray icon (it is drawn again at the taskbar's new
  size) but has not been tried on a real scale change.
- Skill checks use the Windows proxy settings (not tested behind a proxy). The anonymous app-release check
  connects directly to GitHub; if the network requires a proxy, use the manual ZIP upgrade instead.
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
  installed or changed in the meantime. Completed or paused earlier-version searches are remembered and shown
  after restarting without repeating the history requests. Relevant local edits or a changed upstream listing
  invalidate completed comparisons; an active search continues in small pieces.
- GitHub's servers can answer for up to a minute with a version of a public repository that is one change old. When
  two of its answers disagree (a file the listing names is different or cannot be found), the check stops with
  "Changed while checking" and changes nothing, and **Show changes** and **Use the GitHub version** say "the
  repository changed on GitHub while (the file) was being read; try again". The next try works.
- One check downloads at most 512 MiB (the rest waits for the next check), and a file over 64 MiB or a skill with more
  than 10,000 files is refused. There is no limit on what all the installed skills add up to.
- The window opens at 980 by 680 at 100% scale (smaller if the screen has less room) and can be made as small as 640
  by 480. It has been tried only at 100% scale.
- The window's controls are drawn by the app. Each is a real window with its text, and Tab, Enter, Space and Esc work,
  but they do not tell a screen reader what kind of control they are. A skill's icons are drawn inside its row, not
  separate Tab stops. Use Shift+F10 on the row to choose an action by name.
- Changing the skills folder does not move installed skills (see [Files](#files)). Once more than four other folders
  have been used, the oldest record is forgotten; going back to that folder then works like a damaged `state.json`.
  Two names for one folder that only the disk could tell apart (a short 8.3 name, a mapped drive, a link) count as two
  folders.

## More

- [CONTRIBUTING.md](../CONTRIBUTING.md): building, testing and measuring.
- [ARCHITECTURE.md](../ARCHITECTURE.md): how it works inside, with diagrams.
- [AGENTS.md](../AGENTS.md): rules for AI agents and people changing the code.
- [spikes/README.md](../spikes/README.md): the feasibility experiments the design is based on.
