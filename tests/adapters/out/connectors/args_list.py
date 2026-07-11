#!/usr/bin/env python3
"""
Connector fixture that mimics an Ansible dynamic inventory script:
it REQUIRES --list on the command line (like argparse-based scripts such
as real Device42/VMware inventories) and fails with exit code 2 otherwise.

With --list it emits a Dataset-format JSON whose hostvars record the
arguments it received, so tests can assert they arrived verbatim.
"""

import json
import sys

args = sys.argv[1:]

if "--list" not in args:
    print("usage: args_list.py --list", file=sys.stderr)
    sys.exit(2)

print(
    json.dumps(
        {
            "hostvars": {
                "argshost.section9.net": {"received_args": args},
            },
            "groups": {"args": {"hosts": ["argshost.section9.net"]}},
        }
    )
)
