# PortOS Agent Guide

`AGENTS.md` links to this file (`CLAUDE.md`); edit this shared source.
PortOS is a Rust workspace developing an Agent OS/Runtime.

**There is one kind of runnable thing: a plugin** — a process that speaks the
ABI. The kernel has no notion of kinds and nothing branches on one. The tree
says so: `abi/` is the wire, `kernel/` is the kernel, `sdk/` is the plugin
side, `drivers/` holds the interfaces that regulate a class of plugin (they
never run), `plugins/` holds everything that does, and `cli/` is the front
door. **New components are written as plugins, and any plugin can be
replaced by another implementation of its driver's interface with no change
anywhere else.** Nothing outside a plugin may find it by name — only by what
it answers. `cli/tests/run.rs` proves it on the model driver.

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

### 1b. No Special Cases

**A special case is a design defect reporting itself. Stop.**

When something needs an `if` before the general path — a reserved name handled
before dispatch, a verb intercepted before it is routed, one caller that must
be treated differently — do not write the branch. Go back and look at the
design, because the branch is evidence that the general mechanism is the wrong
shape or is missing something.

This is not a style preference. It has cost real time here: `kernel::*` was
intercepted before routing, `artifact::read` was intercepted before invoking,
`remote` built a parallel router outside the kernel, and each of those made the
missing abstraction — one routing mechanism — look *less* needed rather than
more. Three workarounds read as three solved problems.

Corollary, because it is the harder half: **a good workaround is worse than a
bad one.** It hides the gap. When you find yourself pleased with how cleanly
you routed around something, that is the moment to stop.

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

The goal is a usable AI workstation, and "usable" is the only acceptance
criterion; build order is `.dev/plans/workstation-v1.md`, rationale is
`.dev/gen/architecture.md`.

- The kernel knows processes, `driver::verb`, capabilities, handles and
  events; domain vocabulary in `kernel/` is a violation.
- A plugin is found by what it answers, never by name; with several
  answerers the caller names an instance or gets an error, never a default.
- A driver interface lives in `drivers/`, never runs, and states verbs,
  types and `tools()`; a new kind of plugin adds a driver, not a kernel
  mechanism.
- A driver has no version: a compatible change adds verbs, an incompatible
  one is a new driver under a new name.
- A plugin is data: `portos plugin` captures a launch's hello into a
  manifest in the CAS, and a spec names the plugin by that one id; a
  manifest is generated from a run, never written by hand.
- What ran is recorded by content (`ran`) whatever the spec named it by;
  `bin` is the development path and costs the record nothing.
- Nothing is reserved and nothing is intercepted before routing; `kernel::*`
  are ordinary rows in the one route table.
- Grants in `portos.json` are the tool surface and the whole approval
  story; a grant is on the driver, which instance is routing.
- A result that may be large answers `Bulk` via `Sink`; the model sees
  handles, never paths.
- `Payload` is opaque; `serde_json::Value` is for a provider's wire format,
  the audit log and operator config only.
- Plumbing (`config`, `scope`, `bulk`) lives in the SDK, not in plugins.
- Dependencies are `Plugin::needs`; unmet means running but not routed, and
  nothing declares an order.
- Subscriptions are declared in `hello` and live before the spawn returns;
  there is no runtime subscribe.
- A long operation is accepted, not awaited, and owes exactly one terminal
  event.
- Teardown escalates to SIGKILL and the cgroup; never rely on `Child::kill`.
- The launcher (`portos run`) is the only non-plugin and knows no driver.
- The API key exists in no process but the broker.
- Deliberately absent, not to be reintroduced without a spec: effect plans,
  consent ceremonies, taint gates, resource classes, ledgers and leases,
  and PortOS as anyone's MCP server.

## Build and Validate

```sh
cargo build --workspace   # first: a package with no tests (remote) is not
cargo test --workspace    # rebuilt by `cargo test`, and tests find plugin
cargo fmt --all           # binaries beside their own
```

102 tests; all pass, zero warnings. Grep the output for `skipping:` before
believing a green run: `cli/tests/run.rs` skips without `node` and
`npm install` in `plugins/browser`, `plugins/echo/tests/form.rs` without
writable cgroup v2. Run the narrowest relevant test first, then the whole
workspace before reporting done. A driver is proven by `cli/tests/run.rs`,
the walking skeleton, never by unit tests alone. What each test file
asserts: `.dev/gen/tests.md`.

## Configuration and Conventions
- Use Rust 2024, existing style, and tracing initialized only in binaries:
  `info` for lifecycle, `debug` for internals, `warn` for recoverable issues,
  `error` for unrecoverable failures. Update schemas/config/docs with contract changes.
- Use Conventional Commit prefixes (`feat:`, `fix:`, `refactor:`, `ci:`, `chore:`).
- Describe in comment, instead of referencing to documentation section labels.

## Workspace Codemap

- `abi/` (`portos-abi`): the wire. `wire`, `ids`, frame codec, chunks,
  `bulk`, `boundary`.
- `kernel/` (`portos-kernel`): `caps`, `cas`, `host`, `routes`, `audit`.
- `sdk/rust/` (`portos-sdk`): `Plugin`, `KernelClient`, `config`, `scope`,
  `bulk`. `sdk/js/client.js` is its JS twin.
- `cli/` (`portos-cli`): `init|put|bundle|plugin|meta|get|audit-verify|sessions|run`.
- `drivers/`: interfaces only. `kernel`, `egress`, `model`, `fs`, `shell`,
  `browser` (with `tools.json`), `link`, `router`.
- `plugins/`: `broker`, `echo` (the kernel's end-to-end tests live in its
  `tests/`), `model`, `model-echo`, `tty`, `browser` (JS), `fs`, `shell`,
  `remote`, `bridge-http`, `render-tty`.
- `docs/`: not yet. `.dev/` (gitignored): `plans/`, `gen/`, `root/`, `tmp/`.

## Documentations
Temporal plans lives in `.dev/plans`, describing what's going to do.
Agent generated documentation could only lives under `.dev/gen`.
Only solid and documentation could go into `docs/` after human approval / refinement.

When writing, using plain and comprehensive sentences with accurate term use.

## Details by Topic

Keep this guide concise; put implementation details in the linked docs or code.
