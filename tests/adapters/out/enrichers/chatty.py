#!/usr/bin/env python3
"""
An enricher that talks before it listens.

It writes far more than a pipe buffer (64 KiB on Linux) to stderr *before*
reading its stdin — an ordinary verbose script logging progress as it starts up.

That is enough to deadlock a caller that writes the whole dataset to stdin
before draining stdout/stderr: the script blocks writing to a full stderr pipe,
so it never drains stdin; the caller blocks writing to a full stdin pipe, so it
never drains stderr. Neither side can move until something times out.

It is otherwise a normal enricher: it tags every host it is given.
"""

import json
import sys

# ~400 KiB, comfortably past any pipe buffer, and written before stdin is read
for i in range(4000):
    print("chatty enricher progress line %05d %s" % (i, "." * 60), file=sys.stderr)
sys.stderr.flush()

dataset = json.load(sys.stdin)

result = {
    "hostvars": {
        hostname: {**vars, "chatty": True}
        for hostname, vars in dataset.get("hostvars", {}).items()
    },
    "groups": {},
    "remove_hosts": [],
}

json.dump(result, sys.stdout)
