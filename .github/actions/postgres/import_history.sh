#!/usr/bin/env bash
set -euo pipefail

# import_history.sh (Optimized Native JSON Ingest)
# Ingests historical JSONL data directly into PostgreSQL using jsonb.

LEDGER_DIR=${1:-""}
TEMP_DIR=""
DB_TARGET=${DATABASE_URL:-"c10y"}

if [ -z "$LEDGER_DIR" ]; then
  TEMP_DIR=$(mktemp -d -t c10y-import-XXXXXX)
  echo "✓ No ledger directory provided. Cloning _metadata/badges branch..."
  REPO_URL=$(git remote get-url origin 2>/dev/null || echo "")
  git clone --depth 1 --branch _metadata/badges "$REPO_URL" "$TEMP_DIR" > /dev/null 2>&1
  LEDGER_DIR="$TEMP_DIR"
fi

SQL_FILE="$(dirname "$0")/tables.sql"
[ -f "$SQL_FILE" ] && psql "$DB_TARGET" -f "$SQL_FILE" > /dev/null

echo "✓ Starting bulk historical JSON import into '$DB_TARGET'..."

# 1. Bulk Ingest Summaries via JSONB
echo "→ Ingesting run summaries..."
psql "$DB_TARGET" <<EOF
CREATE TEMP TABLE tmp_runs_json (j jsonb);
\copy tmp_runs_json FROM '$LEDGER_DIR/runs.jsonl' csv quote e'\x01' delimiter e'\x02';

INSERT INTO runs (run_id, run_date, commit_hash, upstream_commit, branch, author_name, actor, provider, arch, os, version_string, features, binary_sha256, passed_count, skipped_count, failed_count)
SELECT
    CASE WHEN (j->>'run_id') ~ '^[0-9a-f]{64}$' THEN (j->>'run_id')
         ELSE encode(sha256((j->>'run_id')::bytea), 'hex')
    END,
    (j->>'run_date')::timestamptz,
    (j->>'commit_hash'),
    (j->>'upstream_commit'),
    (j->>'branch'),
    (j->>'author_name'),
    (j->>'actor'),
    (j->>'provider'),
    (j->>'arch'),
    (j->>'os'),
    (j->>'version_string'),
    (j->>'features'),
    (j->>'binary_sha256'),
    (j->'passed_count')::int,
    (j->'skipped_count')::int,
    (j->'failed_count')::int
FROM tmp_runs_json
ON CONFLICT (run_id, arch, os) DO NOTHING;
EOF

# 2. Bulk Ingest Details via JSONB
echo "→ Ingesting test details (streaming)..."
psql "$DB_TARGET" <<EOF
CREATE TEMP TABLE tmp_details_json (j jsonb);
\copy tmp_details_json FROM STDIN csv quote e'\x01' delimiter e'\x02';
$(
  for f in "$LEDGER_DIR/runs_data"/*.jsonl; do
    [ -f "$f" ] || continue
    COMMIT=$(basename "$f" .jsonl)
    jq -c --arg h "$COMMIT" '. + {commit: $h}' "$f"
  done
)
\.

INSERT INTO run_details (run_id, test_name, status)
SELECT r.id, (d.j->>'Test'), (d.j->>'Action')
FROM tmp_details_json d
JOIN runs r ON r.commit_hash = (d.j->>'commit')
ON CONFLICT (run_id, test_name) DO NOTHING;
EOF

[ -n "$TEMP_DIR" ] && rm -rf "$TEMP_DIR"
echo "✓ Bulk import complete."
