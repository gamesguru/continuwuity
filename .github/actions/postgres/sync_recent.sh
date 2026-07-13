#!/usr/bin/env bash
set -euo pipefail

# sync_recent.sh
# Single ingest path for CI run data -> PostgreSQL. Two modes:
#   sync_recent.sh [LIMIT]                  bulk: catch up the last LIMIT runs from the orphan branch
#   sync_recent.sh --direct FILE META_JSON  direct: ingest one local run immediately, no git fetch
#
# Both modes share the same run_details/ever_passed upsert logic below, so there is exactly
# one place that knows how a run gets written -- avoids the two write paths drifting (e.g.
# computing run_date independently) and silently duplicating "runs" rows.
#
# import_history.sh also sources this file (for psql_remote/PSQL_SINK/ingest_details) to
# reuse the same ingest logic for its full-rebuild path, just pointed at a direct local
# connection instead of the SSH-tunneled one used here.

TARGET_BRANCH=${TARGET_BRANCH:-"_metadata/badges"}
export DB_TARGET=${DATABASE_URL:-"c10y"}
SSH_TARGET=${SSH_TARGET:-"git@nutra.tk"}

psql_remote() {
	ssh -C -o StrictHostKeyChecking=no -o ServerAliveInterval=30 "$SSH_TARGET" "psql -U git c10y"
}

# Command (function or binary name) that ingest_details() pipes assembled SQL into.
# Defaults to the SSH-tunneled remote psql; import_history.sh overrides this to a direct
# local connection before sourcing this file.
PSQL_SINK=${PSQL_SINK:-psql_remote}

# Reads NDJSON on stdin (each line already tagged with commit/arch/os/profile/room_version/
# features), streams it into a temp table, then upserts run_details + ever_passed for exactly
# the run rows those lines belong to.
ingest_details() {
	(
		echo "BEGIN;"
		echo "SET LOCAL synchronous_commit = OFF;"
		echo "CREATE TEMP TABLE t (j jsonb);"
		printf '%s\n' "\copy t FROM STDIN csv quote e'\x01' delimiter e'\x02';"
		cat
		echo "\."
		cat "$(dirname "${BASH_SOURCE[0]}")/ingest_details.sql"
		echo "COMMIT;"
	) | "$PSQL_SINK"
}

# Everything below only runs when this file is executed directly, not when sourced
# (import_history.sh sources it just for the ingest_details() function above).
if [[ "${BASH_SOURCE[0]}" != "${0}" ]]; then
	return 0
fi

if [[ "${1:-}" == "--direct" ]]; then
	RESULTS_FILE=$2
	RUN_META=$3

	COMMIT=$(jq -r '.commit_hash' <<<"$RUN_META")
	BRANCH=$(jq -r '.branch // ""' <<<"$RUN_META")
	ARCH=$(jq -r '.arch // ""' <<<"$RUN_META")
	OS=$(jq -r '.os // ""' <<<"$RUN_META")
	PROFILE=$(jq -r '.profile // ""' <<<"$RUN_META")
	ROOM_VERSION=$(jq -r '.room_version // ""' <<<"$RUN_META")
	VERSION=$(jq -r '.version_string // ""' <<<"$RUN_META")
	FEATURES=$(jq -r '.features // ""' <<<"$RUN_META")
	PASS=$(jq -r '.pass // 0' <<<"$RUN_META")
	FAIL=$(jq -r '.fail // 0' <<<"$RUN_META")
	SKIP=$(jq -r '.skip // 0' <<<"$RUN_META")
	RUN_DATE=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

	if [[ -z "$COMMIT" || ! -f "$RESULTS_FILE" ]]; then
		echo "⚠ Direct ingest skipped: missing commit_hash or results_file"
		exit 1
	fi

	echo "→ Direct ingest for $COMMIT ($ARCH/$OS/v$ROOM_VERSION)..."
	(
		echo "BEGIN;"
		echo "SET LOCAL synchronous_commit = OFF;"
		echo "INSERT INTO runs (run_date, commit_hash, branch, arch, os, profile, n_pass, n_skip, n_fail, room_version, features, version_string)
        SELECT '${RUN_DATE}'::timestamptz, '${COMMIT}', '${BRANCH}', '${ARCH}', '${OS}', '${PROFILE}', ${PASS}, ${SKIP}, ${FAIL}, '${ROOM_VERSION}',
          COALESCE(regexp_replace(btrim('${FEATURES}', ' ,'), '[,\s]+', ' ', 'g'), ''), '${VERSION}'
        ON CONFLICT (commit_hash, arch, os, profile, room_version, features) DO NOTHING;"
		echo "COMMIT;"
	) | psql_remote

	jq -c --arg c "$COMMIT" --arg a "$ARCH" --arg o "$OS" --arg p "$PROFILE" --arg rv "$ROOM_VERSION" --arg f "$FEATURES" \
		'. + {commit: $c, arch: $a, os: $o, profile: $p, room_version: $rv, features: ($f | gsub("[,\\s]+"; " ") | gsub("^ | $"; ""))}' "$RESULTS_FILE" |
		ingest_details
	echo "✓ Direct ingest complete."
	exit 0
fi

LIMIT=${1:-100}

echo "→ Fetching latest metadata from origin/$TARGET_BRANCH..."
git fetch origin "$TARGET_BRANCH" --depth 1 --filter=blob:none >/dev/null 2>&1

# Ingest Recent Summaries (fast — ON CONFLICT DO NOTHING skips existing)
echo "→ Streaming last $LIMIT run summaries..."
(
	echo "BEGIN;"
	echo "SET LOCAL synchronous_commit = OFF;"
	echo "CREATE TEMP TABLE b (j jsonb);"
	printf '%s\n' "\copy b FROM STDIN csv quote e'\x01' delimiter e'\x02';"
	git show "FETCH_HEAD:runs.jsonl" | tail -n "$LIMIT"
	echo "\."
	echo "INSERT INTO runs (run_date, commit_hash, upstream_commit, branch, author_name, actor, provider, arch, os, version_string, features, profile, binary_sha256, n_pass, n_skip, n_fail, room_version)
        SELECT
          (j->>'run_date')::timestamptz, (j->>'commit_hash'), (j->>'upstream_commit'), (j->>'branch'),
          (j->>'author_name'), (j->>'actor'), (j->>'provider'), NULLIF(j->>'arch', ''), NULLIF(j->>'os', ''),
          (j->>'version_string'), COALESCE(btrim(regexp_replace(j->>'features', '[,\s]+', ' ', 'g'), ' ,'), ''), (j->>'profile'), (j->>'binary_sha256'),
          (j->'passed_count')::int, (j->'skipped_count')::int, (j->'failed_count')::int, (j->>'room_version')
        FROM b ON CONFLICT (commit_hash, arch, os, profile, room_version, features) DO NOTHING;"
	echo "COMMIT;"
) | psql_remote

# Pre-cache git tree for fast existence checks
ALL_FILES=$(git ls-tree -r FETCH_HEAD:runs_data --name-only || true)

# Build manifest with single jq pass (instead of 5× jq calls per line)
TMPMANIFEST=$(mktemp)
trap 'rm -f "$TMPMANIFEST"' EXIT
git show "FETCH_HEAD:runs.jsonl" | tail -n "$LIMIT" |
	jq -r '[.commit_hash, (.arch // ""), (.os // ""), (.profile // ""), (.room_version // ""), ((.features // "") | gsub("[,\\s]+"; " ") | gsub("^ | $"; ""))] | @tsv' \
		>"$TMPMANIFEST"

NEED=0
declare -a PENDING_FILES=()
declare -a PENDING_META=()

while IFS=$'\t' read -r COMMIT ARCH OS PROFILE ROOM_VERSION FEATURES; do
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

echo "→ $NEED runs have detail files to ingest."

if [[ $NEED -eq 0 ]]; then
	echo "✓ All runs already have details. Nothing to do."
	exit 0
fi

# Stream only the NEW run detail files
echo "→ Streaming $NEED new run detail files..."
(
	for i in "${!PENDING_FILES[@]}"; do
		IFS=$'\t' read -r COMMIT ARCH OS PROFILE ROOM_VERSION FEATURES <<<"${PENDING_META[$i]}"
		git show "FETCH_HEAD:${PENDING_FILES[$i]}" |
			jq -c --arg c "$COMMIT" --arg a "$ARCH" --arg o "$OS" --arg p "$PROFILE" --arg rv "$ROOM_VERSION" --arg f "$FEATURES" \
				'. + {commit: $c, arch: $a, os: $o, profile: $p, room_version: $rv, features: $f}'
	done
) | ingest_details

echo "✓ Incremental sync of last $LIMIT runs complete."
