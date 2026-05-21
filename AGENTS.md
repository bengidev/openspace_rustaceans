# AGENTS.md

Guidance for AI coding agents working in this repo.

## Agent skills

### Issue tracker

Issues live as GitHub issues at `bengidev/openspace_rustaceans`. Use the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Canonical five-role label vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout — `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Naming third-party references

Do not name other products, vendors, or peer projects in any artefact you
commit on behalf of the maintainer — source, comments, docs, ADRs, commit
messages, PR titles and bodies, issue titles and bodies. This applies even
when the user names a reference in their prompt as inspiration.

Use an abstract alias that describes the role, not the brand:

- "best-in-class peer editor" instead of a named product
- "hosted provider adapter" / "local engine adapter" instead of a named vendor
- "industry-standard pattern" instead of a named framework

If you cannot find an alias that carries the meaning, drop the comparison
and describe the behaviour directly. See `CONTRIBUTING.md` ("Naming
third-party references") for the full rule and examples.
