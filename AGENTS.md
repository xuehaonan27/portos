# AGENTS.md
## Workspace
This is a Rust workspace.

- `crates`: where project code lives.
- `docs`: documentation that's short and accurate, describing current status, short and accurate enough for human to read.
- `.dev` (gitignored): temporal development space, never added into git worktree.
    - `.dev/plans`: plans that's a draft, describing what's going to do.
    - `.dev/gen`: store agent generated documentations.
    - `.dev/root`: ephemeral `root` that `PortOS` uses when doing experiment, test or debugging. Wiping content is allowed in this ephemeral root. Use this instead of creating directory under `/tmp` when possible. Create more directories of roots under `.dev/tmp` with prefix `root-` if multiple roots needed.
    - `.dev/tmp`: everything else that should lives in `.dev` but not in categories described above.

## Documentations
Temporal plans lives in `.dev/plans`, describing what's going to do.
Agent generated documentation could only lives under `.dev/gen`.
Only solid and documentation could go into `docs/` after human approval / refinement.

When writing, using plain and comprehensive sentences with accurate term use.

## Git
Git commit message should use English, with Conventional Commits format:
```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```
