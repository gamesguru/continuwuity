#!/usr/bin/env bash
set -euo pipefail

# sync_recent.sh
# Performs an incremental, efficient sync of the last N runs into PostgreSQL.
# Optimized to only stream the specific run data needed over the wire.

LIMIT=${1:-100}
TARGET_BRANCH=${TARGET_BRANCH:-"_metadata/badges"}
DB_TARGET=${DATABASE_URL:-"c10y"}
SSH_TARGET=${SSH_TARGET:-"git@nutra.tk"}

echo "→ Fetching latest metadata from origin/$TARGET_BRANCH..."
# Treeless fetch of the metadata branch
git fetch origin "$TARGET_BRANCH" --depth 1 --filter=blob:none > /dev/null 2>&1

# 1. Ingest Recent Summaries
echo "→ Streaming last $LIMIT run summaries..."
(
  echo "CREATE TEMP TABLE b (j jsonb);"
  echo "\copy b FROM STDIN csv quote e'\x01' delimiter e'\x02';"
  git show "FETCH_HEAD:runs.jsonl" | tail -n "$LIMIT"
  echo "\."
  echo "INSERT INTO runs (run_date, commit_hash, upstream_commit, branch, author_name, actor, provider, arch, os, version_string, features, profile, binary_sha256, n_pass, n_skip, n_fail)
        SELECT
          (j->>'run_date')::timestamptz, (j->>'commit_hash'), (j->>'upstream_commit'), (j->>'branch'),
          (j->>'author_name'), (j->>'actor'), (j->>'provider'), NULLIF(j->>'arch', ''), NULLIF(j->>'os', ''),
          (j->>'version_string'), (j->>'features'), (j->>'profile'), (j->>'binary_sha256'),
          (j->'passed_count')::int, (j->'skipped_count')::int, (j->'failed_count')::int
        FROM b ON CONFLICT (commit_hash, run_date, arch, os) DO NOTHING;"
) | ssh -C -o StrictHostKeyChecking=no "$SSH_TARGET" "psql -U git c10y"

# 2. Ingest Recent Test Details
echo "→ Streaming last $LIMIT run details (incremental files)..."
(
  echo "CREATE TEMP TABLE t (j jsonb);"
  echo "\copy t FROM STDIN csv quote e'\x01' delimiter e'\x02';"

  # Pre-cache the file list from the tree for fast existence checks
  ALL_FILES=$(git ls-tree -r FETCH_HEAD:runs_data --name-only || true)

  # Iterate over the last LIMIT runs from runs.jsonl
  # We extract metadata to find the exact file and to re-inject for robustness.
  git show "FETCH_HEAD:runs.jsonl" | tail -n "$LIMIT" | while read -r run_json; do
      COMMIT=$(echo "$run_json" | jq -r '.commit_hash')
      ARCH=$(echo "$run_json" | jq -r '.arch')
      OS=$(echo "$run_json" | jq -r '.os')
      PROFILE=$(echo "$run_json" | jq -r '.profile // ""')

      SAFE_ARCH=$(echo "$ARCH" | sed 's/[^a-zA-Z0-9._-]/_/g')
      SAFE_OS=$(echo "$OS" | sed 's/[^a-zA-Z0-9._-]/_/g')
      SAFE_PROFILE=$(echo "$PROFILE" | sed 's/[^a-zA-Z0-9._-]/_/g')

      BASENAME="${COMMIT}-${SAFE_ARCH}-${SAFE_OS}-${SAFE_PROFILE}.jsonl"
      FILENAME="runs_data/${BASENAME}"

      # Check if file exists in the pre-cached list
      if grep -Fqx "$BASENAME" <<< "$ALL_FILES" > /dev/null; then
          # Inject/Overwrite metadata from the summary record for robustness
          git show "FETCH_HEAD:$FILENAME" | \
            jq -c --arg c "$COMMIT" --arg a "$ARCH" --arg o "$OS" --arg p "$PROFILE" \
            '. + {commit: $c, arch: $a, os: $o, profile: $p}'
      fi
  done

  echo "\."
  echo "INSERT INTO run_details (run_id, test_name, status)
        SELECT r.id, (t.j->>'Test'), (t.j->>'Action')
        FROM t JOIN runs r ON r.commit_hash = (t.j->>'commit')
          AND r.arch IS NOT DISTINCT FROM (NULLIF((t.j->>'arch'), ''))
          AND r.os IS NOT DISTINCT FROM (NULLIF((t.j->>'os'), ''))
          AND r.profile IS NOT DISTINCT FROM (NULLIF((t.j->>'profile'), ''))
        ON CONFLICT (run_id, test_name) DO NOTHING;"
) | ssh -C -o StrictHostKeyChecking=no "$SSH_TARGET" "psql -U git c10y"

echo "✓ Incremental sync of last $LIMIT runs complete."
