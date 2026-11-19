# Tiny Skill Sync App

**Keep your Copilot skills up to date, without copying files by hand.**

Add a GitHub repository, or its skills folder, and this small Windows tray app installs the skills for you.
It checks for updates on a schedule, keeps unchanged copies in sync, and stops when a skill has your edits.
You decide what happens next.

![Demo: add a repository, install skills, receive an update, inspect a held edit, and keep your version.](docs/demo.gif)

*Real app, throwaway demo data. The five-minute scheduled wait is shortened.
[Read the demo as text](#what-the-demo-shows).*

## Why use it?

- **Set it up once.** Follow several repositories without downloading each new version yourself.
- **Keep your edits.** Review differences in the window before choosing GitHub's version or keeping yours.
- **Keep your own files.** Files you add to a skill stay yours; the app does not list them as changes.
- **Stay out of the way.** It lives in the tray, can start with Windows, and does no work between checks.
  The tray exe is under 1 MiB; [measured idle memory is about 1 MiB](docs/GUIDE.md#measured-cost).

## Get started

Windows 10 or 11, x64. No runtime or installer is needed. The exes are not signed.

1. Download the Windows zip from [Releases](https://github.com/AngelosP/tiny-skill-sync/releases).
   Extract both exes into a folder you want to keep.
2. Start `tiny-skill-sync.exe` and click its tray icon.
3. Click **Add repository** and paste the address of the folder holding the skills. For example:

   ```text
   https://github.com/anthropics/skills/tree/main/skills
   ```

4. Click **Add**. Skills install at once; the app then checks once a day by default.
   Change **Check every** if you prefer, or use **Sync now**. Starting with Windows is optional.

Public repositories need no sign-in. For private ones, sign in with `gh auth login` or save a read-only token
in the app's Settings. See [private repositories](docs/GUIDE.md#private-repositories).

## How it works

A skill is a folder containing `SKILL.md`. Give the app that folder, or the folder directly containing several
skills. It copies them into `%USERPROFILE%\.copilot\skills`, where Copilot looks for user-level skills.

On each scheduled check, it installs new skills and updates copies you have not edited. When GitHub removes a skill,
only its proven upstream files are removed; your added files stay. A network failure never counts as a deletion.
If the whole listing becomes empty, the app asks first.

Edited skills are held for your choice. **Show changes** compares your copy with GitHub.
**Use the GitHub version** keeps the replaced copy as a backup; **Keep my version** stops managing that skill.
Existing copies are adopted only after a complete match with a current or past upstream version.
Downloads are copied, never run.

[Read the full safety promises](docs/GUIDE.md#safety-promises), including
[which text differences count as edits](docs/GUIDE.md#what-counts-as-the-same).

### What the demo shows

1. Add a public repository's skills folder.
2. Its skills appear in the app and are verified on disk in an isolated copy of the Copilot skills folder.
3. Change a file on GitHub and let the next scheduled check install it. The five-minute wait is cut from the GIF.
4. Edit a managed file locally. An upstream update is held, and **Show changes** explains the difference.
5. Choose **Keep my version**. That skill stays yours, the other skills stay managed, and the blue tray icon means
   all is well. The final icon illustration is enlarged, not a recording of the desktop.

## More

- [User guide](docs/GUIDE.md): the window, command line (`tss`), backups, settings, updating the app and limitations.
- [Measured cost](docs/GUIDE.md#measured-cost): timings, memory, disk use and test conditions.
- [Contributing](CONTRIBUTING.md): build, test and remake the demo.
- [Architecture](ARCHITECTURE.md): how the safety rules work inside.

The app does not update itself. To update, quit it from the tray, replace both exes, and start it again.
Whether an already-running Copilot session reloads changed skills without restarting has not been verified.
