# Contributing

How to build, test and measure tiny-skill-sync. Read [ARCHITECTURE.md](ARCHITECTURE.md) first for how it works, and
[AGENTS.md](AGENTS.md) for the rules every change follows.

## What you need

- Windows 10 or 11, x64.
- Rust, stable, MSVC toolchain (`rustup`, `stable-x86_64-pc-windows-msvc`), 1.85 or newer. The code is edition 2024.
- The MSVC linker: Visual Studio Build Tools with the "Desktop development with C++" workload.
- Optional: the GitHub CLI (`gh`), to try private repositories without saving a token.

The scripts run in Windows PowerShell 5.1 and in PowerShell 7. If your execution policy blocks them, run them as
`powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev.ps1 ...`. The scripts add `%USERPROFILE%\.cargo\bin`
to the `PATH` when `cargo` is not found.

## The scripts

Start with `scripts\dev.ps1`; it runs the others.

| Command | What it does |
| --- | --- |
| `scripts\dev.ps1` or `scripts\dev.ps1 run` | Build (debug) and start the tray app on the sandbox. Options: `-Release`, `-Fresh` (delete the sandbox first), `-Quit`. |
| `scripts\dev.ps1 build` | Build both exes (release) and print their sizes. `-DebugBuild` for a debug build. |
| `scripts\dev.ps1 test` | Formatting, clippy and every test. `-Quick` (tests only), `-Filter TEXT`, `-Root FOLDER` (see below). |
| `scripts\dev.ps1 tss ARGS` | Run `tss` on the sandbox, for example `scripts\dev.ps1 tss check --dry-run`. |
| `scripts\dev.ps1 dry-run -Repo OWNER/NAME` | Show what a check of that repository would install, in throwaway folders. `-UseToken` for private ones. |
| `scripts\dev.ps1 bench` | Measure the release app against its budgets (see [Measuring](#measuring)). |
| `scripts\dev.ps1 e2e` | Run the real exes against real GitHub test repositories and check what they do (see [Live end-to-end runs](#live-end-to-end-runs)). `-Only NAME,NAME`, `-Keep`, `-SkipBuild`. |
| `scripts\dev.ps1 soak` | Leave the release app running for 75 minutes against a repository that changes every 10 minutes, and sample its memory, handles, threads, CPU and file I/O. `-Minutes`, `-Every`, `-ChangeEveryMinutes`, `-Keep`. |
| `scripts\dev.ps1 demo` | Make `docs\demo.gif` from the real app and disposable public fixture data. `-Python PATH`, `-SkipBuild`. See [Remaking the README demo](#remaking-the-readme-demo). |
| `scripts\dev.ps1 clean` | Close the sandbox copy and any copy a live script left running, and delete the sandbox, test and live-script leftovers in `%TEMP%` and `bench\out`, and the test copies' Start with Windows keys (`HKCU\Software\tiny-skill-sync-test`). Keeps `target\`. |
| `scripts\dev.ps1 logo` | Write the files in `assets\` again from `src\logo.rs`: `logo.svg`, `logo-16.png` to `logo-256.png`, `favicon.ico` and `tiny-skill-sync.ico` (the exes' icon). Run it after changing the logo, look at the files, and commit them with the change; a test fails while they differ from what the code draws. |

The **sandbox** is `.tss-test\` in the repository (ignored by git): its own settings (`.tss-test\home`) and skills
folder (`.tss-test\skills`), set through `TSS_HOME` and `TSS_SKILLS_DIR`. A sandbox copy of the tray runs beside a
real one, with its own icon. Its Start with Windows entry is kept under `HKCU\Software\tiny-skill-sync-test`, which
Windows never reads, so turning it on there starts nothing at sign-in. The saved GitHub token is the one thing it
shares with the real app, so `scripts\dev.ps1 tss token set` and `token clear` are refused.

Two small public and private fixture repositories are used for live tries:
`AngelosP/tiny-skill-sync-fixtures-public` and `AngelosP/tiny-skill-sync-fixtures-private` (the private one needs a
token with access to it).

## Before a change is done

```powershell
scripts\dev.ps1 test
```

This runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --no-fail-fast`. All three
must pass. There are 580 tests (365 in the library, 215 integration tests); the whole gate takes about 40 seconds when
the code is already built.

If the change can affect speed, memory or disk use (startup, verification, the poll, the tray, the window),
also run `scripts\dev.ps1 bench` and check that every row is still within budget.

## Tests

**Every test works in its own throwaway folder** under `%TEMP%` (named `tss-<area>-...`). No test reads or writes the
real skills folder, the real settings or the saved GitHub token. A failing test can leave its folder behind;
`scripts\dev.ps1 clean` removes those. The Start with Windows tests (`win::autorun`, `tests\cli.rs`) write only keys of
their own under `HKCU\Software\tiny-skill-sync-test`, which Windows never reads, and delete them when they end; the code
refuses to write Windows' own keys from a test.

| Where | What |
| --- | --- |
| `src\*.rs` (`#[cfg(test)]`) | Unit tests next to the code. The modules that make decisions (`reconcile`, `tree`, `scheduler`, `board`, `app`, `interval`, `pathsafe`, `autostart`) are pure, so every rule is tested without a disk or a network; so are the comparison behind **Show changes** (`diff`) and the window's pages (`win::ui::page`). `apply` and `win::fs` are tested on the real file system, `win::autorun` on the real registry. |
| `tests\engine_sync.rs`, `engine_failures.rs`, `engine_lifecycle.rs` | The engine end to end, on the real file system, with a fake GitHub (`tests\common\mod.rs`: `FakeRemote`, `World`). Failures, crashes in every step of a change, restarts. |
| `tests\downtime.rs` | Edits made while the app was stopped, and drives that cannot vouch for a file. The evidence for [How edits are found](docs/GUIDE.md#how-edits-are-found). |
| `tests\http.rs` | The real WinHTTP client against a local test server: redirects, timeouts, limits, cancel. |
| `tests\cli.rs`, `tests\app.rs` | The command line and the tray's logic, with a real engine and a fake GitHub. |

**Another drive.** `scripts\dev.ps1 test -Root FOLDER` sets `TSS_TEST_ROOT`, so the file-system tests make their folders
there: another drive, a ReFS or Dev Drive volume, or a network share. Run it when you change file-system code. A run on
an SMB share once found a bug the local NTFS runs could not.

**Live GitHub.** The tests never talk to GitHub. To try the real client, use `scripts\dev.ps1 dry-run -Repo ...` or the
sandbox (`scripts\dev.ps1 tss add ...` then `tss check`), and for everything the tests cannot see, the live runs below.

### Live end-to-end runs

The tests use a fake GitHub and no windows. `scripts\dev.ps1 e2e` is the other half: it starts the real release exes on
throwaway folders against real GitHub, drives the tray and the window the way a person would, and checks what
ends up on disk, what the app says and what it leaves alone.

- **It needs** the network, a signed-in GitHub CLI (`gh auth login`) that may push to the two fixture repositories, and a
  release build (it makes one unless you pass `-SkipBuild`).
- Live copies receive `TSS_NO_SAVED_TOKEN=1`, which prevents any read of the owner's saved app credential, including
  from Settings. They use the GitHub CLI sign-in when authentication is needed. The bench can be run with the same
  variable; it does not change production credential behaviour unless explicitly set.
- **It changes** only folders in `%TEMP%`, keys under `HKCU\Software\tiny-skill-sync-test` (which Windows never reads),
  and branches of the fixture repositories named `e2e-<time>-<scenario>` or `soak-<time>`, which it deletes afterwards.
  It refuses to touch any other branch, so `main` is never pushed to. It never touches the real skills folder, the real
  settings, the saved token or Windows' own startup keys (the `signin` scenario reads them before and after, to prove
  it). The `places` scenario, which runs the app
  without the `TSS_` folders, points `USERPROFILE` and `LOCALAPPDATA` into `%TEMP%` and checks with `cmd.exe` and
  `tss status` that the app sees only those before it lets the app write anything. A small proxy inside the script makes
  GitHub fail in the ways the `failures` scenario needs.
- **You will see** one tray icon per running copy of the app (Windows 11 keeps new ones in the overflow flyout), and the
  `ui` scenario opens, moves and photographs windows and the tray menu, keeping its throwaway window in front.
  It also moves the mouse pointer onto the window for a moment, then puts it back; puts text on the clipboard, then
  puts back the text that was there; and opens a skill's folder in File Explorer, then closes it. Leave the mouse and
  keyboard alone while it runs.
- **The result** is in `bench\out\e2e-<time>.txt`: every check, PASS or FAIL, with what was seen, plus a table at the
  end. The `ui` screenshots are in `bench\out\shots-<time>\`. The exit code is 1 if a check failed.

| Scenario | What it checks |
| --- | --- |
| `install` | First start: what is installed, how the idle app behaves, how fast it quits. |
| `places` | With `TSS_HOME` and `TSS_SKILLS_DIR` unset and the profile folders pointed at `%TEMP%`: skills go to `%USERPROFILE%\.copilot\skills` beside a skill written by hand, the settings to `%LOCALAPPDATA%\tiny-skill-sync`, nothing anywhere else. Then `skills_dir` set in `config.json`: the new folder gets its own copies and the old one is left as it is; taken out again while the tray runs, the first folder is managed as before, with an edit made there meanwhile kept and shown. |
| `portable` | `tss portable` exports two companion files without changing `config.json`; two tray copies read those shared sources but keep separate local state and skills folders. Uses the public fixture's existing branch without pushing. |
| `changes` | Six kinds of upstream change reach the disk through the tray's **Sync now**. |
| `local-edit` | An edit made while the app runs, then an upstream change to the same skill. |
| `converged` | Three skills edited here, then the edits pushed to GitHub: one with Windows line endings against GitHub's Unix ones, one byte for byte, one differently. The first two are marked as in sync with nothing written and take the next update; the third stays edited. |
| `edit-stopped` | Edits made while the app was not running, including ones that keep a file's size and time; a file added meanwhile is not reported and stays. |
| `holds` | A skill deleted locally; a skill deleted upstream after being edited; every skill deleted upstream, where only the files GitHub put there go and a file the user added stays. |
| `unmanaged` | Current and whole historical copies, an edited old copy, a longer saved search, persistent pause (including a window screenshot) and overwrite with a backup. The user's extra file survives adoption, update and removal. Makes ten small history commits only on its own fixture branch. |
| `private` | A private repository with the token and without it; the token never shows in anything the app writes or says. |
| `hostile` | A branch full of hostile shapes: nothing may be written outside the skills folder. |
| `names` | Upstream names with control and direction characters are shown escaped, and can still be selected. |
| `failures` | A bad network, bad answers, a bad token: the skills folder must not change. |
| `stale` | GitHub answering with a listing that is newer than its commit. |
| `empty` | A repository added as a whole when its skills are in a subfolder. |
| `ui` | The window, its questions and the tray menu, driven and read from outside: layout, tab order, painting, failure text, skill details in row tooltips, inline folder and clipboard actions, **Preview**, and repair choices for an edited skill. **Show changes** compares a fixture update; neither comparison nor preview should list a file the user added. Also checks the repository and diff lists widening under the mouse, scroll positions, and remembering a new window size. Runs in the current Windows light or dark mode. |
| `signin` | Start with Windows: `tss startup` on, off, and switched off in Task Manager; the app started from its entry the way Windows starts it at sign-in: only the tray, nothing looked at or checked for 30 seconds, then the look and the check; started by hand during that wait, it looks at once; a second start; settings that cannot be read; copies in folders too deep for the entry (named by the short path, or not possible). |

Run it when you change the engine's visible behaviour, the tray, the window or anything GitHub-facing, at least
the scenarios that cover the change (`-Only`). `scripts\dev.ps1 soak` is the long run for leaks and idle cost. The screens
are checked on real screen copies, not on `PrintWindow`, which sometimes returns an unpainted bottom band for a window
that is fully painted on screen.

### Fixing a bug

1. Write a test that shows the bug, and watch it fail for the right reason.
2. Fix the code. The test passes, and so does everything else.
3. Keep the test.

### Remaking the README demo

The demo needs Python 3.13, Pillow (development only), a release build, and a GitHub CLI sign-in with push access
to `AngelosP/tiny-skill-sync-fixtures-public`. The script never installs tools for you:

```powershell
python -m venv bench\out\demo-venv
bench\out\demo-venv\Scripts\python.exe -m pip install -r scripts\requirements-demo.txt
scripts\dev.ps1 demo -Python bench\out\demo-venv\Scripts\python.exe
```

It takes about five minutes plus GitHub's cache delays. It creates its own `e2e-demo-*` branch in that fixed public fixture, adds two small
skills through the app's window, and proves they reached disk. It pushes an update and **really waits for the
five-minute scheduled check**, with no manual sync. Then it edits a managed file, pushes a conflicting change,
uses **Sync now**, opens **Show changes**, and chooses **Keep my version**. Assertions check that the edit survives,
the other skill is unchanged, and the window reports each result.

Every app process uses a throwaway `%TEMP%\tss-demo-*` install, with both `TSS_HOME` and `TSS_SKILLS_DIR` set and
`TSS_NO_SAVED_TOKEN=1`. The child environment is checked before starting. The startup offer is declined.
The branch and app install are removed in `finally`, including on failure. Cleanup failures are reported and make
the script fail. There is no repository or branch override, so it cannot be pointed at a real skill repository.

Only the app's own windows are photographed, with `PrintWindow`, never a desktop capture. Because `PrintWindow`
can leave a blank band, captures are retried until the visible text controls have painted. The script refuses
captures whose controls contain a local absolute path or credential-shaped text. It uses the current Windows theme,
does not move the mouse or use the clipboard, and never opens Settings or File Explorer.

Pillow adds readable captions, pulsing highlights, a progress strip and a tray-icon illustration.
That illustration is `assets\logo-48.png` (the healthy tray icon), so it follows the logo after `scripts\dev.ps1 logo`.
The window captures themselves pick up new app icons on the next run. No logo is required in the README.
The five-minute wait is labelled as shortened; the later manual check is labelled too.

The output is 960 by 760 pixels, 25 seconds, loops forever and must be strictly under 5,000,000 bytes.
The renderer checks the decoded frame count, duration and loop, exports every decoded frame for review, and runs
twice on the same inputs to prove identical GIF bytes. A new live capture is not byte-identical: its branch,
commit and relative times change.

Raw shots, `story.json`, decoded `frame-*.png` files and both encodings are left in `bench\out\demo-*`.
Inspect every frame for clipped text, blank areas and private information before committing `docs\demo.gif`.
The web page shows the same GIF: `.github\workflows\pages.yml` copies `docs\demo.gif` into the published site, so a new
GIF reaches the page by itself on the next push to `main`. Keep the page's caption (`site\index.html`) and the
README's caption the same.
To re-encode those same captures without another live run:

```powershell
bench\out\demo-venv\Scripts\python.exe scripts\demo_gif.py bench\out\demo-RUN\story.json bench\out\demo-RUN\demo.gif
```

Run `scripts\dev.ps1 test` after changing the script. `scripts\dev.ps1 clean` removes the capture folders,
the Python environment and any abandoned demo install, but keeps the committed GIF.
When worktrees share this PC, do not run the global cleanup while another run is active: remove only your own
worktree's outputs. Serialize live runs with the coordination lock agreed for that work.

### When a new test passes at once

For code that protects the user's files (`apply`, `reconcile`, `verify`, `text`, `win::fs`, the engine), a test that
passes on its first run is not yet proof that it tests anything. Check it with deliberate bugs ("mutants"):

1. Make one small wrong change in the code the test should guard (flip a condition, skip a step, return early).
2. Run the tests. At least one must fail. If none does, the tests have a gap: add a test.
3. Undo the change, and make sure the file is byte for byte as before (compare its hash). If you undo it by copying a
   saved copy back, give the file the current time afterwards (`(Get-Item FILE).LastWriteTime = Get-Date`): a copy
   keeps the saved copy's older time, so cargo does not rebuild, and the next test run still runs the bug.

This has been done for `apply`, the engine, the downtime tests, the board, the HTTP layer and the command line, the
start at sign-in (`app`), the preview of a skill's files (the engine and the window's page), and for the `places`
scenario (a wrong default skills folder stops it before the app writes anything).

## Measuring

```powershell
scripts\dev.ps1 bench
scripts\dev.ps1 bench -Repo https://github.com/github/awesome-copilot/tree/main/skills -Runs 5
```

The bench builds the release exes first. It installs real skills from GitHub into a throwaway folder (this needs the
network), then starts the release tray app many times and reads Windows' own counters: time to the tray icon, the
start-up check, the start at sign-in, CPU, memory, disk and network operations while idle, the window and an unchanged
poll. The app writes timing marks only when `TSS_BENCH_REPORT` names a file (`src\win\ui\bench.rs`).

It prints a table, saves it as `bench\out\bench-<time>.md` (ignored by git) and exits with 1 if any row is over budget.
The window step briefly shows the app's window five times; leave the mouse and keyboard alone while it runs.

The budgets, agreed before the app was written:

| Measure | Budget |
| --- | --- |
| Process start to tray icon | median 30 ms, p95 60 ms |
| Start-up check | 10 ms for an install of 84 files; 75 µs per file at scale |
| Start at sign-in | no job in the first 3 s after the icon; under 300 ms of CPU and 300 KB of file I/O by then (Task Manager's "low" startup impact) |
| Idle | 0 CPU, 0 disk or network operations, 0 page faults |
| Idle memory | 12 MiB working set, 2 MiB once trimmed; 4 MiB private |
| Window | open to painted in 100 ms; 40 MiB working set while open; 2 MiB (trimmed) after it closes |
| Unchanged poll | 50 ms CPU, 0 file reads or writes, 1 request |
| After a poll | 20 MiB working set, 5 MiB private, 10 threads, 350 handles |
| Quit | 2 s at most |
| `tiny-skill-sync.exe` | 1.1 MiB |

The exe's budget was 1 MiB until the shared-folder settings, the new skill rows and the check that marks an edit
GitHub also has as in sync took it to 1.01 MiB; the owner raised it to 1.1 MiB. The other rows are as first agreed.

The recorded results and their test conditions are in [the user guide](docs/GUIDE.md#measured-cost).

## Code style

- `cargo fmt` (`rustfmt.toml`: 160 columns) and clippy with no warnings.
- Comments explain why, not what. Module comments (`//!`) say what a module is for and what it promises.
- Text the user reads (the tray, the window, `tss`) is short, plain English, and says what to do next. `tss`
  and the scripts print ASCII only, so every console shows it.
- Windows calls stay in `src\win\`. The rest of the code is plain Rust that tests can drive.
- A page is a `Surface` built by the pure `src\win\ui\page.rs` and shown with `control::set`
  (`src\win\ui\control.rs`), which makes, updates and scrolls its controls. When a kind of control is missing, add a
  `Kind` or `Look` there rather than calling `CreateWindowExW` in a window's code.
- Dependencies: `windows-sys`, `serde` and `serde_json` only. A new dependency needs a good reason: every one costs
  build time and start-up time.

## Release build

`Cargo.toml` and `.cargo\config.toml` set the release profile: `opt-level = "z"`, fat LTO, one codegen unit,
`panic = "abort"`, stripped symbols and a static C runtime (no Visual C++ redistributable needed). `opt-level = "z"`
keeps the tray exe at 1,057,280 bytes, within its 1.1 MiB budget (in the earlier comparison, `opt-level = "s"` made the
0.89 MiB exe 1.01 MiB, `opt-level = 3` larger still). It costs about 12% of SHA-1 speed (339 MB/s instead of 384) and nothing the
bench can see. `build.rs` embeds
`app.manifest` (common controls 6, per-monitor DPI, long paths, UTF-8), a version resource: the name and version
Windows shows for each exe, as in Task Manager's startup apps (about 1 KiB), and the icon, `assets\tiny-skill-sync.ico`
(about 13 KB).

## Releases

`.github\workflows\release.yml` publishes a GitHub release when a tag `vX.Y.Z` is pushed:

1. Set `version` in `Cargo.toml` and in `app.manifest` (`assemblyIdentity`, with a fourth `.0`), build once so
   `Cargo.lock` follows, and commit. Run the live scripts (`e2e`, and
   `bench` for a change on a measured path) on that commit: the workflow cannot, as they need the fixture repositories
   and a desktop.
2. `git tag -a vX.Y.Z -m "tiny-skill-sync X.Y.Z"`, then `git push origin main vX.Y.Z`.

On a Windows runner, the workflow refuses a tag that does not match `Cargo.toml`, runs `scripts\test.ps1` and
`scripts\build.ps1`, and publishes `tiny-skill-sync-X.Y.Z-windows-x64.zip` (both exes, the README, user guide and demo GIF) with
`SHA256SUMS.txt` (the zip and both exes). A tag with a hyphen (`v0.2.0-rc.1`) makes a pre-release. To check that a
release would build, without publishing anything: `gh workflow run release.yml`, then `gh run watch`.

The workflow uses the Rust version in its `RUSTUP_TOOLCHAIN`, so a new clippy lint cannot block a release. Raise it when
you move to a newer Rust here.

## The web page

`site\` holds the project's one-page site, `https://angelosp.github.io/tiny-skill-sync/`: `index.html` and
`style.css`, plain HTML and CSS with no build step. It follows the device's light or dark mode with the app's colours
(`src\win\ui\theme.rs`). Keep its wording in step with the README and [docs/GUIDE.md](docs/GUIDE.md). It shows the
README's demo, `docs\demo.gif`, and the logo from `assets\` (`logo.svg` beside the name and as the favicon,
`favicon.ico` for older browsers, `logo-256.png` as the link preview). None of them has a second copy in the
repository.

**Preview it** with any static server, then open `http://127.0.0.1:8765/`. Copy the GIF and the logo in first (git
ignores the copies):

```powershell
Copy-Item docs\demo.gif, assets\logo.svg, assets\logo-256.png, assets\favicon.ico site\
python -m http.server 8765 --bind 127.0.0.1 --directory site
```

Check it in light and dark mode, in a narrow and a wide window, and with the keyboard alone (Tab, Enter). Stop the
server afterwards (Ctrl+C).

**Publishing.** `.github\workflows\pages.yml` copies `docs\demo.gif` and those three logo files into `site\` and
publishes `site\`, on every push to `main` that changes `site\`, the GIF, the logo files or the workflow, and by hand
(`gh workflow run pages.yml`). Only what is in
`site\` is published, so any other picture the page shows must be in `site\`. Pages is turned on with **GitHub
Actions** as the source (**Settings**, **Pages**, **Build and deployment**); on GitHub Free that needs a public
repository. The site is public, and its download links work only while the releases are public.

## Spikes

`examples\` and `spikes\` hold the experiments the design was based on. They are not part of the app and are built only
with `--features spikes`. See [spikes/README.md](spikes/README.md).
