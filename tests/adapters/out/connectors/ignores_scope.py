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

json.dump(
    {"hostvars": {"h1.example": {"os": "linux"}}, "groups": {}},
    sys.stdout,
)
