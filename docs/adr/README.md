# Architecture Decision Records

This directory captures decisions that shape OpenSpace's architecture.

An ADR is a small document — usually a page or two — that records:

- The context that forced a decision.
- The decision itself.
- The consequences, both good and inconvenient.

ADRs are append-only. When a decision is revisited, the new ADR
**supersedes** the old one and links back to it. The old record stays in
place so future readers can trace the reasoning.

## When to write an ADR

Write one when a choice will be hard to reverse, will affect multiple
modules, or will surprise a new contributor reading the code without
context. Examples:

- Choosing the implementation language or UI runtime.
- Deciding on a mode model, tool model, or provider adapter shape.
- Picking a storage format that other components will rely on.
- Adopting or dropping a major dependency.

If a change can be redone in an afternoon by one person without breaking
anyone else, it does not need an ADR.

## Format

Use this skeleton (see `0001-record-architecture-decisions.md` for a
worked example):

```
# ADR-NNNN: <title>

- **Status:** Proposed | Accepted | Superseded by ADR-XXXX | Deprecated
- **Date:** YYYY-MM-DD

## Context
What forced this decision? What constraints, prior choices, or pain points
matter here?

## Decision
What we decided. State it as a clear, falsifiable claim.

## Consequences
What gets easier. What gets harder. What we accept by choosing this path.

## Alternatives considered
Briefly: what we did not pick, and why.
```

## Naming

Files are numbered sequentially: `NNNN-kebab-case-title.md`. Numbers do
not get reused — even when an ADR is superseded.

## Index

| #    | Title                                  | Status   |
| ---- | -------------------------------------- | -------- |
| 0001 | Record architecture decisions          | Accepted |
| 0002 | Core operating modes                   | Accepted |
| 0003 | Rust as the implementation language    | Accepted |

When you add an ADR, append a row here in the same PR.
