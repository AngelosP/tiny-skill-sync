# .orchestrate

This folder was created by the `/orchestrate init` command (the `orchestrate`
skill for GitHub Copilot). It holds what that workflow needs to know about this
repository.

- `profile.json`: documentation map, toolchain, enabled specialist reviewers
  and their tools, forbidden commands, and install policy.
- `memory/<specialist>-memory.md`: what each specialist has learned about this
  repository, so later runs start from it.

`AGENTS.md` has a managed block (between `<!-- agent-pack:start -->` and
`<!-- agent-pack:end -->`) that points here.

Commit this folder. Run `/orchestrate init` again to check or upgrade it; it
shows each change before making it.
