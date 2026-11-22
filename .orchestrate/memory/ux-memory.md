# UX memory

## Map

- [The tray and the window](../../ARCHITECTURE.md#the-tray-and-the-window): native controls, layout and interaction.
- [The window](../../docs/GUIDE.md#the-window): user-facing behavior.
- [Live runs](../../CONTRIBUTING.md#live-end-to-end-runs): UI checks and screenshots.

## Commands

- `scripts\dev.ps1 test -Quick -Filter TEXT` for relevant pure page or app tests.
- `scripts\dev.ps1 e2e -Only ui` only after the coordinator has approval for fixture writes and has coordinated desktop access.
  Inspect its saved screenshots; do not infer visual success from unit tests.

## Learned

- `tss` output is forced to ASCII by `ascii_lines` in `src\cli.rs` (applied in `run()` and before the add prompt);
  `help_is_plain_ascii` covers HELP. Tray text may use Unicode.
- The control layer has no `WM_GETOBJECT`/UIA provider (none in `src\win`): assistive tech gets only each control's
  window text. Paragraphs are not Tab stops, so text shown only in a paragraph is not heard while tabbing. Row text is
  `name\ntag\nfacts\nexplanation` (`RowText` in `src\win\ui\control.rs`). GUIDE "Limitations" documents this.
- Ask dialogs are a fixed `ASK_W` 468 DIP; `text` is a wrapped Body para 420 DIP wide; each radio choice adds 36 DIP
  (`page::ask`). `dialogs::fitted_size` (2026-11-22) clamps the client height to the work area and widens the dialog
  by a scroll bar when it must scroll. Ask for a small-work-area screenshot when choices are added.
- Settings rows (`page::setting`): a `Desc::Text` is measured with `fit` at its width and drawn as a WRAP para, so the
  card grows; controls stack under the text when under 160 DIP would remain beside them. `Desc::Path` is one 18-DIP
  line (path ellipsis). Titles are ONE_LINE decos. Narrowest Settings width is about 343 DIP (640 window, list 268,
  page 360, minus scroll bar). `paint::measure` and `paint::text` use the same DT_WORDBREAK|DT_EDITCONTROL flags.
- e2e `ui` matches the token description only by its second sentence; shot `02c-settings-startup-token` shows that
  row. No e2e shot shows Settings at 640 wide. Never save a token to see the saved variant (shared credential).
- App-update rate limit text is "GitHub has limited update requests. Try again later." (`release.rs`), kept even when
  the token retry fails, so a user cannot tell whether a token was tried.
- Plugin choices (Approve, Reinstall, Uninstall, Forget) dispatch with no confirmation (`window.rs` `skill()`); only
  "Delete them" asks. Plugin row status words and details live in `board.rs` `plugin_row`, shared by tray and `tss`.
- `-Filter cli` runs the lib `cli` tests and only `tests\cli.rs` tests whose names contain "cli".
- Live evidence: screenshots in `bench\out\shots-<stamp>\`, logs in `bench\out\e2e-<stamp>.txt`.

- Issue #8 (mixed sources): `page::what_to_sync` `combo_line` sizes each combo from its label width (caps the label at
  120 DIP), so at the default list width in a 640-wide window "Repository-specific skills and agents" and its value
  ("Default ...") are both cut; screenshots 05e/05g/17-long-names-narrow-items show it.
- `board::item_nouns` falls back to Skills for an empty iterator, so `page::hero_of` says "All skills are up to date /
  0 skills" when every detected type is off (07c shot).
- A source entry in the nav (`Look::Source`) has one tooltip over the whole entry (`control::source_tool_move`, also at
  HEAD). It holds the entry's window text, which is also its accessible name: `page::nav_name` (owner/repo, folder and
  branch, what each chip means). Item rows have hover tips of their own (`control::row_tool_move`).
- Source entries (UI polish, 2026-11-22): `page::source_entry` is two lines at `ENTRY_H` 56 DIP with the owner dimmed
  and the repo strong. The owner gives way first (`entry_title`/`shortened`, `MIN_OWNER` 48 DIP), so long same-named
  repos can look alike in the list until the splitter is widened; the full text stays in the tooltip and accessible name.
  The e2e fixture repo names are so long that no live shot shows the owner; the pure `entry_title` tests are the evidence.
- Live UI shot names: 05e/05f/05g come in compact and wide pairs (`05g-{wide,compact}-flat-row-tab-focused`);
  17-long-names-{narrow,narrow-items,wide}. List the shots folder before using a glob: one glob missed 05g.
- Section headers (`Look::Section`) draw chevron, icon (x+28), title (x+52), an optional warning after the title and
  right-hand marks. `section_parts` cuts the title before the warning and drops the warning when it does not fit.
  Shot 08c-collapsed-attention shows a collapsed Skills header with its warning; no shot shows What to sync collapsed.
- Light-theme contrast on the page background: Text3 #8B8B8B is about 3.1:1 (What to sync's off marks rely on it
  alone, with no tooltip); Warn #9D5D00 is about 4.7:1.
- Header band (`HERO_H` 72 in page.rs): sky, hills and edge are derived from the system accent per look (`theme.rs`
  `palette`). A light sky must be at least `RICH` (150, summed RGB distance) from white so it never reads as a wash.
  The status disc has its own ring (`BandDotRing`, 3:1 against page and sky) and the Sync now button its own
  `BandButton`/`BandButtonText` roles, with a focus ring in the lettering colour (`control::focus_ink`); other focus
  rings use Role::Text. Colours follow the accent, so judge the shots 18-band-light to 19c-band-dark-changes-narrow
  rather than quoting hex. `diff_head` stacks its sentence under the buttons at y 44 when it does not fit beside them;
  a long Failed reason can still be cut at 640 DIP (one line, no tooltip).

## Baselines

- 2026-11-20, uncommitted issue #7 F on 893cebe: `scripts\dev.ps1 test -Quick -Filter page` 36 passed;
  `-Filter board` 54 passed; `-Filter cli` 17 passed (12 lib, 5 tests\cli.rs). All 0 failed.
- 2026-11-22, uncommitted app-update token retry: `-Filter every_page_fits_a_narrow_and_a_wide_window` 1 passed,
  646 filtered out (647 lib tests). The cargo test binaries ran on this PC that day.

## Open proposals

- Plugin rows: explain Removed from GitHub (Uninstall vs Forget), Turned off in Copilot and Removed in Copilot in
  `plugin_row` details; confirm Uninstall; put the approve warning in row text (proposed 2026-11-20, issue #7 F).
- App-update rate limit: when no token could be found, add how to get one (as skill checks do); visually check
  Settings at 640 wide and the Save a GitHub token dialog once fresh exes can run (proposed 2026-11-22).
