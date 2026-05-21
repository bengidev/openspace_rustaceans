# Security Policy

## Supported versions

OpenSpace is in pre-release development. Until the first tagged release,
only the current `main` branch receives security attention. Once tagged
releases exist, this section will list which minor versions are eligible
for security updates.

## Reporting a vulnerability

Please do **not** open a public GitHub issue for a security report.

Instead, contact the maintainer privately through one of:

- A GitHub private vulnerability report on
  `bengidev/openspace_rustaceans` (preferred).
- Direct email to the maintainer listed on the repository profile.

Include in the report:

- A description of the issue and its potential impact.
- Steps to reproduce, ideally with a minimal proof of concept.
- Any suggested mitigation if you have one.
- Whether you would like to be credited in the eventual advisory.

## What to expect

- Acknowledgement within a few business days.
- A triage assessment (severity, scope, affected versions) shortly after.
- Coordinated disclosure: a fix lands first, an advisory and credit follow.

## Scope

In scope:

- Code and configuration in this repository.
- Default behaviour of the desktop shell, mode subsystems, AI core, and
  provider adapter implementations shipped from this repo.

Out of scope:

- Vulnerabilities in third-party model providers or local engines reached
  through provider adapters. Report those to the upstream vendor.
- Issues that require an attacker who already has full local code execution
  on the user's machine.

## Hardening priorities

Areas that receive heightened review:

- Tool execution and consent surfaces (anything the assistant can run on
  behalf of the user).
- Filesystem access scoping (workspace boundaries, path traversal).
- Credential storage for provider adapters.
- Update and plugin loading paths once they exist.

If you are unsure whether something qualifies, send the report anyway. It is
better to receive a non-issue than to miss a real one.
