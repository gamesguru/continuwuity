-- Create runs table
CREATE TABLE IF NOT EXISTS runs (
    id serial PRIMARY KEY,
    run_id text NOT NULL,
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
    passed_count integer,
    skipped_count integer,
    failed_count integer
);

-- Ensure uniqueness for runs (handles NULL arch/os correctly in PG 15+)
CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_unique_run ON runs (run_id, arch, os) NULLS NOT DISTINCT;

-- Create run_details table
CREATE TABLE IF NOT EXISTS run_details (
    id serial PRIMARY KEY,
    run_id integer REFERENCES runs (id) ON DELETE CASCADE,
    test_name text NOT NULL,
    status text NOT NULL
);

-- Ensure uniqueness for test results
CREATE UNIQUE INDEX IF NOT EXISTS idx_run_details_unique_test ON run_details (run_id, test_name);

-- Create indexes for performance
CREATE INDEX IF NOT EXISTS idx_run_details_run_id ON run_details (run_id);
CREATE INDEX IF NOT EXISTS idx_runs_run_id ON runs (run_id);

-- View for Run Regressions (New Failures vs Main)
CREATE OR REPLACE VIEW v_run_regressions AS
WITH baseline AS (
    -- For each unique (arch, os), find the ID of the latest 'main' run
    SELECT DISTINCT ON (arch, os)
        id, arch, os
    FROM runs
    WHERE branch = 'main'
    ORDER BY arch, os, run_date DESC
)
SELECT
    r.id,
    r.run_id,
    r.version_string,
    r.run_date,
    r.branch,
    r.arch,
    r.os,
    r.failed_count as total_failed,
    -- Count tests that fail in current run but did NOT fail in the latest main run
    (
        SELECT count(rd.test_name)
        FROM run_details rd
        LEFT JOIN baseline b ON b.arch IS NOT DISTINCT FROM r.arch AND b.os IS NOT DISTINCT FROM r.os
        LEFT JOIN run_details bd ON bd.run_id = b.id AND bd.test_name = rd.test_name
        WHERE rd.run_id = r.id
          AND rd.status = 'fail'
          AND (bd.status IS NULL OR bd.status != 'fail')
    ) as n_failed_new,
    -- Aggregated list of new failures
    (
        SELECT string_agg(rd.test_name, E'\n')
        FROM run_details rd
        LEFT JOIN baseline b ON b.arch IS NOT DISTINCT FROM r.arch AND b.os IS NOT DISTINCT FROM r.os
        LEFT JOIN run_details bd ON bd.run_id = b.id AND bd.test_name = rd.test_name
        WHERE rd.run_id = r.id
          AND rd.status = 'fail'
          AND (bd.status IS NULL OR bd.status != 'fail')
    ) as new_failures_list
FROM runs r;
