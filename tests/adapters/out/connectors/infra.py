#!/usr/bin/env python3
"""
Sample infrastructure data collector.
Simulates SSH-based collection of: filesystems, CPU, memory, sysctl, users.
In production this would SSH into each host and run commands.
"""

import json
import os
import sys

config = json.loads(os.environ.get("SOURCE_CONFIG", "{}"))
scenario = config.get("scenario", "default")

scope = config.get("scope", "")
target = config.get("target", "")

HOSTS = {
    "motoko.section9.net": {
        "ansible_host": "10.9.1.1",
        "datacenter": "section9",
        "os": "OracleLinux",
        "os_version": "8.9",
        "cpu": {
            "model": "Intel Xeon E5-2680 v4",
            "cores": 8,
            "threads": 16,
            "architecture": "x86_64"
        },
        "memory": {
            "total_mb": 32768,
            "swap_mb": 4096
        },
        "filesystems": [
            {"mountpoint": "/", "device": "/dev/sda2", "fstype": "xfs", "size_gb": 50, "used_gb": 12},
            {"mountpoint": "/home", "device": "/dev/sda3", "fstype": "xfs", "size_gb": 100, "used_gb": 34},
            {"mountpoint": "/var", "device": "/dev/sda4", "fstype": "xfs", "size_gb": 80, "used_gb": 45},
            {"mountpoint": "/opt/app", "device": "/dev/sdb1", "fstype": "ext4", "size_gb": 200, "used_gb": 89}
        ],
        "sysctl": {
            "net.ipv4.ip_forward": "1",
            "vm.swappiness": "10",
            "net.core.somaxconn": "65535",
            "fs.file-max": "2097152",
            "kernel.pid_max": "65536"
        },
        "users": [
            {"name": "root", "uid": 0, "shell": "/bin/bash"},
            {"name": "ansible", "uid": 1000, "shell": "/bin/bash"},
            {"name": "app_user", "uid": 1001, "shell": "/bin/bash"},
            {"name": "monitoring", "uid": 1002, "shell": "/sbin/nologin"}
        ]
    },
    "batou.section9.net": {
        "ansible_host": "10.9.1.2",
        "datacenter": "section9",
        "os": "OracleLinux",
        "os_version": "8.9",
        "cpu": {
            "model": "Intel Xeon E5-2680 v4",
            "cores": 4,
            "threads": 8,
            "architecture": "x86_64"
        },
        "memory": {
            "total_mb": 16384,
            "swap_mb": 2048
        },
        "filesystems": [
            {"mountpoint": "/", "device": "/dev/sda2", "fstype": "xfs", "size_gb": 50, "used_gb": 18},
            {"mountpoint": "/var/log", "device": "/dev/sda3", "fstype": "xfs", "size_gb": 40, "used_gb": 31}
        ],
        "sysctl": {
            "net.ipv4.ip_forward": "0",
            "vm.swappiness": "30",
            "net.core.somaxconn": "128",
            "fs.file-max": "1048576"
        },
        "users": [
            {"name": "root", "uid": 0, "shell": "/bin/bash"},
            {"name": "ansible", "uid": 1000, "shell": "/bin/bash"}
        ]
    },
    "tachikoma.section9.net": {
        "ansible_host": "10.9.1.3",
        "datacenter": "section9",
        "os": "Ubuntu",
        "os_version": "22.04",
        "cpu": {
            "model": "AMD EPYC 7543",
            "cores": 2,
            "threads": 4,
            "architecture": "x86_64"
        },
        "memory": {
            "total_mb": 8192,
            "swap_mb": 1024
        },
        "filesystems": [
            {"mountpoint": "/", "device": "/dev/vda1", "fstype": "ext4", "size_gb": 30, "used_gb": 9}
        ],
        "sysctl": {
            "net.ipv4.ip_forward": "1",
            "vm.swappiness": "60"
        },
        "users": [
            {"name": "root", "uid": 0, "shell": "/bin/bash"},
            {"name": "ubuntu", "uid": 1000, "shell": "/bin/bash"},
            {"name": "ansible", "uid": 1001, "shell": "/bin/bash"}
        ]
    },
    "melchior.seele.net": {
        "ansible_host": "10.6.1.1",
        "datacenter": "seele",
        "os": "OracleLinux",
        "os_version": "9.3",
        "cpu": {
            "model": "Intel Xeon Gold 6348",
            "cores": 16,
            "threads": 32,
            "architecture": "x86_64"
        },
        "memory": {
            "total_mb": 65536,
            "swap_mb": 8192
        },
        "filesystems": [
            {"mountpoint": "/", "device": "/dev/sda2", "fstype": "xfs", "size_gb": 100, "used_gb": 22},
            {"mountpoint": "/opt/oracle", "device": "/dev/sdb1", "fstype": "xfs", "size_gb": 500, "used_gb": 312},
            {"mountpoint": "/backup", "device": "/dev/sdc1", "fstype": "xfs", "size_gb": 1000, "used_gb": 678}
        ],
        "sysctl": {
            "net.ipv4.ip_forward": "0",
            "vm.swappiness": "1",
            "kernel.shmmax": "68719476736",
            "kernel.shmall": "4294967296",
            "fs.file-max": "6815744",
            "net.core.rmem_max": "4194304",
            "net.core.wmem_max": "1048576"
        },
        "users": [
            {"name": "root", "uid": 0, "shell": "/bin/bash"},
            {"name": "ansible", "uid": 1000, "shell": "/bin/bash"},
            {"name": "oracle", "uid": 1001, "shell": "/bin/bash"},
            {"name": "grid", "uid": 1002, "shell": "/bin/bash"},
            {"name": "monitoring", "uid": 1003, "shell": "/sbin/nologin"}
        ]
    },
    "balthasar.seele.net": {
        "ansible_host": "10.6.1.2",
        "datacenter": "seele",
        "os": "OracleLinux",
        "os_version": "9.3",
        "cpu": {
            "model": "Intel Xeon Gold 6348",
            "cores": 16,
            "threads": 32,
            "architecture": "x86_64"
        },
        "memory": {
            "total_mb": 65536,
            "swap_mb": 8192
        },
        "filesystems": [
            {"mountpoint": "/", "device": "/dev/sda2", "fstype": "xfs", "size_gb": 100, "used_gb": 19},
            {"mountpoint": "/opt/oracle", "device": "/dev/sdb1", "fstype": "xfs", "size_gb": 500, "used_gb": 287}
        ],
        "sysctl": {
            "net.ipv4.ip_forward": "0",
            "vm.swappiness": "1",
            "kernel.shmmax": "68719476736",
            "kernel.shmall": "4294967296",
            "fs.file-max": "6815744"
        },
        "users": [
            {"name": "root", "uid": 0, "shell": "/bin/bash"},
            {"name": "ansible", "uid": 1000, "shell": "/bin/bash"},
            {"name": "oracle", "uid": 1001, "shell": "/bin/bash"},
            {"name": "grid", "uid": 1002, "shell": "/bin/bash"}
        ]
    },
    "casper.seele.net": {
        "ansible_host": "10.6.1.3",
        "datacenter": "seele",
        "os": "OracleLinux",
        "os_version": "9.3",
        "cpu": {
            "model": "Intel Xeon Gold 6348",
            "cores": 8,
            "threads": 16,
            "architecture": "x86_64"
        },
        "memory": {
            "total_mb": 32768,
            "swap_mb": 4096
        },
        "filesystems": [
            {"mountpoint": "/", "device": "/dev/sda2", "fstype": "xfs", "size_gb": 100, "used_gb": 15},
            {"mountpoint": "/opt/oracle", "device": "/dev/sdb1", "fstype": "xfs", "size_gb": 300, "used_gb": 156}
        ],
        "sysctl": {
            "net.ipv4.ip_forward": "0",
            "vm.swappiness": "1",
            "kernel.shmmax": "34359738368",
            "fs.file-max": "6815744"
        },
        "users": [
            {"name": "root", "uid": 0, "shell": "/bin/bash"},
            {"name": "ansible", "uid": 1000, "shell": "/bin/bash"},
            {"name": "oracle", "uid": 1001, "shell": "/bin/bash"}
        ]
    }
}

GROUPS = {
    "section9": {
        "hosts": ["motoko.section9.net", "batou.section9.net", "tachikoma.section9.net"],
        "vars": {"ntp_server": "ntp.section9.net"}
    },
    "seele": {
        "hosts": ["melchior.seele.net", "balthasar.seele.net", "casper.seele.net"],
        "vars": {"ntp_server": "ntp.seele.net"}
    },
    "magi": {
        "hosts": ["melchior.seele.net", "balthasar.seele.net", "casper.seele.net"],
        "vars": {"cluster_name": "MAGI"}
    },
    "oraclelinux": {
        "hosts": ["motoko.section9.net", "batou.section9.net", "melchior.seele.net", "balthasar.seele.net", "casper.seele.net"]
    },
    "ubuntu": {
        "hosts": ["tachikoma.section9.net"]
    },
    "oracle_db": {
        "hosts": ["melchior.seele.net", "balthasar.seele.net", "casper.seele.net"],
        "vars": {"oracle_home": "/opt/oracle/product/19c/dbhome_1"}
    },
    "high_memory": {
        "hosts": ["melchior.seele.net", "balthasar.seele.net"],
        "children": ["magi"]
    }
}

if scenario == "empty":
    json.dump({"hostvars": {}, "groups": {}}, sys.stdout)
    sys.exit(0)

if scenario == "error":
    print("Simulated SSH connection failure", file=sys.stderr)
    sys.exit(1)

# Scope filtering
if scope == "host":
    if target in HOSTS:
        json.dump({
            "hostvars": {target: HOSTS[target]},
            "groups": {}
        }, sys.stdout)
    else:
        print(f"Host '{target}' not found - SSH connection failed", file=sys.stderr)
        sys.exit(1)
elif scope == "group":
    if target in GROUPS:
        group_hosts = GROUPS[target].get("hosts", [])
        json.dump({
            "hostvars": {h: HOSTS[h] for h in group_hosts if h in HOSTS},
            "groups": {target: GROUPS[target]}
        }, sys.stdout)
    else:
        print(f"Group '{target}' not found", file=sys.stderr)
        sys.exit(1)
else:
    json.dump({
        "hostvars": HOSTS,
        "groups": GROUPS
    }, sys.stdout)
