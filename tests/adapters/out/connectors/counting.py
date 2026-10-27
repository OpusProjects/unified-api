#!/usr/bin/env python3
"""
Connector that records every invocation, so a test can count how many gathers
actually happened rather than inferring it from the data.

Config keys it understands (all optional):
  counter_file    append one "<scope>:<target>" line per run
  delay_seconds   sleep before answering, to make concurrent requests overlap
  fail            "true" = exit non-zero, to exercise a failed refresh
"""

import json
import os
import sys
import time


# What a connector prints: Ansible dynamic inventory JSON, with host variables
# under _meta.hostvars and each group a top-level key. The inventory is built
# in the {"hostvars", "groups"} shape, which is easier to filter, and
# converted on the way out.
def to_ansible(inventory):
    return {"_meta": {"hostvars": inventory.get("hostvars", {})}, **inventory.get("groups", {})}


def dump_inventory(inventory, out, **kwargs):
    json.dump(to_ansible(inventory), out, **kwargs)


def inventory_json(inventory, **kwargs):
    return json.dumps(to_ansible(inventory), **kwargs)

config = json.loads(os.environ.get("SOURCE_CONFIG", "{}"))
scope = config.get("scope", "full")
target = config.get("target", "")

counter_file = config.get("counter_file")
if counter_file:
    with open(counter_file, "a") as handle:
        handle.write(f"{scope}:{target}\n")

delay = float(config.get("delay_seconds", 0))
if delay:
    time.sleep(delay)

if config.get("fail") == "true":
    print("simulated gather failure", file=sys.stderr)
    sys.exit(1)

HOSTS = {
    "h1.example": {"os": "linux", "gathered": True},
    "h2.example": {"os": "linux", "gathered": True},
}

# The target is a comma-separated list, like the API's ?host=
if scope == "host":
    wanted = [h.strip() for h in target.split(",") if h.strip()]
    known = [h for h in wanted if h in HOSTS]
    if not known:
        print(f"host '{target}' not found", file=sys.stderr)
        sys.exit(1)
    dump_inventory({"hostvars": {h: HOSTS[h] for h in known}, "groups": {}}, sys.stdout)
else:
    dump_inventory({"hostvars": HOSTS, "groups": {}}, sys.stdout)
