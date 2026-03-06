import sqlite3
import json
import os
import hashlib

INSERT_RUN = """
INSERT
    OR IGNORE INTO runs (run_id, run_date, commit_hash, branch, author_name, provider, host_info, binary_sha256, version_string, passed_count, skipped_count, failed_count, row_hash)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);
"""

INSERT_RUN_DETAILS = """
INSERT
    OR IGNORE INTO run_details (run_id, file_name, status, row_hash)
        VALUES (?, ?, ?, ?);
"""

INSERT_TEST_SCORE = """
INSERT
    OR IGNORE INTO test_scores (file_name)
        VALUES (?);
"""

UPDATE_PASS = """
UPDATE
    test_scores
SET
    total_runs = total_runs + 1,
    passed_count = passed_count + 1
WHERE
    file_name = ?;
"""

UPDATE_FAIL = """
UPDATE
    test_scores
SET
    total_runs = total_runs + 1,
    failed_count = failed_count + 1
WHERE
    file_name = ?;
"""

UPDATE_SKIP = """
UPDATE
    test_scores
SET
    total_runs = total_runs + 1,
    skipped_count = skipped_count + 1
WHERE
    file_name = ?;
"""

db = sqlite3.connect("ledger.db")
db.executescript(open("tables.sql").read())

if os.path.exists("runs.jsonl"):
    with open("runs.jsonl") as f:
        for line in f:
            try:
                line = line.strip()
                if not line:
                    continue
                d = json.loads(line)
                # Deterministic hashing: remove nulls, sort keys, strip whitespace
                clean_d = {k: v for k, v in d.items() if v is not None}
                canonical_str = json.dumps(
                    clean_d, separators=(",", ":"), sort_keys=True
                )
                row_hash = hashlib.sha256(canonical_str.encode("utf-8")).hexdigest()

                db.execute(
                    INSERT_RUN,
                    (
                        d.get("run_id"),
                        d.get("run_date"),
                        d.get("commit_hash"),
                        d.get("branch"),
                        d.get("author_name"),
                        d.get("provider"),
                        d.get("host_info"),
                        d.get("binary_sha256"),
                        d.get("version_string"),
                        d.get("passed_count"),
                        d.get("skipped_count"),
                        d.get("failed_count"),
                        row_hash,
                    ),
                )
            except Exception as e:
                print(f"Error runs: {e}")

if os.path.exists("run_details.jsonl"):
    with open("run_details.jsonl") as f:
        for line in f:
            try:
                line = line.strip()
                if not line:
                    continue
                d = json.loads(line)
                # Deterministic hashing: remove nulls, sort keys, strip whitespace
                clean_d = {k: v for k, v in d.items() if v is not None}
                canonical_str = json.dumps(
                    clean_d, separators=(",", ":"), sort_keys=True
                )
                row_hash = hashlib.sha256(canonical_str.encode("utf-8")).hexdigest()

                file_name = d.get("Test")
                status = d.get("Action")

                cur = db.cursor()
                cur.execute(
                    INSERT_RUN_DETAILS,
                    (d.get("run_id"), file_name, status, row_hash),
                )

                if cur.rowcount > 0:
                    db.execute(INSERT_TEST_SCORE, (file_name,))
                    if status == "pass":
                        db.execute(UPDATE_PASS, (file_name,))
                    elif status == "fail":
                        db.execute(UPDATE_FAIL, (file_name,))
                    elif status == "skip":
                        db.execute(UPDATE_SKIP, (file_name,))
            except Exception as e:
                print(f"Error details: {e}")

db.commit()
