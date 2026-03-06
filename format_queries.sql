INSERT OR IGNORE INTO runs (run_id, run_date, commit_hash, branch, author_name, provider, host_info, binary_sha256, passed_count, skipped_count, failed_count, row_hash) VALUES (?,?,?,?,?,?,?,?,?,?,?,?);
INSERT OR IGNORE INTO run_details (run_id, file_name, status, row_hash) VALUES (?,?,?,?);
INSERT OR IGNORE INTO test_scores (file_name) VALUES (?);
UPDATE test_scores SET total_runs = total_runs + 1, passed_count = passed_count + 1 WHERE file_name = ?;
UPDATE test_scores SET total_runs = total_runs + 1, failed_count = failed_count + 1 WHERE file_name = ?;
UPDATE test_scores SET total_runs = total_runs + 1, skipped_count = skipped_count + 1 WHERE file_name = ?;
