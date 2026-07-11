#!/usr/bin/env python3
"""
Connector fixture that behaves like a real Ansible dynamic inventory script
(the d42/VMware kind): requires --list and prints standard Ansible inventory
JSON — hostvars under _meta, groups as top-level keys, one group in the
legacy list form, plus the implicit all/ungrouped meta-groups.
"""

import json
import sys

if "--list" not in sys.argv[1:]:
    print("usage: ansible_inventory_source.py --list", file=sys.stderr)
    sys.exit(2)

print(
    json.dumps(
        {
            "_meta": {
                "hostvars": {
                    "motoko.section9.net": {"ansible_host": "10.9.1.1", "os": "OracleLinux"},
                    "batou.section9.net": {"ansible_host": "10.9.1.2", "os": "OracleLinux"},
                }
            },
            "all": {"children": ["section9", "legacy", "ungrouped"]},
            "ungrouped": {"hosts": []},
            "section9": {
                "hosts": ["motoko.section9.net", "batou.section9.net"],
                "vars": {"ntp_server": "ntp.section9.net"},
            },
            "legacy": ["motoko.section9.net"],
        }
    )
)
