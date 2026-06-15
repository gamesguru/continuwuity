#!/usr/bin/env bash
set -euo pipefail

# sync_recent.sh
# Performs an incremental, efficient sync of the last N runs into PostgreSQL.
# Optimized to only stream the specific run data needed over the wire.

LIMIT=${1:-100}
TARGET_BRANCH=${TARGET_BRANCH:-"_metadata/badges"}
export DB_TARGET=${DATABASE_URL:-"c10y"}
SSH_TARGET=${SSH_TARGET:-"git@nutra.tk"}

echo "→ Fetching latest metadata from origin/$TARGET_BRANCH..."
# Treeless fetch of the metadata branch
git fetch origin "$TARGET_BRANCH" --depth 1 --filter=blob:none >/dev/null 2>&1

# 1. Ingest Recent Summaries
echo "→ Streaming last $LIMIT run summaries..."
(
	echo "CREATE TEMP TABLE b (j jsonb);"
	printf '%s\n' "\copy b FROM STDIN csv quote e'\x01' delimiter e'\x02';"
	git show "FETCH_HEAD:runs.jsonl" | tail -n "$LIMIT"
	echo "\."
	echo "INSERT INTO runs (run_date, commit_hash, upstream_commit, branch, author_name, actor, provider, arch, os, version_string, features, profile, binary_sha256, n_pass, n_skip, n_fail, room_version)
        SELECT
          (j->>'run_date')::timestamptz, (j->>'commit_hash'), (j->>'upstream_commit'), (j->>'branch'),
          (j->>'author_name'), (j->>'actor'), (j->>'provider'), NULLIF(j->>'arch', ''), NULLIF(j->>'os', ''),
          (j->>'version_string'), (j->>'features'), (j->>'profile'), (j->>'binary_sha256'),
          (j->'passed_count')::int, (j->'skipped_count')::int, (j->'failed_count')::int, (j->>'room_version')
        FROM b ON CONFLICT (commit_hash, run_date, arch, os, profile, room_version) DO NOTHING;"
) | ssh -C -o StrictHostKeyChecking=no -o ServerAliveInterval=30 "$SSH_TARGET" "psql -U git c10y"

# 2. Ingest Recent Test Details
echo "→ Streaming last $LIMIT run details (incremental files)..."
(
	echo "CREATE TEMP TABLE t (j jsonb);"
	printf '%s\n' "\copy t FROM STDIN csv quote e'\x01' delimiter e'\x02';"

	# Pre-cache the file list from the tree for fast existence checks
	ALL_FILES=$(git ls-tree -r FETCH_HEAD:runs_data --name-only || true)

	# Iterate over the last LIMIT runs from runs.jsonl
	# We extract metadata to find the exact file and to re-inject for robustness.
	git show "FETCH_HEAD:runs.jsonl" | tail -n "$LIMIT" | while read -r run_json; do
		COMMIT=$(echo "$run_json" | jq -r '.commit_hash')
		ARCH=$(echo "$run_json" | jq -r '.arch')
		OS=$(echo "$run_json" | jq -r '.os')
		PROFILE=$(echo "$run_json" | jq -r '.profile // ""')
		ROOM_VERSION=$(echo "$run_json" | jq -r '.room_version // ""')

		SAFE_ARCH=${ARCH//[!a-zA-Z0-9._-]/_}
		SAFE_OS=${OS//[!a-zA-Z0-9._-]/_}
		SAFE_PROFILE=${PROFILE//[!a-zA-Z0-9._-]/_}
		SAFE_ROOM_VERSION=${ROOM_VERSION//[!a-zA-Z0-9._-]/_}

		BASENAME="${COMMIT}-${SAFE_ARCH}-${SAFE_OS}-${SAFE_PROFILE}-${SAFE_ROOM_VERSION}.jsonl"
		FILENAME="runs_data/${BASENAME}"

		# Check if file exists in the pre-cached list
		if grep -Fqx "$BASENAME" <<<"$ALL_FILES" >/dev/null; then
			# Inject/Overwrite metadata from the summary record for robustness
			git show "FETCH_HEAD:$FILENAME" |
				jq -c --arg c "$COMMIT" --arg a "$ARCH" --arg o "$OS" --arg p "$PROFILE" --arg rv "$ROOM_VERSION" \
					'. + {commit: $c, arch: $a, os: $o, profile: $p, room_version: $rv}'
		fi
	done

	echo "\."
	echo "INSERT INTO run_details (run_id, test_name, status)
        SELECT r.id, (t.j->>'Test'), (t.j->>'Action')
        FROM t JOIN runs r ON r.commit_hash = (t.j->>'commit')
          AND r.arch IS NOT DISTINCT FROM (NULLIF((t.j->>'arch'), ''))
          AND r.os IS NOT DISTINCT FROM (NULLIF((t.j->>'os'), ''))
          AND r.profile IS NOT DISTINCT FROM (NULLIF((t.j->>'profile'), ''))
          AND r.room_version IS NOT DISTINCT FROM (NULLIF((t.j->>'room_version'), ''))
        WHERE (t.j->>'Action') IN ('pass', 'fail', 'skip')
        ON CONFLICT (run_id, test_name) DO UPDATE SET status = EXCLUDED.status;

        INSERT INTO ever_passed (test_name, rv, last_passed)
        SELECT rd.test_name, COALESCE(r.room_version, '11'), MAX(r.run_date)::date::text
        FROM run_details rd
        JOIN runs r ON rd.run_id = r.id
        WHERE rd.status = 'pass'
        GROUP BY rd.test_name, COALESCE(r.room_version, '11')
        ON CONFLICT (test_name, rv) DO UPDATE
        SET last_passed = GREATEST(ever_passed.last_passed, EXCLUDED.last_passed);
"
) | ssh -C -o StrictHostKeyChecking=no -o ServerAliveInterval=30 "$SSH_TARGET" "psql -U git c10y"

echo "✓ Incremental sync of last $LIMIT runs complete."
