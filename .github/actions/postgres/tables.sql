-- tables.sql
-- Relational schema for Continuwuity CI runs.

-- Drop views to allow schema updates
DROP VIEW IF EXISTS v_run_regressions CASCADE;

-- Create runs table
CREATE TABLE IF NOT EXISTS runs (
    id serial PRIMARY KEY,
    run_date timestamp with time zone NOT NULL,
    commit_hash text NOT NULL,
    upstream_commit text,
    branch text,
    author_name text,
    actor text,
    provider text,
    arch text,
    os text,
    version_string text,
    features text,
    profile text,
    binary_sha256 text,
    n_pass integer DEFAULT 0,
    n_skip integer DEFAULT 0,
    n_fail integer DEFAULT 0,
    room_version text
);

-- Unique index to prevent duplicate machine reports
CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_unique_machine_run
ON runs (commit_hash, run_date, arch, os, profile, room_version) NULLS NOT DISTINCT;

-- Create run_details table
CREATE TABLE IF NOT EXISTS run_details (
    id serial PRIMARY KEY,
    run_id integer REFERENCES runs (id) ON DELETE CASCADE,
    test_name text NOT NULL,
    status text NOT NULL
);

-- Ensure we don't log the same test twice for the same run
CREATE UNIQUE INDEX IF NOT EXISTS idx_run_details_unique_test
ON run_details (run_id, test_name);

-- Performance indexes
CREATE INDEX IF NOT EXISTS idx_run_details_run_id ON run_details (run_id);
CREATE INDEX IF NOT EXISTS idx_runs_commit_hash ON runs (commit_hash);

-- Pre-computed set of tests that have ever passed, per room_version.
-- Refreshed by CI after each insert. Enables O(1) regression lookups.
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_ever_passed AS
SELECT rd.test_name,
       COALESCE(r.room_version, '11') AS rv,
       MAX(r.run_date)::date::text AS last_passed,
       '[' || (SELECT STRING_AGG(b, ', ') FROM (
           SELECT DISTINCT unnest(array_agg(LEFT(r.branch, 30) ORDER BY r.run_date DESC)) AS b LIMIT 3
       ) t) || ']' AS branches,
       (array_agg(LEFT(r.commit_hash, 10) ORDER BY r.run_date DESC))[1] AS last_commit
FROM run_details rd
JOIN runs r ON rd.run_id = r.id
WHERE rd.status = 'pass'
GROUP BY rd.test_name, COALESCE(r.room_version, '11');

CREATE UNIQUE INDEX IF NOT EXISTS idx_mv_ever_passed
ON mv_ever_passed (test_name, rv);

-- Global regression view: a test is a "new failure" if it fails now
-- AND has ever passed in any run with the same room_version.
CREATE OR REPLACE VIEW v_run_regressions AS
SELECT
    r.id,
    r.version_string,
    r.run_date,
    r.commit_hash,
    r.upstream_commit AS upstream_sha,
    r.author_name,
    r.actor,
    r.branch,
    r.arch,
    r.os,
    r.room_version,
    r.features,
    r.profile,
    r.n_pass,
    r.n_fail,
    r.n_skip,
    counts.*
FROM runs r
LEFT JOIN LATERAL (
    SELECT
        COUNT(*) AS run_total,
        COUNT(*) FILTER (WHERE rd.status = 'pass' AND ep.test_name IS NULL) AS new_pass,
        COUNT(*) FILTER (WHERE rd.status = 'fail' AND ep.test_name IS NOT NULL) AS new_fail,
        COUNT(*) FILTER (WHERE rd.status = 'skip') AS new_skip,
        STRING_AGG(rd.test_name, E'\n' ORDER BY rd.test_name)
            FILTER (WHERE rd.status = 'fail' AND ep.test_name IS NOT NULL) AS new_failures_list,
        STRING_AGG(rd.test_name, E'\n' ORDER BY rd.test_name)
            FILTER (WHERE rd.status = 'pass' AND ep.test_name IS NULL) AS new_passes_list
    FROM run_details rd
    LEFT JOIN mv_ever_passed ep
        ON ep.test_name = rd.test_name
        AND ep.rv IS NOT DISTINCT FROM COALESCE(r.room_version, '11')
    WHERE rd.run_id = r.id
) counts ON TRUE
WHERE r.n_pass > 0 AND counts.run_total > 0;

-- Auto-grant SELECT on all current and future objects so read-only users always work
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO public;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO public;
