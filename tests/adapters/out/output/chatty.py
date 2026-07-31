#!/usr/bin/env python3
"""
An output transformer that talks before it listens.

Same shape as the chatty enricher, for the endpoint path: it writes far more
than a pipe buffer (64 KiB on Linux) to stderr before reading its stdin, which
deadlocks any caller that hands over the datasets before draining stdout/stderr.

An endpoint is fed every configured source at once, so its stdin is the largest
in the process — this is the path with the most room for the failure.
"""

import json
import sys

# ~400 KiB, written before stdin is read
for i in range(4000):
    print("chatty endpoint progress line %05d %s" % (i, "." * 60), file=sys.stderr)
sys.stderr.flush()

datasets = json.load(sys.stdin)

hosts = sorted(
    host for dataset in datasets.values() for host in dataset.get("hostvars", {})
)
json.dump({"hosts": hosts}, sys.stdout)
