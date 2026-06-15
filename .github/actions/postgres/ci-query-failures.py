#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Created on Wed Mar 18 19:20:48 2026

@author: shane
"""

import os
import re
import subprocess
import sys
import time

args_str = " ".join(sys.argv[1:])

# Get the local machine's timezone offset (e.g. "-04:00") to send to Postgres over SSH
tz_raw = time.strftime("%z")
if tz_raw:
    # Postgres expects POSIX offsets where West of UTC is positive.
    # Python's %z is ISO 8601 (West is negative). So we invert the sign.
    sign = "+" if tz_raw[0] == "-" else "-"
    tz_sql = f"{sign}{tz_raw[1:3]}:{tz_raw[3:]}"
else:
    tz_sql = "+00:00"

# Defaults
like_str = "all"
limit = "15"
order = "run_date DESC, n_pass DESC"

# Extract like
like_match = re.search(r"like=([^\s]+)", args_str)
if like_match:
    like_str = like_match.group(1)
    args_str = args_str.replace(like_match.group(0), "")

# Extract limit
limit_match = re.search(r"limit=([0-9]+)", args_str)
if limit_match:
    limit = limit_match.group(1)
    args_str = args_str.replace(limit_match.group(0), "")

# Extract baseline (can be a branch, commit hash, or run ID)
baseline = None
baseline_match = re.search(r"baseline=([a-zA-Z0-9_\-\.]+)", args_str)
if baseline_match:
    baseline = baseline_match.group(1)
    args_str = args_str.replace(baseline_match.group(0), "")

# Extract --verbose flag
verbose = "--verbose" in args_str or "-v" in args_str.split()
args_str = args_str.replace("--verbose", "").replace(" -v ", " ")

# Extract new_passes (Must be done before order parsing to avoid greedy capture)
new_passes = False
new_passes_match = re.search(r"new_passes=([^\s]*)", args_str, re.IGNORECASE)
if new_passes_match:
    val = new_passes_match.group(1).strip().lower()
    if val in ("1", "true", "yes"):
        new_passes = True
    elif val in ("0", "false", "no", ""):
        new_passes = False
    args_str = args_str.replace(new_passes_match.group(0), "")

# Extract order (it takes whatever is left if it starts with order=)
order_match = re.search(
    r"order=(.+?)(?:$| like=| limit=| new_passes=)", args_str + " ", re.IGNORECASE
)
if order_match:
    order = order_match.group(1).strip()

# Build columns_tail based on flags
cols = ["new_failures_list"]
if verbose:
    cols.append("date_last_passed")
if new_passes:
    cols.append("new_passes_list")
# Global query uses 'a.' prefix (run_agg alias), baseline uses bare names
columns_tail = ",\n    ".join(f"a.{c}" for c in cols)

if like_str == "all":
    like_filter = ""
else:
    like_filter = f"AND version_string LIKE '%{like_str}%'"

# Pick query template based on whether a baseline was specified
script_dir = os.path.dirname(__file__)
if baseline:
    # Custom baseline: compare against a specific commit
    baseline_run_filter = (
        f"(b.commit_hash LIKE '{baseline}%'"
        f" OR b.version_string LIKE '%{baseline}%'"
        f" OR b.branch LIKE '%{baseline}%'"
        f" OR b.id::text = '{baseline}')"
    )
    sql_file_path = os.path.join(script_dir, "queries.sql")
    with open(sql_file_path, "r", encoding="utf-8") as f:
        base_query_template = f.read()
    query = base_query_template.format(
        baseline_run_filter=baseline_run_filter,
        tz_sql=tz_sql,
        columns_tail=columns_tail,
        order=order,
        limit=limit,
        like_filter=like_filter,
    )
else:
    # Global baseline: uses mv_ever_passed matview (fast, catches all regressions)
    sql_file_path = os.path.join(script_dir, "queries_global.sql")
    with open(sql_file_path, "r", encoding="utf-8") as f:
        base_query_template = f.read()
    query = base_query_template.format(
        tz_sql=tz_sql,
        columns_tail=columns_tail,
        order=order,
        limit=limit,
        like_filter=like_filter,
    )

print(f"\nExecuting Query:\n{query}\n")

env = os.environ.copy()
env["PAGER"] = env.get("PAGER") or "less -X -F -S"

try:
    subprocess.run(["./bin/db-shell", "-c", query], env=env, check=False)
except KeyboardInterrupt as exc:
    raise SystemExit(130) from exc
finally:
    if sys.stdin.isatty():
        os.system("stty sane 2>/dev/null")
