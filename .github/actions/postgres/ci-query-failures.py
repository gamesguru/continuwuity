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
sha = "all"
limit = "15"
order = "run_date DESC, n_pass DESC"

# Extract sha
sha_match = re.search(r"sha=([^\s]+)", args_str)
if sha_match:
    sha = sha_match.group(1)
    args_str = args_str.replace(sha_match.group(0), "")

# Extract limit
limit_match = re.search(r"limit=([0-9]+)", args_str)
if limit_match:
    limit = limit_match.group(1)
    args_str = args_str.replace(limit_match.group(0), "")

# Extract order (it takes whatever is left if it starts with order=)
order_match = re.search(r"order=(.+?)(?:$| sha=| limit=)", args_str + " ")
if order_match:
    order = order_match.group(1).strip()

base_query = f"""
SELECT
    version_string,
    to_char(run_date AT TIME ZONE '{tz_sql}', 'YYYY-MM-DD HH24:MI:SS') AS run_date,
    (n_pass + n_skip + n_fail) AS n_total,
    n_pass,
    n_skip,
    n_fail,
    new_pass,
    new_fail,
    profile,
    regexp_replace(features, '[, ]+', E'\n', 'g') AS features,
    os,
    arch,
    new_failures_list
FROM
    v_run_regressions"""

if sha == "all":
    query = f"{base_query}\nORDER BY\n    {order}\nLIMIT {limit}"
else:
    query = f"{base_query}\nWHERE\n    commit_hash LIKE '{sha}%'\n    OR upstream_sha LIKE '{sha}%'\nORDER BY\n    {order}\nLIMIT {limit}"

print(f"\nExecuting Query:\n{query}\n")

# Execute the db-shell script with the query
env = os.environ.copy()
env["PAGER"] = "less -X -F -S"
subprocess.run(["./bin/db-shell", "-c", query], env=env)
