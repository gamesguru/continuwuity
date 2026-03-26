#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Enhanced CI Run Regression Query Script
Allows comparing against different baselines (branches) and showing new passes.
"""

import os
import re
import subprocess
import sys
import time

# Parse command line arguments
args_str = " ".join(sys.argv[1:])

# Get the local machine's timezone offset (e.g. "-04:00")
tz_raw = time.strftime("%z")
if tz_raw:
    sign = "+" if tz_raw[0] == "-" else "-"
    tz_sql = f"{sign}{tz_raw[1:3]}:{tz_raw[3:]}"
else:
    tz_sql = "+00:00"

# Defaults
like_str = "all"
limit = "15"
order = "run_date DESC, n_pass DESC"
show_passes = False
baseline_branch = None
raw_mode = False

# Extract options from key=value pairs
for arg in sys.argv[1:]:
    if "=" in arg:
        k, v = arg.split("=", 1)
        if k == "like":
            like_str = v
        elif k == "limit":
            # Sanitize limit: digits only
            if v.isdigit():
                limit = v
        elif k == "order":
            # Sanitize order: allow only safe characters/words
            if re.match(r"^[a-zA-Z0-9_,. ]+$", v):
                order = v
        elif k == "baseline":
            # Escape baseline: replace single quotes
            baseline_branch = v.replace("'", "''")
        elif k == "show" and "passes" in v:
            show_passes = True
        elif k == "raw":
            raw_mode = v.lower() in ("1", "true", "yes")
    elif arg == "passes":
        show_passes = True

# Construct the baseline logic
baseline_table = "master_baseline"
baseline_cte = ""
if baseline_branch:
    baseline_cte = f"""
WITH custom_baseline AS (
    SELECT test_name, status
    FROM run_details
    WHERE run_id = (
        SELECT id FROM runs
        WHERE branch = '{baseline_branch}'
        ORDER BY run_date DESC LIMIT 1
    )
)"""
    baseline_table = "custom_baseline"

# Optional columns
passes_col = ""
passes_agg = ""
if show_passes:
    # In raw mode, we use #01 as a newline placeholder to keep psql output single-line per record.
    # In normal mode, we use E'\n' for readability.
    nl = "#01" if raw_mode else "\n"
    passes_col = f",\n    counts.new_passes_list"
    passes_agg = f",\n        string_agg(rd.test_name, E'{nl}') FILTER (WHERE rd.status = 'pass'::text AND (mb.status IS NULL OR mb.status <> 'pass'::text)) AS new_passes_list"

# Newline placeholder for failures
nl_fail = "#01" if raw_mode else "\n"

# In raw mode, ensure features is also single-line
features_sql = "REPLACE(r.features, E'\n', '#01')" if raw_mode else "regexp_replace(r.features, '[ ,]+', E'\n', 'g')"

query = f"""{baseline_cte}
SELECT
    r.version_string,
    to_char(r.run_date AT TIME ZONE '{tz_sql}', 'YYYY-MM-DD HH24:MI:SS') AS run_date,
    (r.n_pass + r.n_skip + r.n_fail) AS n_total,
    r.n_pass,
    r.n_skip,
    r.n_fail,
    counts.new_pass,
    counts.new_fail,
    r.profile,
    {features_sql} AS features,
    r.os,
    r.arch,
    counts.new_failures_list{passes_col}
FROM runs r
LEFT JOIN LATERAL (
    SELECT
        count(*) AS run_total,
        count(*) FILTER (WHERE rd.status = 'pass'::text AND (mb.status IS NULL OR mb.status <> 'pass'::text)) AS new_pass,
        count(*) FILTER (WHERE rd.status = 'fail'::text AND (mb.status IS NULL OR mb.status <> 'fail'::text)) AS new_fail,
        string_agg(rd.test_name, E'{nl_fail}') FILTER (WHERE rd.status = 'fail'::text AND (mb.status IS NULL OR mb.status <> 'fail'::text)) AS new_failures_list{passes_agg}
    FROM run_details rd
    LEFT JOIN {baseline_table} mb ON mb.test_name = rd.test_name
    WHERE rd.run_id = r.id
) counts ON true
WHERE r.n_pass > 0 AND counts.run_total > 0"""

if like_str != "all":
    # Sanitize like_str
    safe_like = like_str.replace("'", "''")
    query += f"\nAND r.version_string LIKE '%{safe_like}%'"

query += f"\nORDER BY\n    {order}\nLIMIT {limit};"

if os.environ.get("DEBUG") and not raw_mode:
    print(f"\nExecuting Query:\n{query}\n")

# Execute the db-shell script with the query
env = os.environ.copy()
env["PAGER"] = env.get("PAGER") or "less -X -F -S"

cmd = ["./bin/db-shell"]
if raw_mode:
    cmd += ["-A", "-t"]
cmd += ["-c", query]

subprocess.run(cmd, env=env)
