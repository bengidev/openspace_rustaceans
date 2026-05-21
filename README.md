# OpenSpace

OpenSpace is a desktop AI assistant that brings language-model capabilities
into the daily workflow of a developer or knowledge worker. Instead of pushing
the user to a browser tab or a separate chat window, the assistant lives next
to the work itself: a terminal, a chat surface, and a text editor share one
shell, one context, and one set of AI capabilities.

> Status: **Early-stage / scaffolding.** Source layout, framework choices, and
> public APIs are not yet stable. Documentation describes the target shape of
> the product, not a finished implementation. See `docs/roadmap.md` for the
> rollout plan.

## Why OpenSpace

Most assistant experiences treat AI as a side channel — copy code out, paste
an answer back. OpenSpace flips that: the model sits inside the workspace and
can read what the user is reading, run what the user wants run, and edit what
the user is editing, with a single auditable trail.

Design goals:

- **Local-first feel.** Configuration, history, and project context live on
  the user's machine. Cloud models are used through provider adapters, not as
  the default home of user data.
- **Mode parity.** Terminal, chat, and editor are first-class siblings. None
  of them is a "lite" version of the other.
- **Predictable surface.** Keystrokes, panes, and commands behave the same
  across modes. No reinvented chrome per surface.
- **Auditable AI actions.** Anything the assistant does on behalf of the user
  is observable and reversible.

## Features

### Modes

- **Terminal Mode** — a working shell with an AI sidekick that can read
  output, suggest the next command, draft scripts, and explain failures
  without leaving the prompt.
- **Chat Mode** — a long-form conversation surface for planning, drafting,
  and Q&A. Supports attachments, slash commands, and pinned context.
- **Editor Mode** — a text/code editor with inline AI completions, refactors,
  and a scoped review companion that operates on the open buffer.

### Cross-cutting capabilities

- **Workspaces** — folder-scoped projects with their own history and
  configuration.
- **Provider adapters** — pluggable backends for hosted and local model
  runtimes. The shell does not assume a single vendor.
- **Tooling layer** — a small, deliberate set of side-effecting tools
  (filesystem, shell, search) that the assistant can request, gated by a
  consent surface.
- **Session memory** — short-term context plus an opt-in durable memory
  scoped to a workspace.

See `CONTEXT.md` for vocabulary and `docs/architecture.md` for how these
pieces fit together.

## Quick start

OpenSpace is built in Rust. Once a `Cargo.toml` lands at the repo root, the
expected workflow is:

```sh
# Build a development binary
cargo build

# Run the desktop shell
cargo run

# Run the test suite
cargo test
```

Until the crate manifest is in place, this section is aspirational. Track
progress in `docs/roadmap.md`.

## Repository layout

```
/
├── README.md            ← you are here
├── CONTEXT.md           ← domain glossary
├── CONTRIBUTING.md      ← how to contribute
├── CHANGELOG.md         ← release notes
├── SECURITY.md          ← vulnerability reporting
├── CODE_OF_CONDUCT.md   ← community expectations
├── LICENSE              ← MIT
├── AGENTS.md            ← guidance for AI coding agents
├── docs/
│   ├── architecture.md  ← system architecture
│   ├── roadmap.md       ← phased delivery plan
│   ├── adr/             ← architecture decision records
│   └── agents/          ← agent-facing playbooks
└── src/                 ← (to be added) Rust sources
```

## Documentation map

| If you want to…                                  | Read                              |
| ------------------------------------------------ | --------------------------------- |
| Understand what OpenSpace is                     | this `README.md`                  |
| Look up a domain term                            | `CONTEXT.md`                      |
| See the high-level system design                 | `docs/architecture.md`            |
| Find out what's planned next                     | `docs/roadmap.md`                 |
| Trace why a decision was made                    | `docs/adr/`                       |
| Contribute code or docs                          | `CONTRIBUTING.md`                 |
| Report a security issue                          | `SECURITY.md`                     |
| Work on the repo as an AI agent                  | `AGENTS.md`                       |

## License

Released under the MIT License. See `LICENSE` for the full text.
