---
-- tables.sql
-- Relational schema for Continuwuity CI runs.

-- Drop views first to allow column/logic updates
DROP VIEW IF EXISTS v_run_deltas CASCADE;
DROP VIEW IF EXISTS v_run_regressions CASCADE;
DROP VIEW IF EXISTS v_run_baselines CASCADE;

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
    binary_sha256 text,
    passed_count integer DEFAULT 0,
    skipped_count integer DEFAULT 0,
    failed_count integer DEFAULT 0
);

-- Unique index to distinguish every machine run, including re-runs at different times
-- Composite key: (commit_hash, run_date, arch, os)
CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_unique_machine_run
ON runs (commit_hash, run_date, arch, os) NULLS NOT DISTINCT;

-- Create run_details table
CREATE TABLE IF NOT EXISTS run_details (
    id serial PRIMARY KEY,
    run_id integer REFERENCES runs (id) ON DELETE CASCADE,
    test_name text NOT NULL,
    status text NOT NULL
);

-- Ensure we don't log the same test twice for the same machine run
CREATE UNIQUE INDEX IF NOT EXISTS idx_run_details_unique_test
ON run_details (run_id, test_name);

-- Performance indexes
CREATE INDEX IF NOT EXISTS idx_run_details_run_id ON run_details (run_id);
CREATE INDEX IF NOT EXISTS idx_runs_commit_hash ON runs (commit_hash);

-- Baseline Logic: Finds the BEST run (highest pass count) on a stable branch
CREATE OR REPLACE VIEW v_run_baselines AS
WITH stable_scores AS (
    SELECT
        id,
        arch,
        os,
        passed_count,
        ROW_NUMBER() OVER (
            PARTITION BY arch, os
            ORDER BY passed_count DESC, run_date DESC
        ) as rank
    FROM
        runs
    WHERE
        branch IN ('main', 'main-upstream', 'main_upstream')
        AND passed_count > 0
)
SELECT
    r.id AS target_run_id,
    (SELECT s.id FROM stable_scores s WHERE s.rank = 1 AND s.arch IS NOT DISTINCT FROM r.arch AND s.os IS NOT DISTINCT FROM r.os LIMIT 1) AS baseline_id
FROM
    runs r;

-- Final Combined Regressions View
CREATE OR REPLACE VIEW v_run_regressions AS
SELECT
    r.id,
    r.version_string,
    r.run_date,
    r.commit_hash,
    r.upstream_commit AS upstream_sha,
    r.branch,
    r.arch,
    r.os,
    r.passed_count,
    r.failed_count,
    r.skipped_count,
    -- n_fail (New Failures)
    (
        SELECT count(rd.test_name)
        FROM run_details rd
        JOIN v_run_baselines rb ON rb.target_run_id = r.id
        LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id AND bd.test_name = rd.test_name
        WHERE rd.run_id = r.id AND rd.status = 'fail' AND (bd.status IS NULL OR bd.status != 'fail')
    ) AS n_fail,
    -- n_pass (New Passes)
    (
        SELECT count(rd.test_name)
        FROM run_details rd
        JOIN v_run_baselines rb ON rb.target_run_id = r.id
        LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id AND bd.test_name = rd.test_name
        WHERE rd.run_id = r.id AND rd.status = 'pass' AND (bd.status IS NULL OR bd.status != 'pass')
    ) AS n_pass,
    -- n_skip (New Skips)
    (
        SELECT count(rd.test_name)
        FROM run_details rd
        JOIN v_run_baselines rb ON rb.target_run_id = r.id
        LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id AND bd.test_name = rd.test_name
        WHERE rd.run_id = r.id AND rd.status = 'skip' AND (bd.status IS NULL OR bd.status != 'skip')
    ) AS n_skip,
    -- New Failures List
    (
        SELECT string_agg(rd.test_name, E'\n')
        FROM run_details rd
        JOIN v_run_baselines rb ON rb.target_run_id = r.id
        LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id AND bd.test_name = rd.test_name
        WHERE rd.run_id = r.id AND rd.status = 'fail' AND (bd.status IS NULL OR bd.status != 'fail')
    ) AS new_failures_list
FROM
    runs r
WHERE
    r.passed_count > 0;
