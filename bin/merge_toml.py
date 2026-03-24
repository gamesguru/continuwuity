#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Created on Mon Mar 23 22:46:39 2026

@author: shane
"""

import json
import re
import tomllib

with open("conduwuit.toml", "rb") as f:
    user_config = tomllib.load(f)

with open("conduwuit-example.toml", "r") as f:
    example_lines = f.readlines()


def format_value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    elif isinstance(v, str):
        return json.dumps(v)
    elif isinstance(v, list):
        if len(v) == 0:
            return "[]"
        # Format arrays cleanly
        contents = ", ".join(json.dumps(i) if isinstance(i, str) else str(i) for i in v)
        return f"[{contents}]"
    elif isinstance(v, (int, float)):
        return str(v)
    elif isinstance(v, dict):
        return json.dumps(
            v
        )  # Fallback to json dictionary syntax which TOML supports for inline tables
    else:
        return str(v)


merged_lines = []
current_sect = "global"

# Apply MSC overwrites explicitly
if "global" not in user_config:
    user_config["global"] = {}
if not isinstance(user_config["global"], dict):
    user_config["global"] = {}
if "experimental_features" not in user_config["global"]:
    user_config["global"]["experimental_features"] = {}
user_config["global"]["experimental_features"]["msc3266_enabled"] = True
user_config["global"]["experimental_features"]["msc4222_enabled"] = True

i = 0
while i < len(example_lines):
    line = example_lines[i]

    # Detect Section
    sect_match = re.match(r"^#?\[([a-zA-Z0-9_\.]+)\]", line)
    if sect_match:
        sect = sect_match.group(1)
        # Check if the user has keys in this section
        parts = sect.split(".")
        curr = user_config
        has_keys = True
        for p in parts:
            if isinstance(curr, dict) and p in curr:
                curr = curr[p]
            else:
                has_keys = False
                break

        if hasattr(curr, "keys") and not curr:
            has_keys = False

        if has_keys:
            merged_lines.append(f"[{sect}]\n")
        else:
            merged_lines.append(line)
        current_sect = sect
        i += 1
        continue

    # Detect Keys
    kv_match = re.match(r"^#?\s*([a-zA-Z0-9_-]+)\s*=", line)
    if kv_match:
        key = kv_match.group(1)

        # check if user explicitly set this key
        parts = current_sect.split(".")
        curr = user_config
        has_val = True
        for p in parts:
            if isinstance(curr, dict) and p in curr:
                curr = curr[p]
            else:
                has_val = False
                break

        val = None
        if has_val and isinstance(curr, dict) and key in curr:
            val = curr[key]
        else:
            has_val = False

        if has_val:
            merged_lines.append(f"{key} = {format_value(val)}\n")
        else:
            merged_lines.append(line)
        i += 1
        continue

    merged_lines.append(line)
    i += 1

with open("conduwuit.toml", "w") as f:
    f.writelines(merged_lines)

print("Merge completed successfully!")
