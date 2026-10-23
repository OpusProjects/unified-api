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

# Params override config — dynamic beats static. The same names as the
# builtin transformers; this sample implements only `key=value` conditions in
# `filter`, which is all the tests need.
def setting(name):
    return params.get(name, config.get(name, ""))

filter_conditions = [c.split("=", 1) for c in setting("filter").split(";") if "=" in c]
group_filter = setting("group")
exclude_fields = [f for f in setting("exclude_fields").split(",") if f.strip()]

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
for key, value in filter_conditions:
    merged_hostvars = {
        h: v for h, v in merged_hostvars.items()
        if str(v.get(key.strip(), "")) == value.strip()
    }

if group_filter:
    allowed_hosts = set()
    for g in group_filter.split(","):
        g = g.strip()
        if g in merged_groups:
            allowed_hosts.update(merged_groups[g].get("hosts", []))
    merged_hostvars = {
        h: v for h, v in merged_hostvars.items()
        if h in allowed_hosts
    }

# Strip vars the consumer doesn't want
if exclude_fields:
    for hostname in merged_hostvars:
        for var in exclude_fields:
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
