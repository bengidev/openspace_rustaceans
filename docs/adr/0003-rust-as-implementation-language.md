# ADR-0003: Rust as the implementation language

- **Status:** Accepted
- **Date:** 2026-05-21

## Context

OpenSpace is a desktop application that has to satisfy several
constraints simultaneously:

- **Latency budgets.** Editor input, terminal IO, and streaming token
  rendering all sit on the user's hot path. A laggy shell is unusable
  regardless of how good the AI is.
- **Long-lived process.** The shell stays open for an entire work
  session. Memory leaks and runtime stalls are visible.
- **Cross-platform desktop.** macOS, Linux, and Windows are all
  in-scope.
- **Tooling integration.** The shell talks to PTYs, filesystem APIs,
  and provider runtimes — both hosted (network) and local (in-process
  or out-of-process).
- **Single-binary distribution.** No runtime to install on the user's
  machine.

The repository is also already shaped for Rust: `.gitignore` is the Rust
template, the working folder is named after a Rust community, and the
agent playbooks assume a Cargo workflow.

## Decision

OpenSpace is implemented in Rust.

- The repo is structured as a Cargo workspace once `Cargo.toml` lands.
- The shell binary, mode crates, AI core crate, provider adapter crates,
  and tooling crates live as members of that workspace.
- Stable Rust toolchain. Nightly features are not part of the default
  build path; if a specific feature requires nightly, that will be
  proposed as a separate ADR with the trade-offs.
- Default lints: `cargo fmt`, `cargo clippy --all-targets --all-features
  -- -D warnings`.
- Public surface area between workspace crates is documented and
  versioned with the same care as if it were a published crate.

UI runtime, async runtime, and PTY backend are *not* fixed by this ADR.
They will be picked in follow-up ADRs as the shell skeleton lands.

## Consequences

What gets easier:

- Hot paths (editor input, streaming, PTY) get the performance and
  predictability we need without managed-runtime warts.
- Single-binary, native-feeling distribution on all three desktop
  platforms.
- Strong static guarantees around the consent and tool dispatch layers,
  which is exactly where we want bugs to be expensive to write.
- Cross-crate boundaries inside the workspace force the layered
  architecture documented in `docs/architecture.md` to stay honest.

What gets harder:

- Iteration speed on UI-heavy code is lower than in a managed-runtime
  language. We mitigate by keeping mode internals small and isolating
  rendering behind narrow interfaces.
- The contributor pool is smaller than for, say, TypeScript. Onboarding
  documentation has to do more work.
- Some provider runtimes ship reference SDKs in other languages. We
  accept the cost of writing thin Rust adapters or shelling out where
  unavoidable.

What we accept:

- Compile times will be a friction point. Workspace structure and
  feature flags are how we keep them in check.
- Picking Rust commits us to also pick a Rust-friendly UI stack, which
  will narrow options for the desktop shell layer.

## Alternatives considered

- **TypeScript on a web-runtime desktop frame.** Faster UI iteration,
  larger contributor pool, but heavier runtime, harder time hitting
  editor and terminal latency budgets, and weaker guarantees on the
  consent and tool layer.
- **Go.** Good single-binary story, fast compiles, but a weaker desktop
  UI ecosystem and a tougher fit for the buffer/editor data structures
  we will need.
- **Mixed runtime (Rust core, web-runtime UI).** Splits the
  contributor surface and the build pipeline. Worth revisiting if the
  Rust desktop UI ecosystem turns out to be a blocker, but expensive
  to adopt as a default.
