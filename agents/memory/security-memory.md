# Security memory

## Map

- [Contributor rules](../../AGENTS.md): protected data, credentials, registry keys and file-safety invariants.
- [Reconciliation](../../ARCHITECTURE.md#deciding-the-reconcile-table) and [file changes](../../ARCHITECTURE.md#changing-the-skills-folder).
- [Private repositories](../../docs/GUIDE.md#private-repositories): authentication behavior.

## Commands

- `git --no-pager diff -- <assigned-paths>` for the assigned trust-boundary changes.
- `scripts\dev.ps1 test -Quick -Filter TEXT` for relevant existing safety tests.
- No external scanner is configured in the profile; do not install one during a feature run.

## Learned

No additional findings recorded.

## Baselines

No security review, scan or test results recorded. Source inspection is not scanner coverage.

## Open proposals

None.
