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
    failed_count integer
);

CREATE TABLE IF NOT EXISTS run_details (
    run_id text,
    file_name text,
    status text
);

