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
    passed_count integer,
    skipped_count integer,
    failed_count integer
);

-- Ensure uniqueness for runs (identifies a specific machine's run for a commit at a specific time)
CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_unique_machine_run ON runs (commit_hash, run_date, arch, os) NULLS NOT DISTINCT;

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

CREATE INDEX IF NOT EXISTS idx_runs_commit_hash ON runs (commit_hash);

-- Baseline Logic CTE (shared by views)
CREATE OR REPLACE VIEW v_run_baselines AS
WITH stable_branches AS (
    SELECT
        id,
        run_date,
        arch,
        os,
        branch
    FROM
        runs
    WHERE
        branch IN ('main', 'main-upstream', 'main_upstream'))
SELECT
    r.id AS target_run_id,
    (
        SELECT
            s.id
        FROM
            stable_branches s
        WHERE
            s.arch IS NOT DISTINCT FROM r.arch
            AND s.os IS NOT DISTINCT FROM r.os
            AND ((r.branch IN ('main', 'main-upstream', 'main_upstream')
                    AND s.run_date < r.run_date)
                OR (r.branch NOT IN ('main', 'main-upstream', 'main_upstream')
                    AND s.run_date <= r.run_date))
        ORDER BY
            s.run_date DESC
        LIMIT 1) AS baseline_id
FROM
    runs r;

-- View for Run Regressions (includes Improvements and Skips as requested)
CREATE OR REPLACE VIEW v_run_regressions AS
SELECT
    r.*,
    -- n_failed_new
    (
        SELECT
            count(rd.test_name)
        FROM
            run_details rd
            JOIN v_run_baselines rb ON rb.target_run_id = r.id
            LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id
                AND bd.test_name = rd.test_name
        WHERE
            rd.run_id = r.id
            AND rd.status = 'fail'
            AND (bd.status IS NULL
                OR bd.status != 'fail')) AS n_failed_new,
    -- n_passed_new
    (
        SELECT
            count(rd.test_name)
        FROM
            run_details rd
            JOIN v_run_baselines rb ON rb.target_run_id = r.id
            LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id
                AND bd.test_name = rd.test_name
        WHERE
            rd.run_id = r.id
            AND rd.status = 'pass'
            AND (bd.status IS NULL
                OR bd.status != 'pass')) AS n_passed_new,
    -- n_skipped_new
    (
        SELECT
            count(rd.test_name)
        FROM
            run_details rd
            JOIN v_run_baselines rb ON rb.target_run_id = r.id
            LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id
                AND bd.test_name = rd.test_name
        WHERE
            rd.run_id = r.id
            AND rd.status = 'skip'
            AND (bd.status IS NULL
                OR bd.status != 'skip')) AS n_skipped_new,
    -- Details list
    (
        SELECT
            string_agg(rd.test_name, E'
')
        FROM
            run_details rd
            JOIN v_run_baselines rb ON rb.target_run_id = r.id
            LEFT JOIN run_details bd ON bd.run_id = rb.baseline_id
                AND bd.test_name = rd.test_name
        WHERE
            rd.run_id = r.id
            AND rd.status = 'fail'
            AND (bd.status IS NULL
                OR bd.status != 'fail')) AS new_failures_list
FROM
    runs r;
