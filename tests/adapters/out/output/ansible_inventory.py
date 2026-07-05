#!/usr/bin/env python3
"""
Output endpoint: Ansible inventory format.
Receives datasets from one or more sources via stdin.
Produces a combined Ansible inventory JSON on stdout.

Input format (stdin):
{
  "src-inventory": { "hostvars": {...}, "groups": {...} },
  "src-infra": { "hostvars": {...}, "groups": {...} }
}

Output format (stdout): Ansible inventory JSON
{
  "_meta": { "hostvars": { "host1": {...}, ... } },
  "group1": { "hosts": [...], "vars": {...}, "children": [...] },
  ...
}
"""

import json
import os
import sys

config = json.loads(os.environ.get("ENDPOINT_CONFIG", "{}"))
params = json.loads(os.environ.get("ENDPOINT_PARAMS", "{}"))

# Params override config — dynamic beats static
filter_datacenter = params.get("filter_datacenter", config.get("filter_datacenter", ""))
filter_os = params.get("filter_os", config.get("filter_os", ""))
filter_group = params.get("filter_group", config.get("filter_group", ""))
exclude_vars = params.get("exclude_vars", config.get("exclude_vars", "")).split(",") if params.get("exclude_vars", config.get("exclude_vars", "")) else []

datasets = json.load(sys.stdin)

# Merge all sources into a single inventory
merged_hostvars = {}
merged_groups = {}

for source_id, dataset in datasets.items():
    for hostname, vars in dataset.get("hostvars", {}).items():
        if hostname not in merged_hostvars:
            merged_hostvars[hostname] = {}
        # Later sources overwrite earlier ones (enrichment pattern)
        merged_hostvars[hostname].update(vars)

    for group_name, group in dataset.get("groups", {}).items():
        if group_name not in merged_groups:
            merged_groups[group_name] = {
                "hosts": [],
                "children": [],
                "vars": {}
            }
        # Merge hosts (deduplicate)
        existing = set(merged_groups[group_name]["hosts"])
        for h in group.get("hosts", []):
            if h not in existing:
                merged_groups[group_name]["hosts"].append(h)
        # Merge children
        existing_children = set(merged_groups[group_name]["children"])
        for c in group.get("children", []):
            if c not in existing_children:
                merged_groups[group_name]["children"].append(c)
        # Merge vars
        if group.get("vars"):
            merged_groups[group_name]["vars"].update(group["vars"])

# Apply filters
if filter_datacenter:
    merged_hostvars = {
        h: v for h, v in merged_hostvars.items()
        if v.get("datacenter") == filter_datacenter
    }

if filter_os:
    merged_hostvars = {
        h: v for h, v in merged_hostvars.items()
        if v.get("os") == filter_os
    }

if filter_group:
    allowed_hosts = set()
    for g in filter_group.split(","):
        g = g.strip()
        if g in merged_groups:
            allowed_hosts.update(merged_groups[g].get("hosts", []))
    merged_hostvars = {
        h: v for h, v in merged_hostvars.items()
        if h in allowed_hosts
    }

# Strip vars the consumer doesn't want
if exclude_vars:
    for hostname in merged_hostvars:
        for var in exclude_vars:
            merged_hostvars[hostname].pop(var.strip(), None)

# Filter groups to only include hosts that passed the filters
filtered_hosts = set(merged_hostvars.keys())
for group_name in list(merged_groups.keys()):
    merged_groups[group_name]["hosts"] = [
        h for h in merged_groups[group_name]["hosts"]
        if h in filtered_hosts
    ]
    # Remove empty groups
    if not merged_groups[group_name]["hosts"] and not merged_groups[group_name]["children"]:
        del merged_groups[group_name]

# Build Ansible inventory format
inventory = {
    "_meta": {
        "hostvars": merged_hostvars
    }
}

for group_name, group in merged_groups.items():
    entry = {}
    if group["hosts"]:
        entry["hosts"] = sorted(group["hosts"])
    if group["children"]:
        entry["children"] = sorted(group["children"])
    if group["vars"]:
        entry["vars"] = group["vars"]
    inventory[group_name] = entry

json.dump(inventory, sys.stdout, indent=2)
