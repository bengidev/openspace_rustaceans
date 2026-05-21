# ADR-0002: Core operating modes

- **Status:** Accepted
- **Date:** 2026-05-21

## Context

OpenSpace is positioned as a desktop AI assistant integrated into the
user's actual workflow rather than a separate chat surface. The product
needs a clear answer to: *what are the surfaces a user works in, and how
do they relate?*

Two failure shapes from the broader category to avoid:

- **A chat window with extras.** Pure chat surfaces force the user to
  copy state in and out. The assistant never sees what is actually on
  the screen, and side effects happen out-of-band.
- **A monolithic IDE with a chat sidebar.** Treating chat as a side
  panel inside an editor pins the product to one work style and buries
  the assistant for users whose primary surface is a terminal or a
  long-form thread.

We need a model where AI capability and workflow surfaces are equal
citizens.

## Decision

OpenSpace ships **three first-class modes**: Terminal, Chat, and Editor.

- Each mode is a sibling, not a sub-feature of another.
- Each mode owns its own keymap, command palette entries, and command
  set.
- Each mode talks to the AI core through the same client interface; no
  mode has a privileged path to the model.
- The shell and the AI core are mode-agnostic. They do not contain
  per-mode special cases.
- Cross-mode interactions go through the shell's command surface, not
  through direct mode-to-mode coupling.

The vocabulary is fixed in `CONTEXT.md`. The architectural placement is
described in `docs/architecture.md`.

## Consequences

What gets easier:

- The product can speak to multiple work styles without reshaping its
  core for each one.
- Adding capabilities to the AI core lifts all three modes at once.
- Mode authors can move independently; a change inside editor mode does
  not ripple into chat mode.

What gets harder:

- Three real modes is more surface area than one mode-with-panels. We
  pay that cost in implementation effort and in UX consistency work.
- "First-class" is a discipline. There will be pressure to ship a
  half-finished mode; we have to resist or explicitly downgrade it in
  the roadmap rather than letting it become a second-class citizen by
  attrition.
- Cross-mode workflows (e.g. send a terminal output to chat) need
  deliberate design — they cannot rely on shared internals.

What we accept:

- The product cannot be everything to everyone. A user whose work has no
  terminal, chat, or editor element is not the target.
- Adding a fourth mode would be a significant decision and would need
  its own ADR.

## Alternatives considered

- **Single mode with switchable panels.** Simpler shell, but every
  mode-specific keymap and command becomes a special case. The
  "switchable panel" pattern tends to favour whichever surface was
  built first.
- **Two modes (chat plus editor, or chat plus terminal).** Cuts scope
  but leaves a real audience without a primary surface. Adding the
  third mode later forces invasive refactors.
- **Many small modes (chat, terminal, editor, notes, browser, …).** The
  product surface explodes, the consent model gets harder to reason
  about, and the AI core ends up with mode-specific quirks.
