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
    room_version text,
    n_pass integer DEFAULT 0,
    n_skip integer DEFAULT 0,
    n_fail integer DEFAULT 0
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

-- Optimization: Disable sequence caching to prevent jumps of 100 in IDs
-- (Standard Postgres behavior for serial/identity columns in some environments)
ALTER SEQUENCE IF EXISTS runs_id_seq CACHE 1;
ALTER SEQUENCE IF EXISTS run_details_id_seq CACHE 1;

-- Ensure we don't log the same test twice for the same run
CREATE UNIQUE INDEX IF NOT EXISTS idx_run_details_unique_test
ON run_details (run_id, test_name);

-- Performance indexes
CREATE INDEX IF NOT EXISTS idx_run_details_run_id ON run_details (run_id);
CREATE INDEX IF NOT EXISTS idx_runs_commit_hash ON runs (commit_hash);

-- Combine Regressions View: Compares directly against the most recent 'main' baseline
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
    r.room_version,
    r.n_pass,
    r.n_fail,
    r.n_skip,
    (SELECT COUNT(*) FROM run_details WHERE run_id = dbr.default_baseline_run_id) as baseline_total,
    counts.run_total,
    (counts.run_total - (SELECT COUNT(*) FROM run_details WHERE run_id = dbr.default_baseline_run_id)) as diff_total,
    -- Calculate deltas vs Default Baseline
    counts.new_pass,
    counts.new_skip,
    counts.new_fail,
    counts.new_failures_list,
    counts.new_passes_list
FROM runs r
CROSS JOIN LATERAL (
    SELECT id AS default_baseline_run_id FROM runs
    WHERE (branch IN ('main', 'main-upstream', 'refs/heads/main', 'refs/heads/main-upstream')
    OR version_string LIKE '%main%')
    AND room_version IS NOT DISTINCT FROM r.room_version
    ORDER BY run_date DESC
    LIMIT 1
) dbr
LEFT JOIN LATERAL (
    SELECT
        COUNT(*) as run_total,
        COUNT(*) FILTER (WHERE rd.status = 'pass' AND (dbr.default_baseline_run_id IS NOT NULL AND (mb.status IS NULL OR mb.status != 'pass'))) as new_pass,
        COUNT(*) FILTER (WHERE rd.status = 'skip' AND (dbr.default_baseline_run_id IS NOT NULL AND (mb.status IS NULL OR mb.status != 'skip'))) as new_skip,
        COUNT(*) FILTER (WHERE rd.status = 'fail' AND (dbr.default_baseline_run_id IS NOT NULL AND (mb.status IS NULL OR mb.status != 'fail'))) as new_fail,
        STRING_AGG(rd.test_name, E'\n' ORDER BY rd.test_name) FILTER (WHERE rd.status = 'fail' AND (dbr.default_baseline_run_id IS NOT NULL AND (mb.status IS NULL OR mb.status != 'fail'))) as new_failures_list,
        STRING_AGG(rd.test_name, E'\n' ORDER BY rd.test_name) FILTER (WHERE rd.status = 'pass' AND (dbr.default_baseline_run_id IS NOT NULL AND (mb.status IS NULL OR mb.status != 'pass'))) as new_passes_list
    FROM run_details rd
    LEFT JOIN run_details mb ON mb.test_name = rd.test_name AND mb.run_id = dbr.default_baseline_run_id
    WHERE rd.run_id = r.id
) counts ON TRUE
WHERE r.n_pass > 0 AND counts.run_total > 0;

-- Ensure read-only users can query the view and raw tables even after they get recreated by CI
GRANT SELECT ON ALL TABLES IN SCHEMA public TO public;
