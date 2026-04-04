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
order_match = re.search(r"order=(.+?)(?:$| like=| limit=| new_passes=)", args_str + " ", re.IGNORECASE)
if order_match:
    order = order_match.group(1).strip()

if new_passes:
    columns_tail = "new_failures_list,\n    new_passes_list"
else:
    columns_tail = "new_failures_list"

if baseline:
    # A specific commit/branch was requested as the baseline
    baseline_run_filter = f"(b.commit_hash LIKE '{baseline}%' OR b.version_string LIKE '%{baseline}%' OR b.branch LIKE '%{baseline}%' OR b.id::text = '{baseline}')"
else:
    # Default to recent main/upstream
    baseline_run_filter = "(b.branch IN ('main', 'main-upstream', 'refs/heads/main', 'refs/heads/main-upstream') OR b.version_string LIKE '%main%')"

sql_file_path = os.path.join(os.path.dirname(__file__), "queries.sql")
with open(sql_file_path, "r") as f:
    base_query_template = f.read()

base_query = base_query_template.format(
    baseline_run_filter=baseline_run_filter,
    tz_sql=tz_sql,
    columns_tail=columns_tail
)

if like_str == "all":
    query = f"{base_query}\nORDER BY\n    {order}\nLIMIT {limit}"
else:
    query = f"{base_query}\nWHERE\n    version_string LIKE '%{like_str}%'\nORDER BY\n    {order}\nLIMIT {limit}"

print(f"\nExecuting Query:\n{query}\n")

# Execute the db-shell script with the query
env = os.environ.copy()
env["PAGER"] = env.get("PAGER") or "less -X -F -S"
subprocess.run(["./bin/db-shell", "-c", query], env=env)
