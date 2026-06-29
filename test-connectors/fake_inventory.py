#!/usr/bin/env python3
"""
Fake connector that simulates a Device42-like inventory source.
Reads config from environment variables, outputs Ansible inventory JSON to stdout.

This is exactly how a real connector would work:
- Receives SOURCE_CONFIG (JSON) and CREDENTIAL_* env vars
- Outputs Ansible inventory JSON to stdout
- Exit 0 = success, exit non-zero = failure
"""
import json
import os
import sys

# Read config from env (the unified-api will inject these)
config_raw = os.environ.get("SOURCE_CONFIG", "{}")
config = json.loads(config_raw)

# Simulate different scenarios based on config
scenario = config.get("scenario", "default")

if scenario == "empty":
    inventory = {
        "hostvars": {},
        "groups": {}
    }

elif scenario == "error":
    print("ERROR: Could not connect to inventory API", file=sys.stderr)
    sys.exit(1)

elif scenario == "large":
    # Generate a larger inventory (50 hosts across two datacenters)
    hostvars = {}
    section9_hosts = []
    seele_hosts = []
    for i in range(1, 26):
        # Section 9 datacenter — Ghost in the Shell
        s9_host = f"tachikoma{i:03d}.section9.net"
        hostvars[s9_host] = {
            "ansible_host": f"10.9.1.{i}",
            "os": "OracleLinux",
            "os_version": "8.9",
            "datacenter": "section9",
            "ram_gb": 64,
            "cpu_count": 8
        }
        section9_hosts.append(s9_host)

        # SEELE datacenter — Evangelion
        seele_host = f"magi{i:03d}.seele.net"
        hostvars[seele_host] = {
            "ansible_host": f"10.6.1.{i}",
            "os": "OracleLinux",
            "os_version": "9.3",
            "datacenter": "seele",
            "ram_gb": 128,
            "cpu_count": 16
        }
        seele_hosts.append(seele_host)

    inventory = {
        "hostvars": hostvars,
        "groups": {
            "section9": {
                "hosts": section9_hosts,
                "vars": {"ntp_server": "ntp.section9.net"}
            },
            "seele": {
                "hosts": seele_hosts,
                "vars": {"ntp_server": "ntp.seele.net"}
            },
            "oraclelinux8": {
                "hosts": section9_hosts
            },
            "oraclelinux9": {
                "hosts": seele_hosts
            },
            "production": {
                "hosts": section9_hosts
            },
            "staging": {
                "hosts": seele_hosts
            }
        }
    }

else:
    # Default: small realistic inventory across both datacenters
    inventory = {
        "hostvars": {
            "motoko.section9.net": {
                "ansible_host": "10.9.1.1",
                "os": "OracleLinux",
                "os_version": "9.3",
                "datacenter": "section9",
                "ram_gb": 128,
                "cpu_count": 16,
                "role": "commander"
            },
            "batou.section9.net": {
                "ansible_host": "10.9.1.2",
                "os": "OracleLinux",
                "os_version": "8.9",
                "datacenter": "section9",
                "ram_gb": 64,
                "cpu_count": 8,
                "role": "assault"
            },
            "tachikoma01.section9.net": {
                "ansible_host": "10.9.1.10",
                "os": "OracleLinux",
                "os_version": "8.9",
                "datacenter": "section9",
                "ram_gb": 32,
                "cpu_count": 4,
                "role": "think-tank"
            },
            "melchior.seele.net": {
                "ansible_host": "10.6.1.1",
                "os": "OracleLinux",
                "os_version": "9.3",
                "datacenter": "seele",
                "ram_gb": 256,
                "cpu_count": 32,
                "role": "magi-system"
            },
            "balthasar.seele.net": {
                "ansible_host": "10.6.1.2",
                "os": "OracleLinux",
                "os_version": "9.3",
                "datacenter": "seele",
                "ram_gb": 256,
                "cpu_count": 32,
                "role": "magi-system"
            },
            "casper.seele.net": {
                "ansible_host": "10.6.1.3",
                "os": "OracleLinux",
                "os_version": "9.3",
                "datacenter": "seele",
                "ram_gb": 256,
                "cpu_count": 32,
                "role": "magi-system"
            }
        },
        "groups": {
            "section9": {
                "hosts": [
                    "motoko.section9.net",
                    "batou.section9.net",
                    "tachikoma01.section9.net"
                ],
                "vars": {"ntp_server": "ntp.section9.net"}
            },
            "seele": {
                "hosts": [
                    "melchior.seele.net",
                    "balthasar.seele.net",
                    "casper.seele.net"
                ],
                "vars": {"ntp_server": "ntp.seele.net"}
            },
            "magi": {
                "hosts": [
                    "melchior.seele.net",
                    "balthasar.seele.net",
                    "casper.seele.net"
                ],
                "children": ["seele"]
            },
            "oraclelinux9": {
                "hosts": [
                    "motoko.section9.net",
                    "melchior.seele.net",
                    "balthasar.seele.net",
                    "casper.seele.net"
                ]
            },
            "oraclelinux8": {
                "hosts": [
                    "batou.section9.net",
                    "tachikoma01.section9.net"
                ]
            },
            "production": {
                "hosts": [
                    "motoko.section9.net",
                    "batou.section9.net",
                    "melchior.seele.net",
                    "balthasar.seele.net",
                    "casper.seele.net"
                ]
            },
            "staging": {
                "hosts": ["tachikoma01.section9.net"]
            }
        }
    }

# Scope filtering: the unified-api passes scope and target via SOURCE_CONFIG
# to request only a subset of the inventory
scope = config.get("scope", "full")
target = config.get("target", "")

if scope == "host" and target:
    # Return only one host's vars
    if target in inventory.get("hostvars", {}):
        inventory = {
            "hostvars": {target: inventory["hostvars"][target]},
            "groups": {}
        }
    else:
        print(f"ERROR: Host '{target}' not found", file=sys.stderr)
        sys.exit(1)

elif scope == "group" and target:
    # Return only hosts belonging to the target group
    group = inventory.get("groups", {}).get(target)
    if group:
        group_hosts = group.get("hosts", [])
        filtered_hostvars = {
            h: inventory["hostvars"][h]
            for h in group_hosts
            if h in inventory.get("hostvars", {})
        }
        inventory = {
            "hostvars": filtered_hostvars,
            "groups": {target: group}
        }
    else:
        print(f"ERROR: Group '{target}' not found", file=sys.stderr)
        sys.exit(1)

# Output to stdout — this is what the unified-api captures
json.dump(inventory, sys.stdout, indent=2)
