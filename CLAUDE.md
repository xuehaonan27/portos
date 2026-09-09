# PortOS Agent Guide

`AGENTS.md` links to this file (`CLAUDE.md`); edit this shared source.
PortOS is a Rust workspace developing an Agent OS/Runtime. Drivers and plugins exists in separate directories like `drivers/` and `plugins/`.

## Behavioral Guidelines

Behavioral guidelines to reduce common LLM coding mistakes, combined with the
project-specific instructions below.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks,
use judgment.

### 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:

- Read the affected module and its tests. Use the code map and linked design
  docs to locate ownership boundaries.
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them; don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes,
simplify.

### 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:

- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it; don't delete it.
- Preserve unrelated work already present in the checkout.

When your changes create orphans:

- Remove imports, variables, and functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:

- "Add validation" -> "Write tests for invalid inputs, then make them pass."
- "Fix the bug" -> "Write a test that reproduces it, then make it pass."
- "Refactor X" -> "Ensure tests pass before and after."

For multi-step tasks, state a brief plan:

```text
1. [Step] -> verify: [check]
2. [Step] -> verify: [check]
3. [Step] -> verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it
work") require constant clarification.

Run the narrowest relevant checks during development, then the applicable
repository checks below. Report what ran and any checks blocked by missing
hardware, permissions, dependencies, or credentials.

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer
rewrites due to overcomplication, and clarifying questions come before
implementation rather than after mistakes.

## Core Concepts

## Build and Validate

## Configuration and Conventions
- Use Rust 2024, existing style, and tracing initialized only in binaries:
  `info` for lifecycle, `debug` for internals, `warn` for recoverable issues,
  `error` for unrecoverable failures. Update schemas/config/docs with contract changes.
- Use Conventional Commit prefixes (`feat:`, `fix:`, `refactor:`, `ci:`, `chore:`).

## Workspace Codemap

- `crates`: kernel and kernel-side facilities.
- `drivers`: driver plugins and family-interface libraries (browser, model, model-core, render-tty).
- `plugins`: third-party / reference implementations (model-anthropic, compute, signer) — swappable, replaceable, never kernel-side.
- `docs`: documentation that's short and accurate, describing current status, short and accurate enough for human to read.
- `.dev` (gitignored): temporal development space, never added into git worktree.
    - `.dev/plans`: plans that's a draft, describing what's going to do, *MIGHT NOT* be precise or valid.
    - `.dev/design`: designs.
    - `.dev/gen`: store agent generated documentations.
    - `.dev/root`: ephemeral `root` that `PortOS` uses when doing experiment, test or debugging. Wiping content is allowed in this ephemeral root. Use this instead of creating directory under `/tmp` when possible. Create more directories of roots under `.dev/tmp` with prefix `root-` if multiple roots needed.
    - `.dev/tmp`: everything else that should lives in `.dev` but not in categories described above.

## Documentations
Temporal plans lives in `.dev/plans`, describing what's going to do.
Agent generated documentation could only lives under `.dev/gen`.
Only solid and documentation could go into `docs/` after human approval / refinement.

When writing, using plain and comprehensive sentences with accurate term use.

## Details by Topic

Keep this guide concise; put implementation details in the linked docs or code.
