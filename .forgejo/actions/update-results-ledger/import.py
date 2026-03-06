import sqlite3, json, os

db = sqlite3.connect("ledger.db")
db.executescript(open("tables.sql").read())

if os.path.exists("runs.jsonl"):
    with open("runs.jsonl") as f:
        for line in f:
            try:
                d = json.loads(line)
                db.execute(
                    "INSERT OR IGNORE INTO runs (run_id, run_date, commit_hash, branch, author_name, provider, host_info, binary_sha256, passed_count, skipped_count, failed_count) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        d.get("run_id"),
                        d.get("run_date"),
                        d.get("commit_hash"),
                        d.get("branch"),
                        d.get("author_name"),
                        d.get("provider"),
                        d.get("host_info"),
                        d.get("binary_sha256"),
                        d.get("passed_count"),
                        d.get("skipped_count"),
                        d.get("failed_count"),
                    ),
                )
            except:
                pass

if os.path.exists("run_details.jsonl"):
    with open("run_details.jsonl") as f:
        for line in f:
            try:
                d = json.loads(line)
                db.execute(
                    "INSERT OR IGNORE INTO run_details (run_id, file_name, status) VALUES (?,?,?)",
                    (d.get("run_id"), d.get("Test"), d.get("Action")),
                )
            except:
                pass
db.commit()
