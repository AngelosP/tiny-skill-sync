# Tiny Skill Sync App: user guide

[Back to the short introduction](../README.md)

A tiny Windows tray app that keeps your user-level Copilot skills, agents, modular instructions and GitHub Copilot CLI
plugins in sync with GitHub.

You give it a GitHub location: a repository, a branch, or a folder in one. It finds the skills, agents, instruction
files and plugins there, installs them into your Copilot folders, checks GitHub on a schedule, and installs, updates
and removes them as they change upstream. It adopts an
existing copy only after proving it matches upstream history, and never overwrites or deletes an edited skill without
your choice. It removes from your PC only
what came from GitHub and was later removed there; files you add are yours, and the app does not even show them.

- **Small and quiet.** One exe of about 1.6 MiB, no runtime to install. It starts in about 9 ms, uses about 1 MiB of
  memory while idle, and does no work at all between checks: no CPU, no disk, no network.
- **Starts with Windows, without slowing it down.** Turn on **Start with Windows** (the app offers it when you add your
  first repository). At sign-in it goes straight to the tray, reads settings on the worker, then waits 30 seconds
  before it looks at your skills or checks GitHub, and then does both at low priority.
- **Cheap checks.** An unchanged source costs one small "has anything changed?" request to GitHub, which reads and
  writes no files, whatever types it holds. This stays true after a restart, even for a source with nothing installed
  yet: it waits its normal interval. The one exception is a source with something waiting for your choice (plugins to
  approve, or a held skill): it is checked once each time the app starts, so the window can show what needs you.
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
   they are gone. If a source suddenly lists none of a type it installed, nothing of that type is deleted until you
   confirm. Items bundled in a plugin or skipped by a folder rule don't count as listed.
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
3. Click the icon to open the window. Click **Add source**, paste `owner/name`, a repository address, or the address
   of a branch or folder (`.../tree/BRANCH/FOLDER`), and click **Preview**. There is no type to choose. The same
   source page used for saved sources opens with the GitHub preview. For example:

   ```text
   https://github.com/anthropics/skills/tree/main/skills
   ```

4. Review the source page. Its type sections show detected counts, including zero and types set to **Sync off**;
   plugin rows include their declared details. Preview rows have no local status or repair actions, and links use the
   discovered GitHub paths. The **Check every**, **Keep in sync**, **What to sync** and folder-rule settings remain
   editable. **Keep in sync** starts off while you review. **What to sync** is open by default; content sections
   start collapsed. **What to sync** can also collapse; its type icons are solid when on and dimmed when off, with
   adjacent text spelling out each type's state. A collapsed type with something that needs you shows a warning after
   its name. Type-section text says the detected count, whether syncing is off, and whether
   the section is open. Changing type choices or **Check every** reuses the preview; changing repository-specific
   inclusion or folder rules refreshes it.
   **Cancel** saves nothing and installs nothing. **Add and keep in sync** saves the source and starts its first check. When
   approvable plugins are present, **Add and approve plugins** is a separate explicit choice; using the regular Add
   action leaves them waiting for approval. A failed preview offers **Retry** or **Edit location**; a preview with no items to sync
   offers **Edit location** before it can be added. If a source attempt is rejected (for example, because it is already
   set up), the next **Add source** attempt keeps the address you entered. Esc does not cancel a save already in
   progress. The app also asks whether to start with Windows.

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

The updater uses only the official release feed. It asks GitHub for release details without a token first. If GitHub
limits that request, the updater can retry it once with your token: the one saved in Windows Credential Manager or,
if none is saved, the GitHub CLI's (`gh auth token`). The token is only used for release details; downloads are
always anonymous.
If the retry is unavailable or fails, the updater reports the original rate limit and installs nothing.

Both standalone files must have the expected sizes and SHA-256 digests from GitHub before either is replaced.
The installed pair must also match both digests in its own official release: a custom build, edited file, or tag
without standalone digests is left alone. These
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
- `tiny-skill-sync.sources.json` holds the sources, including their IDs, branch, folder, type switches, folder rules,
  lane IDs, enabled state, interval and `use_token` choice, and the global type switches and folder rules.

The files are deliberately separate, so you can copy or update one without replacing the other. The command does not
change the local `config.json`. To change shared settings later, edit the companion JSON files or copy in reviewed
versions; `tss portable` is an export, not a merge command. Each file has a format version, and an unreadable,
unsupported or malformed companion is reported rather than silently ignored.

The settings and sources are shared; `state.json`, `engine.lock`, window size, the one-time startup-offer marker,
mutex, saved GitHub token, destination bindings and work folders remain local to each machine. The default skills folder is resolved
separately on each machine as `%USERPROFILE%\.copilot\skills`. A portable `skills_dir` may use Windows environment
variables such as `%USERPROFILE%`; they are expanded locally and the stored value is not rewritten. `TSS_SKILLS_DIR`
still overrides it on that machine. Agents and Instructions use their own local Copilot folders, not paths derived
from `skills_dir`. Do not share destinations or work folders between machines. The settings file is format 1. The
sources file, like the local `config.json`, is format 3; older sources files (format 1 or 2) are read and moved to
format 3 the next time the app saves them (see [Files](#files)). Update every copy that reads the share first: an
older copy refuses a format-3 file. Do not relabel files to use an older copy. Plugin approvals, destination
bindings, `seen.json` and the window's size and list width stay local.

The app reads and writes companion files on its worker, so a slow or unavailable share does not block the window.
Changes made on another machine are picked up at the next poll, **Sync now** or check, or when the window opens.
Writers on different computers take turns through a small lock file beside the exes (`.tss-settings.lock`). A check
waits up to 10 seconds for another computer's brief hold on it before it fails; a change you make in the window or
with `tss` fails at once with "try again" if the lock is busy. There is no transaction across the two files, and a
lock in a folder that OneDrive copies to each PC separately is not shared: avoid editing the files from multiple
machines at once, and check for OneDrive conflict copies before continuing. If the share is unavailable, the check
fails and installed skills are left alone.

### Agents and modular instructions

A source finds `*.agent.md` files (agents) and `*.instructions.md` files (instructions) anywhere below its folder, not
only directly in it; see [What a source finds](#what-a-source-finds). Each file is installed by its own name, flat.
`AGENTS.md`, `copilot-instructions.md` and Cursor rules are never imported.

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

Install **GitHub Copilot CLI** before syncing plugins. A source turns up plugins by itself when its location holds
plugin manifests; Copilot CLI installs them from a marketplace (`marketplace.json`) at the top of the branch. A
folder location limits the plugins to those under that folder, but the marketplace is still read from the top of the
branch. Copilot CLI can install a plugin from a branch only through a marketplace that lists it, so a plugin the
marketplace does not list is shown with that reason but never installed (this was measured; see spike S4 in
[spikes/README.md](../spikes/README.md)). Entries hosted elsewhere are shown but not handled. For plugins the branch
name may use only letters, digits, `.`, `_`, `/`, `+` or `-`.

**Preview** reads GitHub and never starts Copilot CLI. It shows each plugin's name, version, publisher and what it
can add to Copilot (hooks, MCP servers, LSP servers, agents, skills, commands). That list includes what the plugin
declares and the files and folders Copilot picks up by name even when undeclared, such as `.mcp.json`,
`hooks/hooks.json`, `lsp.json` or an `agents`, `skills` or `commands` folder. Publishers are declared by the plugin or
its marketplace, not verified by GitHub. Then choose:

- **Add and keep in sync**: saves the source and starts its first check. Plugin installation still waits for your
  separate approval; the plugin rows show **Needs your approval** until you approve them.
- **Add and approve plugins**: explicitly approves exactly the plugin names and publishers you reviewed, on this PC,
  as well as adding the source.

`tss add LOCATION --approve-plugins` is the same as the second choice; without it, `tss add` leaves them waiting.
Approve one later with **Approve** in the window or `tss resolve ID PLUGIN approve PUBLISHER`, giving the publisher
exactly as shown. Plugins can run programs: approve only publishers and branches you trust. Updates for an approved
plugin with the same publisher install automatically. A new plugin, a changed publisher, a source copied in through
shared settings, and a source whose default branch has become another branch all need approval again on this PC.
A missing Copilot CLI or a plugin you have not approved never stops the source's files from syncing.

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
Skills, agents and instructions do not need Copilot CLI.

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

## What a source finds

### A source is a location

A source is one place on GitHub. You can give it as:

- `owner/name` or `https://github.com/owner/name`: the whole repository on its **default branch**. If the repository
  makes another branch its default, the source follows it from the next full check.
- `https://github.com/owner/name/tree/BRANCH`: the whole repository on that named branch. `tree/main` is a different
  source from the bare repository, even while `main` is the default branch.
- `https://github.com/owner/name/tree/BRANCH/FOLDER`: only that folder and what is below it.

There is no type to choose. Branch names with slashes work: the app asks GitHub for the branch names and takes the
longest one that matches the start of the address. Only HTTPS addresses on github.com are taken. Addresses of a file,
tag, commit, pull request or raw file are refused, and so is a folder that is not on the branch.

### What is recognised

| Type | What counts | Installed as |
| --- | --- | --- |
| **Skills** | A folder with a `SKILL.md`, with everything under it | A folder named after it (the repository's name when the location itself is the skill), in your skills folder |
| **Agents** | A `*.agent.md` file | That file, by its own name, in your agents folder |
| **Instructions** | A `*.instructions.md` file | That file, by its own name, in your instructions folder |
| **Plugins** | A plugin manifest (`plugin.json`, `.plugin/plugin.json`, `.github/plugin/plugin.json` or `.claude-plugin/plugin.json`), with a marketplace at the top of the branch | Installed by GitHub Copilot CLI after you approve it (see [Plugins](#plugins-for-github-copilot-cli)) |

They are found at any depth below the location. **A package owns everything below it**: once a folder is a plugin or
a skill, the skills, agents and instructions inside it belong to that package and are never listed or installed on
their own. (A folder that is both a plugin and a skill counts as a plugin.) To sync the parts of a plugin separately,
add a source for a folder inside it.

When two items of the same type would get the same name on Windows (names that differ only in case, for example),
neither is synced, the preview says "Same install name on Windows, so none of these sync", and a copy already
installed is kept.

Some files look similar but are never synced, and the preview and the window list them under **Never synced** with
the reason:

| File | Why |
| --- | --- |
| `AGENTS.md` | Repository guidance for coding agents, not a custom agent |
| `copilot-instructions.md` | Repository-wide instructions apply only inside that repository |
| `.cursorrules`, `.cursor/rules/*.mdc` | Cursor rules are not Copilot instructions |

### Turning types on and off

**Settings** has a switch per type: **Sync skills by default**, **Sync agents by default**, **Sync instructions by
default** and **Sync plugins by default**. All four are on at first. Each source can follow that default, or turn a type
on or off for itself, in its **What to sync** card (or with `tss source ID types` and `tss settings types`).

Turning a type off deletes nothing. What it installed stays as it is and is shown as **Sync off**: it is not updated
and not removed. A source whose items are all of types that are off is shown as **Sync off** too, not as having found
nothing. When you turn the type on again in the window, that type is checked right away, and the check compares those
copies in full, so an edit you made meanwhile still holds.

### Folder rules

Two rules leave folders out. Both look only at folders **below** the location you gave, so pasting the address of such
a folder still finds what is in it.

- **Repository-specific folders**: `.github`, `.agents`, `.claude`, `.copilot`, `.codex`, `.cursor` and `.gemini`
  usually hold skills and agents meant for that repository's own tools. Skills and agents in them are skipped;
  instructions and plugins in them are not. Turn on **Include repo-specific skills and agents** in Settings, or
  **Repository-specific skills and agents** for one source (`tss settings repo-items on`,
  `tss source ID repo-items on`), to sync them.
- **Ignored folders**: a folder with one of these names is skipped for every type, with everything in it. The list
  starts as `sample`, `example`, `samples`, `examples`, `test` and `tests`; a name must match a whole folder name, in
  any case. Change it with **Ignored folders** > **Edit...** in Settings (`tss settings ignore add|remove NAME`); leave
  the answer empty to ignore no folders. A source can skip more names, or allow a name the list skips, with **Folder
  rules...** in its card (`tss source ID ignore add|allow|remove NAME`); leave both lists empty to remove its rules.

The preview and the window count what the rules left out ("Skipped 2 by a folder rule: 'tests' is an ignored
folder."). Like a type that is off, a rule that newly leaves out something already installed keeps it as it is,
without updates. Rule changes apply at the next check.

### What stops a skill

A skill whose files include something unsafe on Windows (a link, a submodule, a reserved name such as `CON`, two names
that differ only in case, an over-long path) is not installed, and a version already installed is left as it is. The
reason is shown in the window and by `tss check` whenever a check reads the repository's listing: the first
check, each time the repository has changed, and `tss check --full-listing` (see [Limitations](#limitations)).

When a folder location holds no skills, the source shows "No skills found" and says which folder it read.

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

A band runs across the top of the window, under the title bar: a sky in your Windows accent colour with layered hills on its right, in deeper shades of the same colour, and a thin line along its bottom. In light mode the sky is a rich mid-tone rather than a pale wash, and the main button on it (**Sync now**) is a deep colour that stands out from it. The round mark at the left of the heading is a calm green check when everything is in sync. It follows Windows' light or dark mode and your accent colour, and changes with them while the window is open; the app's buttons, switches and links take the accent colour too. Every shade is chosen so that the words on the band stay easy to read and each hill stands apart from the one behind it, whatever the accent. When the heading and the buttons leave room, the icons of the four types (skills, agents, instructions and plugins) sit in small circles of sky along the middle hill; they are decoration and do nothing when clicked. On a narrow window a long line under the heading can need their room, and then all four step aside. In high contrast the band takes the window colour and a text-coloured line, without the hills or the icons. The same band heads **Show changes** and
**Preview**.

The band says how everything is in one sentence ("All skills are up to date", "1 skill needs your
attention", "1 source can't be read"); when your sources hold more than one type, it says "items" ("All items are up
to date"). It counts sources, so one source that syncs several types counts once. When every type found is switched
off, it says "Nothing is being synced" ("Sync is off for every type found") rather than calling anything up to date. The line under it counts the items
and sources and says when they were checked and when the next check is; after something you asked for, it says what
came of it ("Checked: skills were updated", "Checked: nothing new on GitHub"), naming only the types it is about: a
source that also syncs agents still says "skills were updated" when only its skills changed. **Sync now** checks every
enabled source at once. Items you edited or deleted are never replaced without asking. Hover over it to read the same.

Down the left are your sources, and **Add source**, **Activity** and **Settings**. Each source takes two lines: the repository name with its owner dimmed in front of it, beside its status mark, then a small chip for each type (icon and count, or muted when it is off). A chip is amber with a warning sign when that type needs you. A status word appears only when something is not simply up to date; a source that is up to date shows just a calm green check. The branch and folder stay in the source's details and hover text. When the list is too narrow for every type, a "+2" chip, for example, means two more types; it is amber when one of those needs you. A screen reader hears every type spelled out, for example "Skills 3, Instructions: Sync off", and resting the mouse on a source shows the full repository, branch, folder, type summary and status, even when the list cuts text short. Drag the splitter
between the list and the page to see more of a long name. Or Tab to **Resize list** and use Left or Right to move it
8 pixels at 100% display scale (hold Shift for 32); Home and End go to the narrowest and widest list. The list keeps at least 160 pixels and
the page at least 360 when the window is wide enough. The window remembers the width you choose on this PC (the third
number in `window.txt`). Nothing widens or narrows when you move the mouse over it.

- **A source:** its location (with a **Private** label when it is read with your token), the folder and branch, and
  when it was last checked.
  - When it can't be read, a box says why, with **Try again** and **Open Settings** (where the GitHub sign-in is).
  - **Check every** sets how often it is checked: **Default** (the interval in Settings), a common interval, or
    **Custom…**.
  - **Keep in sync** off pauses it: it is not checked, and nothing it installed is changed.
  - On a saved source, **What to sync** starts collapsed and the content sections (including **Never synced**, when
    present) start expanded. The app remembers sections you open or close for that source until you quit.
  - **What to sync** has a choice per type, **Default (On)** or **Default (Off)** to follow Settings, or **On** or
    **Off** for this source only; **Repository-specific skills and agents** (**Default**, **Include** or **Skip**); and
    **Folder rules…**, which asks for folder names this source should also skip and names it should never skip. See
    [What a source finds](#what-a-source-finds).
  - The **More** button holds **Check now** (all of the source's types at once), **Open on GitHub**, **Open skills
    folder** (when the source syncs skills) and **Remove source…**. Removing stops the whole source, every type.
    When it syncs skills, it asks whether to leave its files installed (they become yours) or to delete its skills too
    (skills you edited are kept, and files you added stay where they are); agents, instruction files and plugins stay
    installed either way.
- **What it holds,** grouped under **Skills**, **Agents**, **Instructions** and **Plugins**. Each type shows its
  detected count, including zero; a type that is off also says **Sync off**. Sections can be expanded or collapsed.
  Type-section text names the type, its detected count and expanded/collapsed state. **What to sync** also collapses;
  its type icons, solid when on and dimmed when off, are paired with adjacent text spelling out each type's state. A
  collapsed type with something that needs you shows a warning after its name. Under each open
  group are the items, folder-rule skips and other
  unsynced items with their reasons. **Never synced** lists files such as `AGENTS.md`.
- **Each item,** on one line with its state. An item that is in sync ("Up to date" or "Installed") shows the same calm green check as the source list instead of a word; every other state keeps its word. Rest the mouse on the name or status to see the explanation
  and any known version, file count, size and update time. Only an item with a problem you can act on has an arrow: its choices start open.
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
  - The same splitter resizes the file list for long paths. It uses the width you chose on either view. Your selected
    file and scroll positions stay put.
  - The chosen file's lines are on the right: `+` on green for a line only on GitHub, `−` on red for a line only on this
    PC, with three lines around each change. Long unchanged stretches are folded.
  - The skill's choices are at the top. In a narrow window, the line under the skill's name moves below them.
    **Back** (or Esc) returns.
  - If local or upstream evidence changed since **Show changes**, the app does not replace the copy. Open
    **Show changes** again and confirm only after reviewing the new comparison. If a local file cannot be verified,
    the app leaves it alone.
  - Only the files whose hash differs are read or downloaded, and nothing is written. A file over 1 MiB, files beyond
    4 MiB from either side, and links (never followed) are named but not shown.
- **Activity:** what the app did recently, newest first, and what went wrong.
- **Settings:**
  - **Check for updates every**: the default interval, for every source set to **Default** (1 day at first).
  - **Sources**: **Sync skills by default**, **Sync agents by default**, **Sync instructions by default** and **Sync
    plugins by default** (all on at first), **Include repo-specific skills and agents** (off at first), and
    **Ignored folders** with **Edit…**. A source can override each of them.
  - **App updates**: manual check and install, plus when the tray checks for and installs new versions of the two programs.
  - **Start with Windows**: a switch, and a line under it that says how it is, also when it was turned off in Task
    Manager or when Windows starts another copy of the app (see [Start with Windows](#start-with-windows)).
  - **Skills folder** and **Backups**, each with **Open**.
  - **GitHub sign-in for private repositories**: **Save a token…**, and once one is saved, **Replace token…** and
    **Delete token**.
  - **About**.

Section open/closed choices are remembered separately for each source until the app exits; they are not saved in
settings. A preview keeps its choices when it is added as a source.

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
| Delete them | Upstream now lists none of this type at all: remove the ones the app installed. |
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

The app's update check uses the same token only to retry release details once after GitHub limits the request
without a token; downloads never carry it. Any token works for this.

## The command line

`tss` does the same work as the tray, one command at a time, with the same safety rules. Run `tss help` for the details.

```text
tss status                                   what is installed and what needs attention (no network)
tss update status                            app versions and preferences (not a skill check)
tss check [REPO] [--dry-run] [--full-listing] [--delete-all]   check GitHub now and apply what is safe
tss add LOCATION [--preview] [--approve-plugins] [--every 6h] [--use-token] [--yes]
tss source REPO                              one source's types and folder rules
tss source REPO types skills=on|off|default [agents=...] [instructions=...] [plugins=...]
tss source REPO ignore add|allow|remove FOLDER
tss source REPO repo-items on|off|default
tss settings                                 the defaults every source follows
tss settings types skills=on|off [agents=on|off] [...]
tss settings ignore add|remove FOLDER
tss settings repo-items on|off
tss remove REPO [--delete-skills] [--uninstall-plugins]
tss resolve REPO ITEM replace|restore|keep|leave|manage|pause|resume
tss resolve REPO PLUGIN approve [PUBLISHER]
tss resolve REPO PLUGIN reinstall|uninstall|forget
tss token [status|set|clear]                 gh auth token | tss token set
tss startup [status|on|off]                  start the tray app when you sign in
tss portable                                 export local settings to companion files
```

`REPO` is a source ID that `tss status` shows, or one of its type IDs, or `OWNER/NAME` when only one source has that
repository. `tss check` (also `tss sync`) checks sources, not the app. Apply what is safe: install, update, remove.
`tss check ID` checks every type of that source at once.

When a type that has items installed suddenly lists none, its removals wait for you. `tss check TYPE-ID --delete-all`
confirms them for that one type only, using the type ID that `tss status` shows; the source ID (or `owner/repo`)
confirms only its skills, and confirms nothing for a source that does not sync skills.
`--full-listing` reads the whole listing again even when GitHub says nothing changed.

`tss add` takes the same locations as the window (see [A source is a location](#a-source-is-a-location)); there is no
`--type`. It shows the same preview, then asks you to type `yes`. When its input is not a terminal it refuses unless
you pass `--yes`. `--preview` shows what was found and saves nothing. Before saving, it reads the location again and
refuses if anything changed since the preview. Then `tss check ID` installs. Private repositories use the saved token
or the GitHub CLI sign-in when needed. `--use-token` sends the token for every request.

`tss source ID` shows one source: each type's switch ("Skills: Sync on (the default) [ID]"), whether
repository-specific skills and agents are skipped, and its ignored folders. `default` follows `tss settings`.
`ignore allow NAME` keeps a globally ignored name from being skipped in this source only; `remove` takes a name off
the source's own lists. Turning a type off deletes nothing. Changes apply on the next check.

`tss remove ID` stops following the whole source, every type. Its files and plugins stay installed. `--delete-skills`
also deletes the skills it installed (edited skills stay); agents and instruction files stay either way.
Earlier-version searches are only for skills.

For plugins, `tss add ... --preview` never starts Copilot CLI. Without `--approve-plugins`, the plugins wait for your
approval; with it, saving also approves the plugins shown. Then run `tss check ID` (also `tss sync ID`) to install.
`--preview` and `--approve-plugins` together are refused.
`tss status` shows the last recorded versions without starting Copilot. `tss check` shows current plugin status
and the choices that apply. For `approve`, give the publisher as one argument exactly as shown, for example
`tss resolve ID example approve "Example Team"`. Omit it only when the publisher is not declared.

`tss status` lists each source once: its location, status and ID, then "Branch: ... Folder: ...", a "Types:" line,
the last check, and its items grouped by type, with how many were skipped by folder rules and what is not synced.

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
| The window's size and list width | `%LOCALAPPDATA%\tiny-skill-sync\window.txt` |
| What each source held at its last complete check, for display only | `%LOCALAPPDATA%\tiny-skill-sync\seen.json` |
| The lock that lets one writer change the settings at a time | `.tss-settings.lock` beside the exes in portable mode, else in `%LOCALAPPDATA%\tiny-skill-sync` |
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
skills that match a whole current or historical upstream version and asks about the rest). `state.json` is saved in
format 3 once it holds something older versions do not know: installed plugins, or check times for a type that has
nothing installed yet. v0.8.0 and older copies refuse a format-3 `config.json` or `state.json` without changing it, so
update every copy on a PC before using this version there.

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
  "version": 3,
  "default_interval": "1d",
  "global": {
    "types": { "plugins": false },
    "ignore_folders": ["sample", "example", "samples", "examples", "test", "tests", "drafts"]
  },
  "sources": [
    {
      "id": "anthropics-skills",
      "repo": "anthropics/skills",
      "branch": "main",
      "path": "skills",
      "lanes": { "skills": "anthropics-skills", "agents": "anthropics-skills-agents" }
    },
    {
      "id": "me-private-kit",
      "repo": "me/private-kit",
      "interval": "6h",
      "use_token": true,
      "types": { "instructions": false },
      "include_repo_items": true,
      "ignore_add": ["old"],
      "ignore_allow": ["examples"]
    }
  ]
}
```

A missing or empty `branch` means the default branch, and a missing or empty `path` the repository root.
`use_token` sends the token with every request to that repository; without it, a private repository is still read
with the token (see [Private repositories](#private-repositories)). `global` holds the defaults: `types` (all four
`true` unless set to `false`), `include_repo_items` (`false`) and `ignore_folders` (the list shown in Settings). A
source's `types` and `include_repo_items` override them (leave a type out to follow the default); `ignore_add` and
`ignore_allow` add names to skip or never skip. `lanes` holds the ID under which each type's items are recorded. The
app fills in a missing one from the source's `id` alone (`id`, `id-agents`, `id-instructions`, `id-plugins`), so every
PC that reads the same file gets the same IDs. When you turn a type on whose ID already holds another type's or
another location's records on this PC, the app gives that type a new ID and saves it. Never change an `id` or a lane
ID: the installed items are recorded under it. Unknown local settings fields are not preserved when the settings are
saved.

**Older settings are moved over by themselves.** A `config.json` or portable sources file in format 1 or 2 had one
entry per type (`kind`; a missing `kind` meant Skills). Each entry becomes a source with its own type on and the other
three off, keeping its ID as that type's lane ID, so nothing installed is lost or installed twice. Entries with the same
repository, branch, folder, `use_token`, interval and enabled state, but different types, become one source. The file
is written in format 3 the next time the app saves it. A file from a newer version is refused, not changed, and so is
a file without `"version"` that already uses `sources` or `global`.

**`seen.json`** remembers, per source, the branch and what its last complete listing held, so the window and
`tss status` can show a source's types and counts before the next check. It is written only after a complete listing
whose result changed, it stays on this PC, and nothing is installed, kept or removed because of it. The app reads it
again just before writing, and never overwrites one written by a newer version. If it is missing or damaged, the
details appear after the next check.

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

With mixed sources added (skills, agents, instructions and plugins found in one repository), the tray exe measured
**1,711,104 bytes** (1.63 MiB), within its 1.70 MiB budget. On the three-skill, five-file public fixture, the tray
appeared in 7.4 ms median, Settings painted in 36.0 ms median, an unchanged poll used 15.6 ms of CPU (one tick) with no
file read or written, and ordinary Quit took at most 1.3 ms. Ten idle seconds showed zero CPU, I/O and page faults, with
a trimmed working set of 1.0 MiB. The
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
| Exe size | 1,057,280 bytes (1.01 MiB) | 0.93 MiB | 1.70 MiB |

Every budget row passed in those earlier runs. The 442-file startup check took 48.2 microseconds per file; an unchanged poll took 438 ms
wall time and changed no files. The eight-candidate historical quick slice took 11.4 seconds in the live fixture
check, including an initial install and an older-copy update; its time depends on the network.

Windows counts a process's CPU time in ticks of 15.6 ms, so an unchanged check shows as zero to three ticks (0 to
47 ms) from run to run. The exe is built for size: in the earlier comparison, building for speed made the 0.89 MiB exe 1.01 MiB,
and hashed files about 13% faster (384 instead of 339 MB/s). A list of 463 skills opens almost as fast
as one of 19, because only
the rows within about two screens of what is shown are made at first; the rest are made as you scroll (with a row made
for every skill, it took 120 ms). Widening the list of repositories took 18 ms with those 463 skills on the page,
measured when the list still widened under the mouse; it now changes width only when you move the divider.

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
- When a source follows the default branch and the repository switches to another default branch, the app notices
  on the next full check (the first check after a change on GitHub, or `tss check --full-listing`). Its plugins then
  wait for a fresh approval.
- When a branch is renamed, plugins already installed from it keep the marketplace name they were installed under.
- For a source that follows the default branch, the window says "default branch" rather than the branch's name.
- If removing a whole source fails partway (one type could not be removed), the types it could not remove stay in the
  settings. Remove the source again.
- A source whose plugin check keeps failing is listed again on every poll, instead of waiting for its next change.
- The types of a source are applied one after another from one listing. If one fails to download, the types before it
  keep what they installed and the rest wait for the next check.
- A type turned off before anything of it was installed shows no rows in the window; its items appear once the type
  is on again and the source is checked.
- A change of display scale while the app runs is followed by the tray icon (it is drawn again at the taskbar's new
  size) but has not been tried on a real scale change.
- Skill checks use the Windows proxy settings (not tested behind a proxy). The app-release check, including its
  token retry and downloads, connects directly to GitHub; if the network requires a proxy, use the manual ZIP
  upgrade instead.
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
- One check downloads at most 512 MiB, plugin manifests included (the rest waits for the next check), and a file over
  64 MiB or a skill with more than 10,000 files is refused. There is no limit on what all the installed skills add up
  to.
- The window opens at 980 by 680 at 100% scale (smaller if the screen has less room) and can be made as small as 640
  by 480. It has been tried only at 100% scale.
- The window's controls are drawn by the app. Section text names the heading, count and expanded/collapsed state,
  and whether a collapsed type needs your attention;
  section controls can be opened with Enter or Space. Most custom-drawn controls do not tell a screen reader what
  kind of control they are. A skill's icons are drawn inside its row, not separate Tab stops. Use Shift+F10 on the
  row to choose an action by name.
- Changing the skills folder does not move installed skills (see [Files](#files)). Once more than four other folders
  have been used, the oldest record is forgotten; going back to that folder then works like a damaged `state.json`.
  Two names for one folder that only the disk could tell apart (a short 8.3 name, a mapped drive, a link) count as two
  folders.

## More

- [CONTRIBUTING.md](../CONTRIBUTING.md): building, testing and measuring.
- [ARCHITECTURE.md](../ARCHITECTURE.md): how it works inside, with diagrams.
- [AGENTS.md](../AGENTS.md): rules for AI agents and people changing the code.
- [spikes/README.md](../spikes/README.md): the feasibility experiments the design is based on.
