#!/usr/bin/env bash
set -euo pipefail

# import_history.sh (Simplified Bulk Ingest - Fresh Rebuild)
# Ingests historical JSONL data directly into PostgreSQL with a clean slate.

LEDGER_DIR=${1:-}
TEMP_DIR=""
DB_TARGET=${DATABASE_URL:-c10y}

if [ -z "$LEDGER_DIR" ]; then
  TEMP_DIR=$(mktemp -d -t c10y-import-XXXXXX)
  # The _metadata/badges branch is on GitHub
  REPO_URL="https://github.com/gamesguru/continuwuity.git"
  echo "✓ No ledger directory provided. Cloning _metadata/badges branch from $REPO_URL..."
  if ! git clone --depth 1 --branch _metadata/badges "$REPO_URL" "$TEMP_DIR" > /dev/null 2>&1; then
    echo "❌ Failed to clone _metadata/badges from $REPO_URL"
    exit 1
  fi
  LEDGER_DIR="$TEMP_DIR"
fi

# Ensure tables and views exist (Fresh Schema)
SQL_FILE="$(dirname "$0")/tables.sql"
if [ -f "$SQL_FILE" ]; then
    echo "✓ Applying fresh schema from $SQL_FILE..."
    psql "$DB_TARGET" -c "DROP TABLE IF EXISTS run_details CASCADE; DROP TABLE IF EXISTS runs CASCADE; DROP TABLE IF EXISTS master_baseline CASCADE;" > /dev/null
    psql "$DB_TARGET" -f "$SQL_FILE" > /dev/null
fi

echo "✓ Starting bulk historical JSON import into '$DB_TARGET'..."

# 1. Bulk Ingest Run Summaries
echo "→ Ingesting run summaries..."
psql "$DB_TARGET" <<EOF
CREATE TEMP TABLE b (j jsonb);
\copy b FROM '$LEDGER_DIR/runs.jsonl' csv quote e'\x01' delimiter e'\x02';

INSERT INTO runs (run_date, commit_hash, upstream_commit, branch, author_name, actor, provider, arch, os, version_string, features, profile, binary_sha256, n_pass, n_skip, n_fail)
SELECT
    (j->>'run_date')::timestamptz, (j->>'commit_hash'), (j->>'upstream_commit'), (j->>'branch'),
    (j->>'author_name'), (j->>'actor'), (j->>'provider'), NULLIF(j->>'arch', ''), NULLIF(j->>'os', ''),
    (j->>'version_string'), (j->>'features'), (j->>'profile'), (j->>'binary_sha256'),
    (j->'passed_count')::int, (j->'skipped_count')::int, (j->'failed_count')::int
FROM b
ON CONFLICT (commit_hash, run_date, arch, os) DO NOTHING;
EOF

# 2. Bulk Ingest Test Details (Injecting metadata from filenames)
echo "→ Consolidating and ingesting test details..."
(
  echo "CREATE TEMP TABLE t (j jsonb);"
  echo "\copy t FROM STDIN csv quote e'\x01' delimiter e'\x02';"
  for f in "$LEDGER_DIR/runs_data"/*.jsonl; do
    [ -f "$f" ] || continue
    BASENAME=$(basename "$f" .jsonl)
    if [[ "$BASENAME" == *-* ]]; then
      # Format: COMMIT-ARCH-OS-PROFILE
      COMMIT=$(echo "$BASENAME" | cut -d'-' -f1)
      ARCH=$(echo "$BASENAME" | cut -d'-' -f2)
      OS=$(echo "$BASENAME" | cut -d'-' -f3)
      PROFILE=$(echo "$BASENAME" | cut -d'-' -f4-)
    else
      COMMIT="$BASENAME"
      ARCH=""
      OS=""
      PROFILE=""
    fi
    jq -c --arg h "$COMMIT" --arg a "$ARCH" --arg o "$OS" --arg p "$PROFILE" \
       '. + {commit: (if .commit then .commit else $h end),
             arch: (if .arch then .arch else $a end),
             os: (if .os then .os else $o end),
             profile: (if .profile then .profile else $p end)}' "$f"
  done
  echo "\."
  echo "INSERT INTO run_details (run_id, test_name, status)
        SELECT DISTINCT ON ((t.j->>'Test')) r.id, (t.j->>'Test'), (t.j->>'Action')
        FROM t
        JOIN runs r ON r.commit_hash = (t.j->>'commit')
                   AND r.arch IS NOT DISTINCT FROM (NULLIF((t.j->>'arch'),''))
                   AND r.os IS NOT DISTINCT FROM (NULLIF((t.j->>'os'),''))
                   AND r.profile IS NOT DISTINCT FROM (NULLIF((t.j->>'profile'),''))
        ORDER BY (t.j->>'Test'), (t.j->>'Time') DESC
        ON CONFLICT (run_id, test_name) DO NOTHING;"
) | psql "$DB_TARGET"
[ -n "$TEMP_DIR" ] && rm -rf "$TEMP_DIR"
echo "✓ Bulk import complete."
