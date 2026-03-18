#!/usr/bin/env bash
set -euo pipefail

# import_history.sh (Simplified Bulk Ingest - Fresh Rebuild)
# Ingests historical JSONL data directly into PostgreSQL with a clean slate.

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

# Ensure tables and views exist (Fresh Schema)
SQL_FILE="$(dirname "$0")/tables.sql"
if [ -f "$SQL_FILE" ]; then
    echo "✓ Applying fresh schema from $SQL_FILE..."
    # We force a drop of the tables to ensure standardized column names are applied
    psql "$DB_TARGET" -c "DROP TABLE IF EXISTS run_details CASCADE; DROP TABLE IF EXISTS runs CASCADE;" > /dev/null
    psql "$DB_TARGET" -f "$SQL_FILE" > /dev/null
fi

echo "✓ Starting bulk historical JSON import into '$DB_TARGET'..."

# 1. Bulk Ingest Run Summaries
echo "→ Ingesting run summaries..."
psql "$DB_TARGET" <<EOF
CREATE TEMP TABLE b (j jsonb);
\copy b FROM '$LEDGER_DIR/runs.jsonl' csv quote e'\x01' delimiter e'\x02';

INSERT INTO runs (run_date, commit_hash, upstream_commit, branch, author_name, actor, provider, arch, os, version_string, features, binary_sha256, n_pass, n_skip, n_fail)
SELECT
    (j->>'run_date')::timestamptz, (j->>'commit_hash'), (j->>'upstream_commit'), (j->>'branch'),
    (j->>'author_name'), (j->>'actor'), (j->>'provider'), (j->>'arch'), (j->>'os'),
    (j->>'version_string'), (j->>'features'), (j->>'binary_sha256'),
    (j->'passed_count')::int, (j->'skipped_count')::int, (j->'failed_count')::int
FROM b
ON CONFLICT (commit_hash, run_date, arch, os) DO NOTHING;
EOF

# 2. Bulk Ingest Test Details
echo "→ Ingesting test details (streaming all files)..."
(
  for f in "$LEDGER_DIR/runs_data"/*.jsonl; do
    [ -f "$f" ] || continue
    cat "$f"
  done
) | psql "$DB_TARGET" <<'EOF'
CREATE TEMP TABLE t (j jsonb);
\copy t FROM STDIN csv quote e'\x01' delimiter e'\x02';

INSERT INTO run_details (run_id, test_name, status)
SELECT r.id, (t.j->>'Test'), (t.j->>'Action')
FROM t
JOIN runs r ON r.commit_hash = (t.j->>'commit')
           AND r.arch IS NOT DISTINCT FROM (t.j->>'arch')
           AND r.os IS NOT DISTINCT FROM (t.j->>'os')
ON CONFLICT (run_id, test_name) DO NOTHING;
EOF

[ -n "$TEMP_DIR" ] && rm -rf "$TEMP_DIR"
echo "✓ Import complete."
