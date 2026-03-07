import sqlite3
import json
import os
import hashlib

INSERT_RUN = """
INSERT
    OR IGNORE INTO runs (version_string, binary_sha256, run_date, features, commit_hash, branch, author_name, provider, host_info, passed_count, skipped_count, failed_count, prev_hash, row_hash)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);
"""

INSERT_RUN_DETAILS = """
INSERT
    OR IGNORE INTO run_details (version_string, run_date, file_name, status, row_hash)
        VALUES (?, ?, ?, ?, ?);
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

# NOTE: The database `ledger.db` is ephemeral and not tracked in git.
# It is rebuilt organically from `runs.jsonl` and `run_details.jsonl`
# from the ground up on each Action run. This means we can retroactively
# fix data models and hashes simply by letting this script re-process the files.
# This should be relatively quick for up to 50,000 commits/runs or more.

if os.path.exists("runs.jsonl"):
    with open("runs.jsonl") as f:
        for line in f:
            try:
                line = line.strip()
                if not line:
                    continue
                d = json.loads(line)

                # Get the last hash for chaining
                cur = db.cursor()
                cur.execute(
                    "SELECT row_hash FROM runs ORDER BY run_date DESC, rowid DESC LIMIT 1"
                )
                res = cur.fetchone()
                prev_hash = res[0] if res else "0" * 64

                # Deterministic hashing: remove nulls, sort keys, strip whitespace
                clean_d = {k: v for k, v in d.items() if v is not None}
                # Include prev_hash in the data to be hashed
                clean_d["prev_hash"] = prev_hash

                canonical_str = json.dumps(
                    clean_d, separators=(",", ":"), sort_keys=True
                )
                row_hash = hashlib.sha256(canonical_str.encode("utf-8")).hexdigest()

                db.execute(
                    INSERT_RUN,
                    (
                        d.get("version_string"),
                        d.get("binary_sha256"),
                        d.get("run_date"),
                        d.get("features"),
                        d.get("commit_hash"),
                        d.get("branch"),
                        d.get("author_name"),
                        d.get("provider"),
                        d.get("host_info"),
                        d.get("passed_count"),
                        d.get("skipped_count"),
                        d.get("failed_count"),
                        prev_hash,
                        row_hash,
                    ),
                )
            except Exception as e:
                print(f"Error runs: {e}")

if os.path.exists("run_details.jsonl"):
    with open("run_details.jsonl") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            d = json.loads(line)
            # Deterministic hashing: remove nulls, sort keys, strip whitespace
            clean_d = {k: v for k, v in d.items() if v is not None}
            canonical_str = json.dumps(clean_d, separators=(",", ":"), sort_keys=True)
            row_hash = hashlib.sha256(canonical_str.encode("utf-8")).hexdigest()

            file_name = d.get("Test")
            status = d.get("Action")

            cur = db.cursor()
            cur.execute(
                INSERT_RUN_DETAILS,
                (
                    d.get("version_string"),
                    d.get("run_date"),
                    file_name,
                    status,
                    row_hash,
                ),
            )

            if cur.rowcount > 0:
                db.execute(INSERT_TEST_SCORE, (file_name,))
                if status == "pass":
                    db.execute(UPDATE_PASS, (file_name,))
                elif status == "fail":
                    db.execute(UPDATE_FAIL, (file_name,))
                elif status == "skip":
                    db.execute(UPDATE_SKIP, (file_name,))


db.commit()

github_output = os.environ.get("GITHUB_OUTPUT")


if github_output:
    cur = db.cursor()
    # Get the previous run on the same branch (excluding the current one)
    cur.execute("""
        SELECT passed_count, failed_count, skipped_count 
        FROM runs 
        ORDER BY run_date DESC 
        LIMIT 1 OFFSET 1
    """)
    prev_run = cur.fetchone()
    if prev_run:
        with open(github_output, "a") as f:
            f.write(f"prev_pass={prev_run[0]}\n")
            f.write(f"prev_fail={prev_run[1]}\n")
            f.write(f"prev_skip={prev_run[2]}\n")

    # Get the 5 most recent tags (defined as runs with short version strings, e.g. v0.5.6)
    # Since Forgejo doesn't natively expose 'tag' as a column, we can do a LIKE query
    # or just sort older runs. Wait, the user has 'version_string' = 'v0.5.6' vs 'v0.5.6+123~abc'.
    cur.execute("""
        SELECT version_string, passed_count, failed_count, skipped_count 
        FROM runs 
        WHERE version_string NOT LIKE '%+%' AND version_string NOT LIKE '%~%'
        ORDER BY run_date DESC 
        LIMIT 5
    """)
    tags = cur.fetchall()
    if tags:
        tags_json = json.dumps(
            [{"version": t[0], "pass": t[1], "fail": t[2], "skip": t[3]} for t in tags]
        )
        with open(github_output, "a") as f:
            f.write(f"historical_tags={tags_json}\n")
