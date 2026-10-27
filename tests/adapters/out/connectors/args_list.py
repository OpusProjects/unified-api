#!/usr/bin/env python3
"""
Connector fixture that mimics an Ansible dynamic inventory script:
it REQUIRES --list on the command line (like argparse-based scripts such
as real Device42/VMware inventories) and fails with exit code 2 otherwise.

With --list it emits Ansible inventory JSON whose hostvars record the
arguments it received, so tests can assert they arrived verbatim.
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

args = sys.argv[1:]

if "--list" not in args:
    print("usage: args_list.py --list", file=sys.stderr)
    sys.exit(2)

print(
    inventory_json(
        {
            "hostvars": {
                "argshost.section9.net": {"received_args": args},
            },
            "groups": {"args": {"hosts": ["argshost.section9.net"]}},
        }
    )
)
