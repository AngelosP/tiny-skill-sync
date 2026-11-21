# Data memory

## Map

- [Files on disk](../../ARCHITECTURE.md#files-on-disk): settings, ownership records, locking and folder identity.
- [File changes](../../ARCHITECTURE.md#changing-the-skills-folder): transactions, recovery and backups.
- [Files](../../docs/GUIDE.md#files): user-facing persistence behavior.
- [Contributor rules](../../AGENTS.md): disposable data only; never inspect the real installation.

## Commands

- `scripts\dev.ps1 test -Quick -Filter TEXT` for relevant persistence or recovery tests.
- Use the existing [test helpers](../../CONTRIBUTING.md#tests); no database tooling is needed for the JSON files.

## Learned

- `state.json`: v0.7.0 (format 1) decodes a lenient `State`, then refuses `version > 1` untouched. The frozen
  `v1_reader` in `src/state.rs` tests emulates it. Format 2 is written only when `loose_sources` or `plugin_sources`
  is non-empty (`wire_version`); `save` refuses a mismatch.
- The top-level `State` ignores unknown keys (`#[serde(default)]`, no `deny_unknown_fields`). A reader at the same
  format silently drops a namespace it doesn't know on its next save, so a new namespace needs a format bump. The
  nested plugin structs use `deny_unknown_fields`, and `NameMap` rejects duplicate keys.
- Engine saves: `conclude` saves only on `Ok`, but `finish` saves whenever `must_save` is set (Write access), so
  verified partial progress is persisted on `Err`. `sync_all` saves once per source, after all of that source's
  actions.
- `Engine::open` runs `roots.validate_state`. A cross-kind ID collision (a typed or plugin source ID matching a
  skills, loose or plugin record, orphans included) fails every call, including `remove`. The d52f694 test
  `a_typed_config_cannot_verify_or_release_an_old_skill_with_the_same_id` asserts this. `Config::unique_id` checks
  only config IDs, never state.
- Portable `export`/`save_changes` write only Config (settings and sources). Plugin consent stays in local state.

## Baselines

- Issue #7 item F (plugins) review, after the change, on uncommitted work over 893cebe: `scripts\dev.ps1 test -Quick -Filter`
  state, plugin, config and portable all passed (lib 31/23/25/17; also app, cli, engine_plugins, engine_sources,
  source_preview).

## Open proposals

None.
