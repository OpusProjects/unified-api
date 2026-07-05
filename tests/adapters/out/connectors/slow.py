#!/usr/bin/env python3
"""Sample connector that takes too long — used to test execution timeouts."""
import json
import time

time.sleep(10)
print(json.dumps({"hostvars": {"slow.example.net": {"ansible_host": "10.0.0.1"}}, "groups": {}}))
