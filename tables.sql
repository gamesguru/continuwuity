CREATE TABLE IF NOT EXISTS runs (
    run_id text PRIMARY KEY,
    run_date text,
    commit_hash text,
    branch text,
    author_name text,
    provider text,
    host_info text,
    binary_sha256 text,
    passed_count integer,
    skipped_count integer,
    failed_count integer,
    row_hash text
);

CREATE TABLE IF NOT EXISTS run_details (
    run_id text,
    file_name text,
    status text,
    row_hash text,
    UNIQUE (run_id, file_name)
);

CREATE TABLE IF NOT EXISTS test_scores (
    file_name text PRIMARY KEY,
    total_runs integer DEFAULT 0,
    passed_count integer DEFAULT 0,
    failed_count integer DEFAULT 0,
    skipped_count integer DEFAULT 0
);
