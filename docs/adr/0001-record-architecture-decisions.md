# ADR-0001: Record architecture decisions

- **Status:** Accepted
- **Date:** 2026-05-21

## Context

OpenSpace is a multi-mode desktop assistant with several long-lived design
choices already implicit in the codebase: the three-mode model, the
provider-agnostic AI core, the workspace-scoped state model. As the project
grows, contributors — human and AI — will keep asking *why* these shapes
exist, and the answer will drift if it lives only in chat history or one
person's head.

The repository already adopts a **single-context** documentation layout
(`CONTEXT.md` plus `docs/adr/`) per `docs/agents/domain.md`. What was
missing was an explicit commitment to use that layout and a shape for
records that go inside it.

## Decision

We will use lightweight Architecture Decision Records to capture decisions
with long-lived consequences.

- ADRs live in `docs/adr/` as `NNNN-kebab-case-title.md`.
- Every ADR carries a status (`Proposed`, `Accepted`, `Superseded by
  ADR-XXXX`, `Deprecated`) and a date.
- New ADRs append a row to `docs/adr/README.md`.
- Decisions that contradict an existing ADR must surface the conflict in
  the proposing PR; superseding ADRs link back to the record they replace.
- ADRs are append-only. Revisions land as a new ADR, not as an edit.

The format is the four-section template documented in
`docs/adr/README.md`: Context, Decision, Consequences, Alternatives
considered.

## Consequences

What gets easier:

- New contributors and AI agents can recover the reasoning behind the
  shape of the codebase by reading a small set of focused records.
- Reverting or revisiting a decision becomes a deliberate act with a
  visible trail.
- Reviewing a PR can include an explicit check: does this contradict any
  ADR?

What we accept:

- A small ongoing tax: significant decisions need a record, and the index
  needs to stay in sync.
- The need to resist over-using ADRs for trivial choices. Not every
  refactor is an ADR.

## Alternatives considered

- **No ADRs, rely on commit messages.** Commit messages are too granular
  and get lost in history. They do not surface the *option space* a
  decision was made within.
- **A single `DECISIONS.md` file.** Easier to start, but it conflates
  decisions, makes superseding awkward, and grows into an unread
  monolith.
- **Wiki pages.** Out-of-tree, drifts from the code, harder to review.
