# PortOS Overview

PortOS is an agent OS/runtime written in Rust. This document states what it
is, why it exists, how the repository is organized, and where development
stands as of 2026-09-09.

## Identity

PortOS is the **trusted base for delegated computation — the issuer**. It is
the issuing and accounting party, enforced below the model, for every
capability (power), holding (resource), effect (action on the world), and
ingestion (data from the world). It does not understand what an agent is
thinking (behavior stays free); it guarantees that every unit of power has a
warrant, every holding has a ledger entry, every effect passes a gate, and
every ingested byte carries a label.

It is not an agent framework. Orchestration belongs to layers above
(LangGraph-style systems can run on top). It is not a general-purpose OS
replica, a general OS has no notion of model context, taint, or consent.

## Why it exists

Three concrete pain points converge on the identity above:

1. **Delegation without disclosure.** An AI cannot operate a user's
   logged-in web applications, and handing credentials to the AI is
   unacceptable.
2. **Control plane / data plane separation.** Moving data between tools is
   either manual labor or flows entirely through the model context.
3. **A driver model.** The system must not be hardwired to any single
   vertical.

## Core vocabulary (minimal set)

- **c-effect / w-effect**: transformation of shared state inside the
  boundary (carries an inverse, revocable by default) / action on the world
  (irreversible by default).
- **Shadow**: the in-boundary projection of a w-phenomenon — a w-effect
  casts an acquisition record in the resource ledger; a w-coeffect casts a
  taint-labeled artifact in the data plane. **The kernel can only govern
  shadows; the art of the system is forcing every w-phenomenon to cast an
  honest shadow.**
- **Reversibility tiers**: has-inverse / compensable / external — the truth
  table shared by the resource model and effect plans.
- **Four modules**: a resource-class declaration = shared structure (PCM) +
  interface theory (signature + equations) + time & failure + world
  relation.
- **WYSIWYS**: what you see is what you sign, the consent tuple.
- **crash-only**: forced reclamation is the only teardown path; graceful
  unload is the same path triggered early.
- **Issuer gate**: the kernel mints every capability and holding; it is the
  sole issuer.

## Repository layout

```
crates/    kernel and kernel-side facilities
  portos-kernel   ABI v2 host: capability table, holdings ledger (SQLite),
                  CAS data plane, event bus, routing with verb metadata,
                  plan service, audit hash chain
  portos-rm       the laws crate: executable reference for the resource
                  model (F1 ledger … F6 protocols, F8 attach drill)
  portos-broker   egress proxy: allowlist, credential injection, header
                  stripping, audit
  portos-cli      `portos chat`, end-to-end entry point
  portos-proto    shared protocol types (Handle/Ref semantics)
  portos-sdk      SDK
drivers/   driver plugins and family-interface libraries
  model-core      model family interface
  model           provider-neutral model driver
  browser         Playwright-driven browser, mounted as browser::* verbs
  render-tty      rendering as event-plane subscription
plugins/   third-party / reference implementations — swappable, never
           kernel-side (compute, model-anthropic, signer)
sdk/js     JavaScript SDK
examples/  example plans
docs/      this directory
.dev/      working corpus (gitignored): plans, design volumes, status notes
```
