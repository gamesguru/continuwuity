#!/usr/bin/env bash
set -euo pipefail

# import_history.sh
# Ingests historical data from the orphan results branch into PostgreSQL.

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
if [ -f "$SQL_FILE" ]; then
    psql "$DB_TARGET" -f "$SQL_FILE" > /dev/null
fi

echo "✓ Starting historical import into '$DB_TARGET'..."

while IFS= read -r line; do
  COMMIT_HASH=$(echo "$line" | jq -r '.commit_hash')
  RAW_RUN_ID=$(echo "$line" | jq -r '.run_id')
  ARCH=$(echo "$line" | jq -r '.arch // empty')
  OS=$(echo "$line" | jq -r '.os // empty')

  if [[ ! "$RAW_RUN_ID" =~ ^[0-9a-f]{64}$ ]]; then
    FINAL_RUN_ID=$(echo -n "$RAW_RUN_ID" | sha256sum | awk '{print $1}')
  else
    FINAL_RUN_ID="$RAW_RUN_ID"
  fi

  # 1. Attempt the summary insert
  # We use a helper to double up single quotes for PG and wrap in single quotes.
  # Using double quotes for the outer jq string makes handling internal single quotes easier.
  QUERY=$(echo "$line" | jq -r --arg final_id "$FINAL_RUN_ID" '
    def sql_esc(v): if v == null or v == "" then "NULL" else ("'\''" + (v | tostring | gsub("'\''"; "'\'''\''")) + "'\''") end;
    "INSERT INTO runs (run_id, run_date, commit_hash, upstream_commit, branch, author_name, actor, provider, arch, os, version_string, features, binary_sha256, passed_count, skipped_count, failed_count)
     VALUES (" +
        sql_esc($final_id) + ", " +
        sql_esc(.run_date) + ", " +
        sql_esc(.commit_hash) + ", " +
        sql_esc(.upstream_commit) + ", " +
        sql_esc(.branch) + ", " +
        sql_esc(.author_name) + ", " +
        sql_esc(.actor) + ", " +
        sql_esc(.provider) + ", " +
        sql_esc(.arch) + ", " +
        sql_esc(.os) + ", " +
        sql_esc(.version_string) + ", " +
        sql_esc(.features) + ", " +
        sql_esc(.binary_sha256) + ", " +
        (.passed_count | tostring) + ", " +
        (.skipped_count | tostring) + ", " +
        (.failed_count | tostring) +
     ")
     ON CONFLICT (run_id, arch, os) DO NOTHING
     RETURNING id;"
  ')

  # Run the query and capture only the ID
  PK_ID=$(echo "$QUERY" | psql -t -A "$DB_TARGET" 2>/tmp/pg_err | head -n 1 || true)

  if [ -z "$PK_ID" ] || [[ ! "$PK_ID" =~ ^[0-9]+$ ]]; then
    if [ -s /tmp/pg_err ]; then
       echo "  ✗ ERROR on $FINAL_RUN_ID: $(cat /tmp/pg_err)"
       continue
    fi
    # If PK_ID is empty but no error, it likely already existed. Fetch it.
    PK_ID=$(psql "$DB_TARGET" -t -A -c "
      SELECT id FROM runs
      WHERE run_id = '${FINAL_RUN_ID}'
      AND arch IS NOT DISTINCT FROM (CASE WHEN '${ARCH}' = '' THEN NULL ELSE '${ARCH}' END)
      AND os IS NOT DISTINCT FROM (CASE WHEN '${OS}' = '' THEN NULL ELSE '${OS}' END);" | head -n 1)
  fi

  if [ -z "$PK_ID" ] || [[ ! "$PK_ID" =~ ^[0-9]+$ ]]; then
    echo "  ✗ ERROR: Could not find or create numeric PK for $FINAL_RUN_ID ($ARCH/$OS). Got: '$PK_ID'"
    continue
  fi

  echo "→ Ingesting details for $FINAL_RUN_ID ($ARCH/$OS)..."

  DETAILS_FILE="$LEDGER_DIR/runs_data/${COMMIT_HASH}.jsonl"
  if [ -f "$DETAILS_FILE" ]; then
    (
      echo "BEGIN;"
      jq -r --arg pk "$PK_ID" '
        def sql_esc(v): "'\''" + (v | tostring | gsub("'\''"; "'\'''\''")) + "'\''";
        "INSERT INTO run_details (run_id, test_name, status) VALUES (" + $pk + ", " + sql_esc(.Test) + ", " + sql_esc(.Action) + ") ON CONFLICT (run_id, test_name) DO NOTHING;"
      ' "$DETAILS_FILE"
      echo "COMMIT;"
    ) | psql "$DB_TARGET" > /dev/null
  else
    echo "  ⌒ No details file found for commit $COMMIT_HASH."
  fi

done < "$LEDGER_DIR/runs.jsonl"

[ -n "$TEMP_DIR" ] && rm -rf "$TEMP_DIR"
echo "✓ Import complete."
