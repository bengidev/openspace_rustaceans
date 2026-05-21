# Architecture

This document describes how OpenSpace fits together. It is a target-state
sketch — not all layers exist in code yet. Decisions that lock pieces in
place are recorded as ADRs in `docs/adr/`.

Read `CONTEXT.md` first for vocabulary.

## Goals that shape the architecture

1. **Three modes, one shell.** Terminal, Chat, and Editor share keymaps,
   command palette, theming, and a single AI core. Modes do not duplicate
   infrastructure.
2. **Provider-agnostic.** The product does not commit to one model vendor or
   one local engine. Swapping the backend is a configuration change, not a
   rewrite.
3. **Workspace-scoped.** Anything stateful (history, memory, configuration)
   is scoped to the open workspace by default. Cross-workspace state is
   opt-in and explicit.
4. **Auditable side effects.** Every tool call the assistant makes is
   visible, gated by consent, and logged inside the session.
5. **Local-first feel.** The user's machine is the source of truth for
   workspace data. Network calls happen because the user picked a hosted
   provider, not because the shell needs the network to function.

## Layered view

```
┌────────────────────────────────────────────────────────────────┐
│                    Desktop shell (UI runtime)                  │
│  window mgmt · theming · keymap · command palette · panes      │
└──────────────┬─────────────────────────────────────┬───────────┘
               │                                     │
       ┌───────▼────────┐  ┌─────────────────┐  ┌────▼─────────┐
       │  Terminal Mode │  │   Chat Mode     │  │ Editor Mode  │
       │  (PTY, output) │  │  (threads, UI)  │  │ (buffers,    │
       │                │  │                 │  │  diagnostics)│
       └───────┬────────┘  └────────┬────────┘  └──────┬───────┘
               │                    │                  │
               └────────────┬───────┴──────────────────┘
                            │
                ┌───────────▼──────────────┐
                │         AI core          │
                │  session · context win   │
                │  tool dispatch · stream  │
                └───┬────────────┬─────────┘
                    │            │
       ┌────────────▼──┐    ┌────▼────────────┐
       │ Provider      │    │ Tooling layer   │
       │ adapters      │    │ fs · shell ·    │
       │ (hosted/local)│    │ search · index  │
       └───────────────┘    └─────────────────┘
                    │            │
                    └─────┬──────┘
                          │
                ┌─────────▼─────────┐
                │  Local services    │
                │ config · workspace │
                │ memory · telemetry │
                └────────────────────┘
```

## Layer responsibilities

### Desktop shell

The outer process. Owns the window, the global keymap, the theming pipeline,
and the cross-mode command palette. Hosts panes that render modes.

The shell is **dumb about AI**. It does not know which provider is in use or
what the assistant is doing — it forwards events to the active mode and
draws what the mode tells it to draw.

### Modes

Three sibling modules. Each one:

- Renders its own surface inside a pane the shell hands it.
- Owns its mode-specific commands (e.g. `editor.formatBuffer`,
  `terminal.runSelection`, `chat.pinMessage`).
- Talks to the AI core through a single, stable client interface.
- Does **not** depend on the other modes' internals. Cross-mode interactions
  go through the shell's command surface.

Mode internals are deliberately scoped:

- **Terminal Mode** wraps a PTY, captures output streams, and feeds them as
  context to the assistant when the user invites it.
- **Chat Mode** owns threads, attachments, and the conversational rendering.
- **Editor Mode** owns buffers, syntax services, and inline assistance —
  diagnostics and refactors are buffer-scoped.

### AI core

The brain of the product. Receives a request from a mode (`generate`,
`continue`, `tool result`), assembles the next prompt from session history
and the current context window, dispatches to the chosen provider adapter,
and streams tokens and tool calls back to the caller.

The AI core also enforces **tool consent**: any tool call the model emits is
held until the gating policy approves it (auto-allow, prompt the user, or
deny). The core, not the mode, decides what gets executed.

### Provider adapters

Thin shims that translate the AI core's internal request shape into a
specific runtime's protocol. Adapters are responsible for:

- Authentication and credential handling for that runtime.
- Token streaming and back-pressure.
- Mapping native tool-call formats into the core's normalised shape.
- Error translation (rate limits, context overflow, transport errors).

Adapters do not own session state. They are stateless from the core's point
of view, modulo any short-lived connection handles.

### Tooling layer

A small, deliberate set of side-effecting capabilities the assistant can
request:

- Filesystem read and (gated) write inside the workspace.
- Shell command execution (gated, surfaced in the terminal mode trail).
- Workspace search and indexing.
- Diagnostics fetched from the editor mode.

New tools are added with intent, not by accumulation. Each tool documents
its consent surface and its failure modes.

### Local services

Cross-cutting infrastructure that everyone above can rely on:

- Configuration loading (workspace-level, user-level, defaults).
- Workspace metadata and session storage.
- Long-term memory (opt-in).
- Telemetry hooks (opt-in, local-first; nothing leaves the machine without a
  user-flipped switch).

## Data flow: a representative turn

A user typing in chat mode and asking the assistant to read a file:

1. Chat mode posts a `submit` to the AI core with the new user turn.
2. AI core appends the turn to the session, builds the next context window,
   and calls the active provider adapter.
3. Adapter streams tokens back. The model emits a tool call requesting a
   file read.
4. AI core suspends generation, raises a consent prompt through the active
   mode (chat).
5. User accepts. AI core invokes the filesystem tool, which returns the
   file contents scoped to the workspace.
6. AI core feeds the tool result back to the adapter as the next turn.
7. Adapter streams the assistant's final answer; chat mode renders it.
8. Session log records the user turn, tool proposal, consent decision,
   tool result, and assistant turn — in that order.

Every other turn type (terminal command suggestion, editor refactor)
follows the same pattern with different originating mode and tool set.

## Cross-cutting concerns

### Configuration

Three layers, in increasing precedence:

1. Defaults compiled into the binary.
2. User-level configuration (per machine).
3. Workspace-level configuration (checked into the workspace, optional).

A single resolution pass merges them at startup and on workspace switch.

### Sessions and history

A session is per-mode and per-workspace. Sessions can be pinned, named, and
resumed. Session storage is workspace-local. There is no cloud sync layer
in scope for the early phases.

### Security boundaries

- The workspace folder is the filesystem boundary for tool reads/writes by
  default.
- Shell command execution is gated and recorded.
- Provider credentials are stored through the OS-native secret store where
  available; otherwise an explicit, documented fallback.
- Plugin/extension loading is **not** in scope for the early phases; when
  it lands, it gets its own ADR and threat model.

### Error handling

Errors propagate up to the originating mode with enough context for the
user to act:

- Adapter errors carry the provider name and a normalised category.
- Tool errors carry the tool name and the consent state at the time.
- The shell never silently swallows a turn; a failed turn is visible in the
  session log.

## Non-goals (for now)

- Multi-tenant or multi-user instances. OpenSpace is a single-user desktop
  application.
- Cross-device sync.
- A plugin marketplace.
- Mobile or web clients.

These are deliberately deferred to keep the core surface small.
