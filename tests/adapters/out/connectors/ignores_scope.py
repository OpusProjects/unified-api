#!/usr/bin/env python3
"""
A connector that ignores `scope`/`target` and always prints its whole inventory,
exiting 0.

Entirely ordinary: honouring the scope is an optimisation a connector MAY make,
not a duty. The other sample connectors all validate the target and exit
non-zero for a host they do not have, which masks what happens when a scoped
sync succeeds without returning the host it was asked for — the case where the
requested hostname comes back to the caller in a response header.
"""

import json
import sys


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

dump_inventory(
    {"hostvars": {"h1.example": {"os": "linux"}}, "groups": {}},
    sys.stdout,
)
