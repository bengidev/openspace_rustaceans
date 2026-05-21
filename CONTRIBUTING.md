# Contributing to OpenSpace

Thanks for considering a contribution. This document describes the workflow,
conventions, and quality expectations for changes to this repository.

## Ground rules

- Be specific. Issues and PRs that name a concrete behaviour, file, or
  command land faster than ones that describe a vibe.
- Read `CONTEXT.md` before naming things. The glossary is small on purpose.
- If a change contradicts an existing ADR, surface that explicitly in the
  PR description rather than silently overriding the decision.
- Follow the `CODE_OF_CONDUCT.md`.

## Naming third-party references

Do **not** name other products, vendors, or peer projects in committed
artefacts. This applies to:

- Source code and inline comments.
- Documentation under `docs/` and at the repo root.
- Commit messages, PR titles, and PR descriptions.
- Issue titles and bodies, including triage notes.
- ADRs in `docs/adr/`.

Use an **abstract alias** instead. The alias should describe the *role*
the reference plays, not the brand. Examples:

| Don't write                           | Write                                |
| ------------------------------------- | ------------------------------------ |
| "like &lt;chat product X&gt;"               | "like a best-in-class peer editor"   |
| "the &lt;model vendor Y&gt; API"            | "a hosted provider adapter"          |
| "&lt;local engine Z&gt; ships with…"        | "a local engine adapter ships with…" |
| "inspired by &lt;framework W&gt;"           | "an industry-standard pattern"       |
| "we want parity with &lt;tool V&gt;"        | "we want parity with peer tools"     |

If you cannot find an alias that carries the meaning, drop the comparison
and describe the behaviour directly. The reader does not need the brand to
understand the requirement.

This rule holds **even when an issue, design note, or chat thread named the
reference explicitly**. Inspirations and references can sit in private
notes, not in the repository's public history.

The same constraint applies to AI coding agents working in this repo — see
`AGENTS.md`.

## Getting set up

OpenSpace targets a stable Rust toolchain pinned in `rust-toolchain.toml`
at the repo root, so a fresh `rustup` will pick the right channel
automatically. From a clean checkout:

```sh
rustup show          # confirm the pinned toolchain is active
cargo build --workspace                                    # compile
cargo test --workspace                                     # run unit and integration tests
cargo fmt --all                                            # format
cargo clippy --workspace --all-targets -- -D warnings      # lint
```

These are the same commands the CI matrix runs on macOS, Ubuntu, and
Windows — keeping local and CI in lockstep avoids "works on my machine"
drift. See `docs/roadmap.md` for current priorities.

## Branching

- `main` is the integration branch. It must always build.
- Work happens on topic branches. Suggested naming:
  - `feat/<short-description>` for new capabilities
  - `fix/<short-description>` for bug fixes
  - `docs/<short-description>` for documentation-only changes
  - `chore/<short-description>` for tooling, formatting, infra
  - `refactor/<short-description>` for non-behavioural cleanups
- Push topic branches to your fork or to a non-`main` ref on the upstream
  repo. Do not push to `main` directly.

## Commits

Commits should be **granular** — one logical concern per commit. A commit
that mixes a bug fix with unrelated formatting is harder to review and
harder to revert.

The project uses [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<optional scope>): <imperative summary>

<optional body>

<optional footer>
```

Common types:

| Type      | Use for                                            |
| --------- | -------------------------------------------------- |
| `feat`    | a new user-visible capability                      |
| `fix`     | a bug fix                                          |
| `docs`    | documentation-only changes                         |
| `refactor`| internal restructuring with no behaviour change    |
| `perf`    | performance improvements                           |
| `test`    | tests added or adjusted                            |
| `build`   | build system, dependencies, packaging              |
| `ci`      | CI configuration                                   |
| `chore`   | repo housekeeping with no production impact        |

Examples:

```
feat(chat): add slash-command palette
fix(terminal): preserve cursor position after AI rewrite
docs(adr): record decision on provider adapter interface
```

## Pull requests

1. Open a draft PR early if the change is non-trivial. It is easier to
   redirect a draft than to rewrite a finished one.
2. Reference the issue the PR closes (`Closes #N`) when applicable.
3. PR description should answer:
   - **What** changed and **why**.
   - **How it was tested** (commands run, manual verification, screenshots
     if a surface changed).
   - **Anything blocked or follow-ups** that intentionally were not done.
4. Keep PRs focused. If review uncovers a separate concern, file it as a new
   issue rather than expanding the PR.
5. Resolve all CI failures before requesting review.

## Issues

Issues live as GitHub issues at `bengidev/openspace_rustaceans`. See
`docs/agents/issue-tracker.md` for the operating commands and
`docs/agents/triage-labels.md` for the canonical label vocabulary.

When filing a new issue:

- Title is a short imperative or a concrete observation
  (`terminal mode loses focus on resize`).
- Body answers: what happened, what was expected, how to reproduce,
  environment.
- Apply `needs-triage` and let a maintainer route it.

## Documentation expectations

- New user-visible behaviour gets a note in `CHANGELOG.md` under
  `[Unreleased]`.
- New domain terms go in `CONTEXT.md`.
- Decisions with long-lived consequences get an ADR in `docs/adr/`.
- Cross-cutting changes update `docs/architecture.md` if the diagram or
  layering shifts.

## Testing expectations

- Unit tests live next to the code they cover.
- Integration tests covering cross-mode behaviour live under `tests/`.
- A bug fix should land with a regression test that fails before the fix
  and passes after.
- Performance-sensitive paths should include a benchmark when feasible.

## Reviewing

- Be specific in review comments. Quote the exact line; suggest a concrete
  alternative.
- Distinguish blocking comments (`request changes`) from preferences
  (`nit:`). Reviewers should not block on style if a linter would catch it.
- Approving a PR means you would be comfortable owning the change.

Thanks for helping push the project forward.
