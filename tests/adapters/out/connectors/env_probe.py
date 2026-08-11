#!/usr/bin/env python3
"""
Connector fixture that reports the environment it was given, so a test can
assert what the adapter does — and does not — pass to a script.

Prints a native Dataset whose single host's vars are the script's environment.
"""
import json
import os

print(json.dumps({"hostvars": {"probe": dict(os.environ)}}))
