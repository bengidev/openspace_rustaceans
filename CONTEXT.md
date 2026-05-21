# CONTEXT

Domain glossary for OpenSpace. When code, issues, ADRs, or commit messages
name a concept that lives in this file, use the term as defined here. If a
concept you need is missing, that is either a sign you are inventing
vocabulary the project does not use (reconsider) or a real gap (add it).

The glossary is intentionally small. Keep it that way.

---

## Core concepts

### OpenSpace
The application. A desktop shell that hosts one or more **modes** and
connects them to an **AI core** through **provider adapters**.

### Workspace
A folder-scoped working context. A workspace owns its own configuration,
session history, and assistant memory. Opening a different folder switches
workspaces. There is no global "always on" state that follows the user across
folders without their consent.

### Mode
A first-class operating surface inside the shell. The product ships three
modes:

- **Terminal Mode** — interactive shell with an AI sidekick.
- **Chat Mode** — long-form conversational surface.
- **Editor Mode** — text/code editor with inline AI assistance.

A mode is more than a panel; it owns its own keymap, command palette entries,
and command set. Modes are siblings, not tabs of one master view.

### Assistant
The user-visible AI persona inside a mode. The assistant is bound to a
**session** and reaches the model via the **AI core**. "Assistant" refers to
the surface and policies, not the underlying model.

### AI core
The internal subsystem that turns a user request plus session state into a
model call, executes any tool calls the model emits, and streams results back
to the mode. The AI core is provider-agnostic.

### Provider adapter
A pluggable backend that lets the AI core talk to a specific model runtime
(hosted API or local engine). Adapters expose a uniform interface and
translate between OpenSpace's internal request format and the runtime's
native protocol.

### Model
The language model behind a session. Selected per-workspace or per-session.
A model is named by `provider/model-id` (for example, `local/some-engine` or
`hosted/some-model`).

### Session
A bounded interaction: a sequence of user turns, assistant turns, and tool
events that share one context window. Sessions are scoped to a mode and a
workspace. A session can be pinned, named, and resumed.

### Context window
The active set of messages, attachments, and tool outputs that the AI core
sends to the model on the next turn. The context window is finite, explicit,
and visible to the user.

### Tool
A side-effecting capability the assistant can request — for example, reading
a file, running a shell command, or searching the workspace. Each tool call
is **proposed** by the model, **gated** by a consent surface, then **executed**
by the AI core. Tool definitions are deliberate and small in number.

### Action
A user-initiated command on the shell — opening a file, switching modes,
sending a message, accepting a tool proposal. Actions are observable and, for
side-effecting ones, reversible where practical.

### Memory
Durable, opt-in storage that the assistant can recall across sessions inside
a workspace. Distinct from the **context window** (per-turn) and from
**session history** (per-session).

---

## Anti-vocabulary

Avoid these synonyms in code, docs, and commits — they cause drift:

| Don't say     | Say                                   |
| ------------- | ------------------------------------- |
| "agent"       | "assistant" (in product surfaces)     |
| "tab"         | "mode" (when referring to the three)  |
| "plugin"      | "provider adapter" or "tool"          |
| "history"     | "session" (for an interaction trace)  |
| "memory" (vague) | "context window" or "memory" (per definitions above) |

The word "agent" is reserved for AI coding agents that work *on* this repo
(see `AGENTS.md`), not for the user-facing assistant inside the product.
