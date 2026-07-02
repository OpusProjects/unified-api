#!/usr/bin/env python3
"""
Fake enricher for testing.
Reads the current dataset from stdin, adds an "enriched" field to each host,
and optionally removes hosts listed in REMOVE_HOSTS config.
"""

import json
import os
import sys

dataset = json.load(sys.stdin)

config = json.loads(os.environ.get("SOURCE_CONFIG", "{}"))

result = {"hostvars": {}, "groups": {}, "remove_hosts": []}

# Enrich: add a field to every host
for hostname, vars in dataset.get("hostvars", {}).items():
    result["hostvars"][hostname] = {
        **vars,
        "enriched": True,
        "enricher_version": "1.0",
    }

# If config says to remove specific hosts, do it
remove_hosts = config.get("remove_hosts", "")
if remove_hosts:
    result["remove_hosts"] = remove_hosts.split(",")
    # Don't include removed hosts in hostvars
    for h in result["remove_hosts"]:
        result["hostvars"].pop(h, None)

print(json.dumps(result, indent=2), file=sys.stderr)
json.dump(result, sys.stdout)
