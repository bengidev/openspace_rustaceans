# Roadmap

A phased plan for taking OpenSpace from scaffolding to a usable desktop
assistant. Each phase has an exit criterion. Phases are sequential — a
phase finishes when its criterion is met and the changelog reflects it.

This roadmap is a planning artefact, not a contract. Priorities can shift.
When they shift, this file gets updated and an ADR captures the reason.

## Phase 0 — Scaffolding

**Goal:** the repository can be cloned, built, and reasoned about.

- [x] License, gitignore, AGENTS.md, agent playbooks (`docs/agents/`).
- [x] Documentation set: `README.md`, `CONTEXT.md`, `CONTRIBUTING.md`,
      `SECURITY.md`, `CODE_OF_CONDUCT.md`, `CHANGELOG.md`.
- [x] Architecture sketch (`docs/architecture.md`) and ADR practice
      (`docs/adr/`).
- [ ] `Cargo.toml` workspace at the repo root.
- [ ] Minimal `src/main.rs` that boots the shell skeleton and exits
      cleanly.
- [ ] CI: build, format, clippy, test — all green on a hello-world target.

**Exit criterion:** `cargo build && cargo test` succeeds on a clean clone
on macOS, Linux, and Windows.

## Phase 1 — Shell skeleton

**Goal:** the desktop shell exists, with empty-but-real panes for each
mode.

- [ ] Window, pane manager, theming pipeline.
- [ ] Global keymap and command palette.
- [ ] Pane host with a `Mode` trait — terminal, chat, and editor each
      register a stub.
- [ ] Configuration resolution pass (defaults → user → workspace).
- [ ] Workspace open/close, session storage primitive.

**Exit criterion:** the user can launch OpenSpace, open a folder, switch
between three placeholder mode panes, and see configuration changes apply
on workspace switch.

## Phase 2 — AI core and chat MVP

**Goal:** chat mode can hold a real conversation against at least one
provider.

- [ ] AI core: session model, context window assembly, request lifecycle.
- [ ] Provider adapter trait plus one hosted adapter and one local adapter
      stub (no tool calls yet).
- [ ] Streaming token rendering in chat mode.
- [ ] Session pin, name, and resume.
- [ ] Slash command framework and a small starter set (`/clear`, `/model`,
      `/save`).

**Exit criterion:** a fresh user can install OpenSpace, configure a
provider, and have a coherent multi-turn chat with persisted history.

## Phase 3 — Tooling layer and consent

**Goal:** the assistant can take action on the workspace, gated.

- [ ] Tool definition shape, dispatch, and consent surface.
- [ ] Filesystem read tool (workspace-scoped).
- [ ] Workspace search tool.
- [ ] Consent UI: per-call prompt, session-scoped allow, deny.
- [ ] Tool call logging in the session record.

**Exit criterion:** in chat mode, the assistant can request to read a file
or search the workspace; the user gates each call; the session log is
auditable.

## Phase 4 — Terminal mode

**Goal:** terminal mode is a real interactive shell with assistant support.

- [ ] PTY integration with a stable cross-platform backend.
- [ ] Shell command execution as a gated tool (assistant-initiated runs go
      through consent).
- [ ] Output capture surfaced as context the user can attach to a chat
      turn.
- [ ] Mode-specific commands: explain last failure, suggest next command,
      draft script.

**Exit criterion:** the user can run a real workflow in terminal mode and
ask the assistant to interpret or extend it without leaving the pane.

## Phase 5 — Editor mode

**Goal:** editor mode is a real editor with inline AI assistance.

- [ ] Buffer model, file open/save, undo/redo.
- [ ] Syntax services for at least one target language family.
- [ ] Inline completions and accept/reject UI.
- [ ] Buffer-scoped refactor and explain commands.
- [ ] Diagnostics surfacing into the assistant context.

**Exit criterion:** the user can edit code in editor mode with assistant
help, and a refactor proposed by the assistant lands as a reviewable diff.

## Phase 6 — Memory and cross-mode workflows

**Goal:** workspace memory exists and modes interoperate.

- [ ] Opt-in workspace memory store.
- [ ] Promotion: turn a chat conclusion into durable memory; recall it from
      another mode in the same workspace.
- [ ] Cross-mode commands: send terminal output to chat; open a chat
      reference in the editor.
- [ ] Per-workspace model selection and provider routing.

**Exit criterion:** a user can run a multi-step task that crosses all
three modes and rely on consistent context.

## Phase 7 — Hardening and 0.1

**Goal:** first tagged release.

- [ ] Crash reporting (opt-in, local-first).
- [ ] Performance budget for cold start, first token, and editor input
      latency.
- [ ] Documentation pass: tutorials, troubleshooting, configuration
      reference.
- [ ] Packaging for the supported desktop platforms.
- [ ] Security review of tool consent paths and credential storage.

**Exit criterion:** `0.1.0` tag, a published changelog, and a usable
binary on the three desktop platforms.

## Out of scope for now

These show up in conversations and are deliberately deferred:

- Plugin marketplace and third-party plugin loading.
- Cloud sync of workspaces.
- Mobile or web clients.
- Multi-user collaboration inside a workspace.

When any of these graduates from "later" to "next", it gets its own ADR
and a slot in the roadmap.
