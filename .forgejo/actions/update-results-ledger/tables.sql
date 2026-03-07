CREATE TABLE IF NOT EXISTS runs (
    version_string text,
    binary_sha256 text,
    run_date text,
    features text,
    commit_hash text,
    branch text,
    author_name text,
    provider text,
    host_info text,
    passed_count integer,
    skipped_count integer,
    failed_count integer,
    prev_hash text,
    row_hash text,
    PRIMARY KEY (version_string, run_date)
);

CREATE TABLE IF NOT EXISTS run_details (
    version_string text,
    run_date text,
    file_name text,
    status text,
    row_hash text,
    UNIQUE (version_string, run_date, file_name)
);

CREATE TABLE IF NOT EXISTS test_scores (
    file_name text PRIMARY KEY,
    total_runs integer DEFAULT 0,
    passed_count integer DEFAULT 0,
    failed_count integer DEFAULT 0,
    skipped_count integer DEFAULT 0
);
