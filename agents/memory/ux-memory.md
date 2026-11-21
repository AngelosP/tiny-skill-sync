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
- Ask dialogs grow 36 DIP per radio choice (`page::ask`); `place()` in `src\win\ui\mod.rs` clamps position, not
  height, to the work area. Ask for a small-work-area screenshot when choices are added.
- Plugin choices (Approve, Reinstall, Uninstall, Forget) dispatch with no confirmation (`window.rs` `skill()`); only
  "Delete them" asks. Plugin row status words and details live in `board.rs` `plugin_row`, shared by tray and `tss`.
- `-Filter cli` runs the lib `cli` tests and only `tests\cli.rs` tests whose names contain "cli".
- Live evidence: screenshots in `bench\out\shots-<stamp>\`, logs in `bench\out\e2e-<stamp>.txt`.

- Issue #8 (mixed sources): `page::what_to_sync` `combo_line` sizes each combo from its label width (caps the label at
  120 DIP), so at the default list width in a 640-wide window "Repository-specific skills and agents" and its value
  ("Default ...") are both cut; screenshots 05e/05g/17-long-names-narrow-items show it.
- `board::item_nouns` falls back to Skills for an empty iterator, so `page::hero_of` says "All skills are up to date /
  0 skills" when every detected type is off (07c shot).
- Source entries in the nav (`Look::Source`) have no tooltip; type-icon labels exist only in the accessible name
  (`page::nav_name`). Item rows do have hover tips (`control::row_tool_move`).
- Live UI shot names: 05e/05f/05g come in compact and wide pairs; 17-long-names-{narrow,narrow-items,wide}.

## Baselines

- 2026-11-20, uncommitted issue #7 F on 893cebe: `scripts\dev.ps1 test -Quick -Filter page` 36 passed;
  `-Filter board` 54 passed; `-Filter cli` 17 passed (12 lib, 5 tests\cli.rs). All 0 failed.

## Open proposals

- Plugin rows: explain Removed from GitHub (Uninstall vs Forget), Turned off in Copilot and Removed in Copilot in
  `plugin_row` details; confirm Uninstall; put the approve warning in row text (proposed 2026-11-20, issue #7 F).
