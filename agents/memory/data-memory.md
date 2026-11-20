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

No additional findings recorded.

## Baselines

No persistence review or test results recorded.

## Open proposals

None.
