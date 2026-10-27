#!/usr/bin/env python3
"""Sample connector that takes too long — used to test execution timeouts."""
import json
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

time.sleep(10)
print(inventory_json({"hostvars": {"slow.example.net": {"ansible_host": "10.0.0.1"}}, "groups": {}}))
