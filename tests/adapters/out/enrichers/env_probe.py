#!/usr/bin/env python3
"""
Probe enricher for testing: writes the SOURCE_CONFIG it received into every
host's vars, so a test can assert what the service actually handed the script
(e.g. the `trigger` key).
"""

import json
import os
import sys

dataset = json.load(sys.stdin)

source_config = os.environ.get("SOURCE_CONFIG", "{}")

result = {"hostvars": {}, "groups": {}, "remove_hosts": []}
for hostname in dataset.get("hostvars", {}):
    result["hostvars"][hostname] = {"probed_source_config": source_config}

json.dump(result, sys.stdout)
