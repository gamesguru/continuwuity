#!/usr/bin/env bash
set -euo pipefail

# sync_recent.sh
# Performs an incremental, efficient sync of the last N runs into PostgreSQL.
# Only streams run detail files that are not yet in the database.

LIMIT=${1:-100}
TARGET_BRANCH=${TARGET_BRANCH:-"_metadata/badges"}
export DB_TARGET=${DATABASE_URL:-"c10y"}
SSH_TARGET=${SSH_TARGET:-"git@nutra.tk"}

psql_remote() {
	ssh -C -o StrictHostKeyChecking=no -o ServerAliveInterval=30 "$SSH_TARGET" "psql -U git c10y"
}

echo "→ Fetching latest metadata from origin/$TARGET_BRANCH..."
git fetch origin "$TARGET_BRANCH" --depth 1 --filter=blob:none >/dev/null 2>&1

# Ingest Recent Summaries (fast — ON CONFLICT DO NOTHING skips existing)
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
        FROM b ON CONFLICT (commit_hash, run_date, arch, os, profile, room_version, features) DO NOTHING;"
) | psql_remote

# Find which runs already have details (to skip re-ingesting)
echo "→ Checking which runs already have details..."
EXISTING_KEYS=$(
	ssh -o StrictHostKeyChecking=no -o ServerAliveInterval=30 "$SSH_TARGET" "psql -U git c10y -t -A" <<'SQL'
SELECT r.commit_hash || '|' || COALESCE(r.arch,'') || '|' || COALESCE(r.os,'') || '|' || COALESCE(r.profile,'') || '|' || COALESCE(r.room_version,'') || '|' || COALESCE(regexp_replace(btrim(r.features, ' ,'), '[,\s]+', ' ', 'g'), '')
FROM runs r
WHERE r.id >= (SELECT GREATEST(MAX(id) - 200, 0) FROM runs)
  AND EXISTS (SELECT 1 FROM run_details rd WHERE rd.run_id = r.id LIMIT 1);
SQL
)

declare -A HAS_DETAILS=()
while IFS= read -r key; do
	[[ -n "$key" ]] && HAS_DETAILS["$key"]=1
done <<<"$EXISTING_KEYS"

# Pre-cache git tree for fast existence checks
ALL_FILES=$(git ls-tree -r FETCH_HEAD:runs_data --name-only || true)

# Build manifest with single jq pass (instead of 5× jq calls per line)
TMPMANIFEST=$(mktemp)
trap 'rm -f "$TMPMANIFEST"' EXIT
git show "FETCH_HEAD:runs.jsonl" | tail -n "$LIMIT" |
	jq -r '[.commit_hash, (.arch // ""), (.os // ""), (.profile // ""), (.room_version // ""), ((.features // "") | gsub("[,\\s]+"; " ") | gsub("^ | $"; ""))] | @tsv' \
		>"$TMPMANIFEST"

NEED=0
SKIP=0
declare -a PENDING_FILES=()
declare -a PENDING_META=()

while IFS=$'\t' read -r COMMIT ARCH OS PROFILE ROOM_VERSION FEATURES; do
	KEY="${COMMIT}|${ARCH}|${OS}|${PROFILE}|${ROOM_VERSION}|${FEATURES}"
	if [[ -n "${HAS_DETAILS[$KEY]+x}" ]]; then
		((SKIP++)) || true
		continue
	fi

	SAFE_ARCH=${ARCH//[!a-zA-Z0-9._-]/_}
	SAFE_OS=${OS//[!a-zA-Z0-9._-]/_}
	SAFE_PROFILE=${PROFILE//[!a-zA-Z0-9._-]/_}
	SAFE_ROOM_VERSION=${ROOM_VERSION//[!a-zA-Z0-9._-]/_}
	BASENAME="${COMMIT}-${SAFE_ARCH}-${SAFE_OS}-${SAFE_PROFILE}-${SAFE_ROOM_VERSION}.jsonl"

	if grep -Fqx "$BASENAME" <<<"$ALL_FILES" 2>/dev/null; then
		PENDING_FILES+=("runs_data/${BASENAME}")
		PENDING_META+=("${COMMIT}	${ARCH}	${OS}	${PROFILE}	${ROOM_VERSION}	${FEATURES}")
		((NEED++)) || true
	fi
done <"$TMPMANIFEST"

echo "→ Skipped $SKIP already-ingested runs, $NEED need details."

if [[ $NEED -eq 0 ]]; then
	echo "✓ All runs already have details. Nothing to do."
	exit 0
fi

# Stream only the NEW run detail files
echo "→ Streaming $NEED new run detail files..."
(
	# Advisory lock prevents deadlocks from parallel CI jobs
	echo "SELECT pg_advisory_lock(42);"

	echo "CREATE TEMP TABLE t (j jsonb);"
	printf '%s\n' "\copy t FROM STDIN csv quote e'\x01' delimiter e'\x02';"

	for i in "${!PENDING_FILES[@]}"; do
		IFS=$'\t' read -r COMMIT ARCH OS PROFILE ROOM_VERSION FEATURES <<<"${PENDING_META[$i]}"
		git show "FETCH_HEAD:${PENDING_FILES[$i]}" |
			jq -c --arg c "$COMMIT" --arg a "$ARCH" --arg o "$OS" --arg p "$PROFILE" --arg rv "$ROOM_VERSION" --arg f "$FEATURES" \
				'. + {commit: $c, arch: $a, os: $o, profile: $p, room_version: $rv, features: $f}'
	done

	echo "\."
	cat <<'SQL'
	-- Map the distinct run configurations in the temp table to actual run IDs
	CREATE TEMP TABLE newly_ingested_runs AS
	SELECT DISTINCT r.id AS run_id
	FROM (
		SELECT DISTINCT
			(j->>'commit') AS commit_hash,
			(NULLIF((j->>'arch'), '')) AS arch,
			(NULLIF((j->>'os'), '')) AS os,
			(NULLIF((j->>'profile'), '')) AS profile,
			(NULLIF((j->>'room_version'), '')) AS room_version,
			(NULLIF((j->>'features'), '')) AS features
		FROM t
	) nt
	JOIN runs r ON r.commit_hash = nt.commit_hash
		AND r.arch IS NOT DISTINCT FROM nt.arch
		AND r.os IS NOT DISTINCT FROM nt.os
		AND r.profile IS NOT DISTINCT FROM nt.profile
		AND r.room_version IS NOT DISTINCT FROM nt.room_version
		AND COALESCE(regexp_replace(btrim(r.features, ' ,'), '[,\s]+', ' ', 'g'), '') IS NOT DISTINCT FROM COALESCE(regexp_replace(btrim(nt.features, ' ,'), '[,\s]+', ' ', 'g'), '');

	CREATE UNIQUE INDEX idx_newly_ingested_runs ON newly_ingested_runs (run_id);

	INSERT INTO run_details (run_id, test_name, status)
	SELECT DISTINCT ON (r.id, (t.j->>'Test')) r.id, (t.j->>'Test'), (t.j->>'Action')
	FROM t
	JOIN runs r ON r.commit_hash = (t.j->>'commit')
		AND r.arch IS NOT DISTINCT FROM (NULLIF((t.j->>'arch'), ''))
		AND r.os IS NOT DISTINCT FROM (NULLIF((t.j->>'os'), ''))
		AND r.profile IS NOT DISTINCT FROM (NULLIF((t.j->>'profile'), ''))
		AND r.room_version IS NOT DISTINCT FROM (NULLIF((t.j->>'room_version'), ''))
		AND COALESCE(regexp_replace(btrim(r.features, ' ,'), '[,\s]+', ' ', 'g'), '') IS NOT DISTINCT FROM COALESCE(NULLIF((t.j->>'features'), ''), '')
	WHERE (t.j->>'Action') IN ('pass', 'fail', 'skip')
		AND r.id IN (SELECT run_id FROM newly_ingested_runs)
	ON CONFLICT (run_id, test_name) DO UPDATE SET status = EXCLUDED.status;

	-- Incremental ever_passed: scoped to only the newly ingested runs
	INSERT INTO ever_passed (test_name, rv, last_passed, last_commit, last_branch, branches)
	SELECT
			rd.test_name,
			COALESCE(r.room_version, '11'),
			MAX(r.run_date)::date::text,
			(ARRAY_AGG(r.commit_hash ORDER BY r.run_date DESC))[1],
			(ARRAY_AGG(r.branch ORDER BY r.run_date DESC))[1],
			ARRAY_AGG(DISTINCT r.branch) FILTER (WHERE r.branch IS NOT NULL)
	FROM run_details rd
	JOIN runs r ON rd.run_id = r.id
	WHERE rd.status = 'pass'
		AND r.id IN (SELECT run_id FROM newly_ingested_runs)
	GROUP BY rd.test_name, COALESCE(r.room_version, '11')
	ON CONFLICT (test_name, rv) DO UPDATE SET
			last_passed = GREATEST(ever_passed.last_passed, EXCLUDED.last_passed),
			last_commit = CASE
					WHEN EXCLUDED.last_passed > COALESCE(ever_passed.last_passed, '')
					THEN EXCLUDED.last_commit ELSE ever_passed.last_commit END,
			last_branch = CASE
					WHEN EXCLUDED.last_passed > COALESCE(ever_passed.last_passed, '')
					THEN EXCLUDED.last_branch ELSE ever_passed.last_branch END,
			branches = (
					SELECT ARRAY_AGG(DISTINCT b ORDER BY b)
					FROM UNNEST(ever_passed.branches || EXCLUDED.branches) AS b
					WHERE b IS NOT NULL
			);

	SELECT pg_advisory_unlock(42);
SQL
) | psql_remote

echo "✓ Incremental sync of last $LIMIT runs complete."
