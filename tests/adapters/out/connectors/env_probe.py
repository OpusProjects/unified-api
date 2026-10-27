#!/usr/bin/env python3
"""
Connector fixture that reports the environment it was given, so a test can
assert what the adapter does — and does not — pass to a script.

Prints a native Dataset whose single host's vars are the script's environment.
"""
import json
import os


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

print(inventory_json({"hostvars": {"probe": dict(os.environ)}}))
