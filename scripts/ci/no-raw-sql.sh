#!/usr/bin/env bash
set -euo pipefail

# Enforce the persistence boundary: raw SQL and direct SQLite API usage belong only
# in crates/persistence and migrations. Run locally with:
#
#   scripts/ci/no-raw-sql.sh

if ! command -v git >/dev/null 2>&1; then
  echo "error: git is required" >&2
  exit 2
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

pattern='rusqlite::|tokio_rusqlite::|SELECT|INSERT|UPDATE|DELETE FROM|CREATE TABLE'
violations_file="$(mktemp)"
trap 'rm -f "$violations_file"' EXIT

git grep -n -E "$pattern" -- '*.rs' \
  ':(exclude)crates/persistence/**' \
  ':(exclude)migrations/**' \
  >"$violations_file" || true

if [ -s "$violations_file" ]; then
  echo "Raw SQL / SQLite API usage outside crates/persistence or migrations:" >&2
  cat "$violations_file" >&2
  exit 1
fi
