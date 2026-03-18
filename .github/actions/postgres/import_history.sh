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
	git clone --depth 1 --branch _metadata/badges "$REPO_URL" "$TEMP_DIR" >/dev/null 2>&1
	LEDGER_DIR="$TEMP_DIR"
fi

SQL_FILE="$(dirname "$0")/tables.sql"
if [ -f "$SQL_FILE" ]; then
	psql "$DB_TARGET" -f "$SQL_FILE" >/dev/null
fi

echo "✓ Starting historical import into '$DB_TARGET'..."

while IFS= read -r line; do
	COMMIT_HASH=$(echo "$line" | jq -r '.commit_hash')
	RAW_RUN_ID=$(echo "$line" | jq -r '.run_id')

	# Map fields, handling older records that might miss arch/os
	ARCH=$(echo "$line" | jq -r '.arch // empty')
	OS=$(echo "$line" | jq -r '.os // empty')

	# Standardize run_id: if it's not already a 64-char hex string, hash it.
	if [[ ! "$RAW_RUN_ID" =~ ^[0-9a-f]{64}$ ]]; then
		FINAL_RUN_ID=$(echo -n "$RAW_RUN_ID" | sha256sum | awk '{print $1}')
	else
		FINAL_RUN_ID="$RAW_RUN_ID"
	fi

	# 1. Attempt the summary insert
	QUERY=$(echo "$line" | jq -r --arg final_id "$FINAL_RUN_ID" '
    def sql_val(v): if v == null or v == "" then "NULL" else (v | @sh) end;
    "INSERT INTO runs (run_id, run_date, commit_hash, upstream_commit, branch, author_name, actor, provider, arch, os, version_string, features, binary_sha256, passed_count, skipped_count, failed_count)
     VALUES (" +
        ($final_id | @sh) + ", " +
        (.run_date | @sh) + ", " +
        (.commit_hash | @sh) + ", " +
        sql_val(.upstream_commit) + ", " +
        sql_val(.branch) + ", " +
        sql_val(.author_name) + ", " +
        sql_val(.actor) + ", " +
        sql_val(.provider) + ", " +
        sql_val(.arch) + ", " +
        sql_val(.os) + ", " +
        sql_val(.version_string) + ", " +
        sql_val(.features) + ", " +
        sql_val(.binary_sha256) + ", " +
        (.passed_count | tostring) + ", " +
        (.skipped_count | tostring) + ", " +
        (.failed_count | tostring) +
     ")
     ON CONFLICT (run_id, arch, os) DO NOTHING
     RETURNING id;"
  ')

	PK_ID=$(echo "$QUERY" | psql -t -A "$DB_TARGET" 2>/tmp/pg_err || true)

	# 2. If it already exists, fetch the existing PK_ID
	if [ -z "$PK_ID" ]; then
		if [ -s /tmp/pg_err ]; then
			echo "  ✗ ERROR on $FINAL_RUN_ID: $(cat /tmp/pg_err)"
			continue
		fi
		PK_ID=$(psql "$DB_TARGET" -t -A -c "
      SELECT id FROM runs
      WHERE run_id = '${FINAL_RUN_ID}'
      AND arch IS NOT DISTINCT FROM (CASE WHEN '${ARCH}' = '' THEN NULL ELSE '${ARCH}' END)
      AND os IS NOT DISTINCT FROM (CASE WHEN '${OS}' = '' THEN NULL ELSE '${OS}' END);")
	fi

	if [ -z "$PK_ID" ]; then
		echo "  ✗ ERROR: Could not find or create PK for $FINAL_RUN_ID ($ARCH/$OS)"
		continue
	fi

	echo "→ Ingesting details for $FINAL_RUN_ID ($ARCH/$OS)..."

	DETAILS_FILE="$LEDGER_DIR/runs_data/${COMMIT_HASH}.jsonl"
	if [ -f "$DETAILS_FILE" ]; then
		(
			echo "BEGIN;"
			jq -r --arg pk "$PK_ID" '
        "INSERT INTO run_details (run_id, test_name, status) VALUES (" + $pk + ", " + (.Test | @sh) + ", " + (.Action | @sh) + ") ON CONFLICT (run_id, test_name) DO NOTHING;"
      ' "$DETAILS_FILE"
			echo "COMMIT;"
		) | psql "$DB_TARGET" >/dev/null
	else
		echo "  ⌒ No details file found for commit $COMMIT_HASH."
	fi

done <"$LEDGER_DIR/runs.jsonl"

[ -n "$TEMP_DIR" ] && rm -rf "$TEMP_DIR"
echo "✓ Import complete."
