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

## Getting set up

OpenSpace targets a stable Rust toolchain. Once the crate manifest lands:

```sh
rustup show          # confirm an active toolchain
cargo build          # compile
cargo test           # run unit and integration tests
cargo fmt --all      # format
cargo clippy --all-targets --all-features -- -D warnings   # lint
```

Until the manifest is in place, contributions are mostly documentation,
ADRs, and scaffolding. See `docs/roadmap.md` for current priorities.

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
