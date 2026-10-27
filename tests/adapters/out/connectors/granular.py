#!/usr/bin/env python3
"""
Sample connector that can gather ONE host without gathering the inventory.

Two modes, both printing the native Dataset shape:

  --list                   the whole inventory (motoko and batou, both in
                           "legacy")
  --only-host <hostnames>  just those hosts, with the groups they are really
                           in ("modern", described in full)

The second mode is the point: Ansible's own `--host` has no room for group
membership, so a script that can answer "this host and where it belongs"
states its own flag and the source names it in `host_args`.

Each host carries `called_with` so a test can prove which mode ran.
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

INVENTORY = {
    "hostvars": {
        "motoko.section9.net": {"role": "commander"},
        "batou.section9.net": {"role": "ranger"},
    },
    "groups": {
        "legacy": {
            "hosts": ["motoko.section9.net", "batou.section9.net"],
            "vars": {"tier": "bronze"},
        },
    },
}

# Where the granular mode says the hosts really are — a different group from
# the one the full listing knows about, so a test can tell them apart.
MODERN = {
    "hosts": [],
    "vars": {"tier": "gold", "patch_window": "sunday"},
}

args = sys.argv[1:]
called_with = " ".join(args)

if "--only-host" in args:
    wanted = args[args.index("--only-host") + 1].split(",")
    known = [h for h in wanted if h in INVENTORY["hostvars"]]
    if not known:
        print(f"ERROR: no such host: {wanted}", file=sys.stderr)
        sys.exit(1)

    hostvars = {}
    for host in known:
        hostvars[host] = dict(INVENTORY["hostvars"][host])
        hostvars[host]["called_with"] = called_with

    modern = dict(MODERN)
    modern["hosts"] = known
    print(inventory_json({"hostvars": hostvars, "groups": {"modern": modern}}))
    sys.exit(0)

hostvars = {}
for host, vars in INVENTORY["hostvars"].items():
    hostvars[host] = dict(vars)
    hostvars[host]["called_with"] = called_with

print(inventory_json({"hostvars": hostvars, "groups": INVENTORY["groups"]}))
