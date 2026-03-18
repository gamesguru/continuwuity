-- Create runs table
CREATE TABLE IF NOT EXISTS runs (
    id serial PRIMARY KEY,
    run_id text NOT NULL, -- Shared across machines in a matrix
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
    failed_count integer,
    -- Ensure we don't ingest the same machine's report for the same run twice.
    UNIQUE NULLS NOT DISTINCT (run_id, arch, os)
);

-- Create run_details table
CREATE TABLE IF NOT EXISTS run_details (
    id serial PRIMARY KEY,
    run_id integer REFERENCES runs (id) ON DELETE CASCADE, -- Links to the specific machine run
    test_name text NOT NULL,
    status text NOT NULL,
    -- Ensure we don't ingest the same test result for the same machine run twice.
    UNIQUE (run_id, test_name)
);

-- Create index for performance
CREATE INDEX IF NOT EXISTS idx_run_details_run_id ON run_details (run_id);
CREATE INDEX IF NOT EXISTS idx_runs_run_id ON runs (run_id);
