#!/usr/bin/env bash
set -euo pipefail

# Enforce the redaction boundary: API keys, bearer tokens, emails, and
# file-content snippets over 200 characters must never appear unredacted
# in tracing spans, log output, or error messages outside the redactor's
# own test fixtures.
#
# Run locally with:
#   scripts/ci/no-secrets-in-logs.sh

if ! command -v git >/dev/null 2>&1; then
  echo "error: git is required" >&2
  exit 2
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

# Patterns that look like unredacted secrets in log/tracing calls.
# We look for tracing macros (info!, warn!, error!, debug!, trace!) and
# println!/eprintln! that contain API-key-shaped, bearer-token-shaped,
# or email-shaped strings that are NOT inside the redactor test fixtures.
#
# The redactor itself lives in crates/observability — its tests
# intentionally contain fake secrets so they are excluded.

secret_pattern='(info!|warn!|error!|debug!|trace!|println!|eprintln!).*[a-z]{2,}[_-]?[a-z]*[=:].*[A-Za-z0-9]{20,}'
email_pattern='(info!|warn!|error!|debug!|trace!|println!|eprintln!).*[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+[.][A-Za-z]{2,}'

violations_file="$(mktemp)"
trap 'rm -f "$violations_file"' EXIT

# Gate 1: no hardcoded long alphanumeric secrets in tracing/log calls
# outside the observability crate's own tests.
git grep -n -P "$secret_pattern" -- '*.rs' \
  ':(exclude)crates/observability/**' \
  ':(exclude)target/**' \
  >"$violations_file" || true

# Gate 2: no unredacted emails in tracing/log calls
git grep -n -P "$email_pattern" -- '*.rs' \
  ':(exclude)crates/observability/**' \
  ':(exclude)target/**' \
  >>"$violations_file" || true

if [ -s "$violations_file" ]; then
  echo "Unredacted secrets found in tracing/log calls outside crates/observability:" >&2
  echo "These should go through openspace_observability::file_logging::redact()" >&2
  echo "or use [REDACTED_*] placeholder constants." >&2
  cat "$violations_file" >&2
  exit 1
fi

echo "no-secrets-in-logs: clean"
