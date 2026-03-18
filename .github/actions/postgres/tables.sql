-- tables.sql
-- Relational schema for Continuwuity CI runs.

-- Drop views to allow column/logic updates
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
    n_pass integer DEFAULT 0,
    n_skip integer DEFAULT 0,
    n_fail integer DEFAULT 0
);

-- Unique index to prevent duplicate machine reports
CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_unique_machine_run
ON runs (commit_hash, run_date, arch, os) NULLS NOT DISTINCT;

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

-- Baseline Logic: Finds the BEST run (highest pass count) on origin/main or main-upstream
CREATE OR REPLACE VIEW v_run_baselines AS
WITH stable_scores AS (
    SELECT
        id,
        arch,
        os,
        n_pass,
        ROW_NUMBER() OVER (
            PARTITION BY arch, os
            ORDER BY n_pass DESC, run_date DESC
        ) as rank
    FROM
        runs
    WHERE
        branch IN ('main', 'main-upstream', 'main_upstream')
        AND n_pass > 0
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
    r.n_pass,
    r.n_fail,
    r.n_skip,
    -- new_fail (New Failures vs Best Baseline)
    (
        SELECT count(rd.test_name)
        FROM run_details rd
        JOIN v_run_baselines rb ON rb.target_run_id = r.id
        LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id AND bd.test_name = rd.test_name
        WHERE rd.run_id = r.id AND rd.status = 'fail' AND (bd.status IS NULL OR bd.status != 'fail')
    ) AS new_fail,
    -- new_pass (New Passes vs Best Baseline)
    (
        SELECT count(rd.test_name)
        FROM run_details rd
        JOIN v_run_baselines rb ON rb.target_run_id = r.id
        LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id AND bd.test_name = rd.test_name
        WHERE rd.run_id = r.id AND rd.status = 'pass' AND (bd.status IS NULL OR bd.status != 'pass')
    ) AS new_pass,
    -- new_skip (New Skips vs Best Baseline)
    (
        SELECT count(rd.test_name)
        FROM run_details rd
        JOIN v_run_baselines rb ON rb.target_run_id = r.id
        LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id AND bd.test_name = rd.test_name
        WHERE rd.run_id = r.id AND rd.status = 'skip' AND (bd.status IS NULL OR bd.status != 'skip')
    ) AS new_skip,
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
    r.n_pass > 0;
