import sqlite3
import json
import os
import hashlib
import subprocess
import re
import subprocess
import re

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
if os.path.exists("tables.sql"):
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
    # Get the previous run on the same branch
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


def run_cmd(cmd):
    try:
        return subprocess.check_output(cmd, shell=True, text=True).strip()
    except Exception:
        return ""


head_hash = run_cmd("git rev-parse HEAD")
head_short = run_cmd("git rev-parse --short HEAD") or "unknown"
head1_hash = run_cmd("git rev-parse HEAD~1")
head1_short = run_cmd("git rev-parse --short HEAD~1") or "pending"

tags_output = run_cmd("git tag --sort=-creatordate --merged main")
tags = [t for t in tags_output.split("\n") if t.strip() and not ("+" in t or "~" in t)][
    :5
]

rows = []
if head_hash:
    rows.append(f"""  <tr>
    <td valign="top">HEAD ({head_short})</td>
    <td valign="top"><img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fforgejo.ellis.link%2Fgamesguru%2Fcontinuwuity%2Fraw%2Fbranch%2F_metadata%2Fbadges%2Fbadges%2Fforgejo%2Fcommits%2F{head_hash}.json&label=Tests&color=darkgrey" alt="Forgejo HEAD"></td>
    <td valign="top"><a href="https://github.com/gamesguru/continuwuity/actions/workflows/test.yml"><img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fgamesguru%2Fcontinuwuity%2Fbadges%2Fgithub%2Fcommits%2F{head_hash}.json&label=Tests&color=darkgrey" alt="GitHub HEAD"></a></td>
  </tr>""")

if head1_hash:
    rows.append(f"""  <tr>
    <td valign="top">HEAD~1 ({head1_short})</td>
    <td valign="top"><img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fforgejo.ellis.link%2Fgamesguru%2Fcontinuwuity%2Fraw%2Fbranch%2F_metadata%2Fbadges%2Fbadges%2Fforgejo%2Fcommits%2F{head1_hash}.json&label=Tests&color=darkgrey" alt="Forgejo HEAD~1"></td>
    <td valign="top"><a href="https://github.com/gamesguru/continuwuity/actions/workflows/test.yml"><img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fgamesguru%2Fcontinuwuity%2Fbadges%2Fgithub%2Fcommits%2F{head1_hash}.json&label=Tests&color=darkgrey" alt="GitHub HEAD~1"></a></td>
  </tr>""")

for tag in tags:
    tag_hash = run_cmd(f"git rev-list -n 1 {tag}")
    if tag_hash:
        tag_short = tag_hash[:7]
        rows.append(f"""  <tr>
    <td valign="top">{tag} ({tag_short})</td>
    <td valign="top"><img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fforgejo.ellis.link%2Fgamesguru%2Fcontinuwuity%2Fraw%2Fbranch%2F_metadata%2Fbadges%2Fbadges%2Fforgejo%2Fcommits%2F{tag_hash}.json&label=Tests&color=darkgrey" alt="Forgejo {tag}"></td>
    <td valign="top"><a href="https://github.com/gamesguru/continuwuity/actions/workflows/test.yml"><img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fgamesguru%2Fcontinuwuity%2Fbadges%2Fgithub%2Fcommits%2F{tag_hash}.json&label=Tests&color=darkgrey" alt="GitHub {tag}"></a></td>
  </tr>""")

readme_path = "../../README.md"
if os.path.exists(readme_path):
    with open(readme_path, "r") as f:
        readme_content = f.read()

    new_table = f"""<table border="0">
  <tr>
    <td valign="top"><b>Version</b></td>
    <td valign="top"><b>Forgejo</b></td>
    <td valign="top"><b>GitHub</b></td>
  </tr>
{chr(10).join(rows)}
</table>"""

    table_start = readme_content.find('<table border="0">')
    table_end_len = len("</table>")
    table_end = readme_content.find("</table>", table_start)

    updated_readme = (
        readme_content[:table_start]
        + new_table
        + readme_content[table_end + table_end_len :]
    )
    with open(readme_path, "w") as f:
        f.write(updated_readme)
