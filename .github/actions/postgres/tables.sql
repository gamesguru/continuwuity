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
