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

- Issue #8 (config v3, round 2): `Config::parse`/`from_parts` fill missing source/lane IDs from the config alone
  (deterministic, never state); only a brand-new source (`allocate_source_ids` + `check_fresh_id`) avoids state
  IDs. Turning a type on runs `source_preview::fresh_lane_ids` (renames a lane whose ID has foreign records).
  Lanes already on in a hand-written/imported file get no such check (cross-kind collisions fail closed in
  `validate_state`). `LegacyConfig` refuses `sources`/`global` without `"version": 3`.
- `state.json` `STATE_VERSION` = 3; `wire_version` picks 3 only when `needs_format_3()` (plugin `roots` or
  `lane_polls`). Below format 3 those fields are refused untouched (no quarantine). `lane_polls` (per lane ID,
  scheduling only, kind + location + tagged ETag) are dropped on a real record, a conflict at load, or by
  `prune_lane_polls` at startup/sync; `has_source_id`/`check_fresh_id` ignore them.
- `seen.json` (`src/seen.rs`): version 1, no lock; `seen::update` re-reads, merges, atomically replaces only on change,
  and never overwrites a newer version (`Ok(None)`).

## Baselines

- Issue #7 item F (plugins) review, after the change, on uncommitted work over 893cebe: `scripts\dev.ps1 test -Quick -Filter`
  state, plugin, config and portable all passed (lib 31/23/25/17; also app, cli, engine_plugins, engine_sources,
  source_preview).

## Open proposals

None.
