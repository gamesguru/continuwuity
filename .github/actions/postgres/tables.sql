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

-- Create a table for the Master Baseline (from origin/main files)
CREATE TABLE IF NOT EXISTS master_baseline (
    test_name text PRIMARY KEY,
    status text NOT NULL
);

-- Performance indexes
CREATE INDEX IF NOT EXISTS idx_run_details_run_id ON run_details (run_id);
CREATE INDEX IF NOT EXISTS idx_runs_commit_hash ON runs (commit_hash);

-- Combined Regressions View: Compares directly against the Master Baseline
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
    r.features,
    r.profile,
    r.n_pass,
    r.n_fail,
    r.n_skip,
    (SELECT COUNT(*) FROM master_baseline) as baseline_total,
    counts.run_total,
    (counts.run_total - (SELECT COUNT(*) FROM master_baseline)) as diff_total,
    -- Calculate deltas vs Master Baseline
    counts.new_pass,
    counts.new_skip,
    counts.new_fail,
    counts.new_failures_list
FROM runs r
LEFT JOIN LATERAL (
    SELECT
        COUNT(*) as run_total,
        COUNT(*) FILTER (WHERE rd.status = 'pass' AND (mb.status IS NULL OR mb.status != 'pass')) as new_pass,
        COUNT(*) FILTER (WHERE rd.status = 'skip' AND (mb.status IS NULL OR mb.status != 'skip')) as new_skip,
        COUNT(*) FILTER (WHERE rd.status = 'fail' AND (mb.status IS NULL OR mb.status != 'fail')) as new_fail,
        STRING_AGG(rd.test_name, E'\n') FILTER (WHERE rd.status = 'fail' AND (mb.status IS NULL OR mb.status != 'fail')) as new_failures_list
    FROM run_details rd
    LEFT JOIN master_baseline mb ON mb.test_name = rd.test_name
    WHERE rd.run_id = r.id
) counts ON TRUE
WHERE r.n_pass > 0 AND counts.run_total > 0;

-- Ensure read-only users can query the view and raw tables even after they get recreated by CI
GRANT SELECT ON ALL TABLES IN SCHEMA public TO public;
