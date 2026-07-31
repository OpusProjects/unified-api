#!/usr/bin/env python3
"""
Connector that keeps working past its caller's timeout, and leaves proof.

`slow.py` shows that a hung script does not hang the sync. This one answers the
next question: whether the script is actually STOPPED when the sync gives up, or
merely abandoned. It sleeps past the timeout and then appends its pid to
`marker_file` — so if the marker ever appears, the process outlived the run that
was supposed to bound it, and a wedged connector would accumulate one live copy
per sync interval forever.

Config:
  marker_file     where to record that it survived (required)
  sleep_seconds   how long to outlast the caller by (default 2)
"""

import json
import os
import sys
import time

config = json.loads(os.environ.get("SOURCE_CONFIG", "{}"))

time.sleep(float(config.get("sleep_seconds", "2")))

with open(config["marker_file"], "a") as handle:
    handle.write("survived: pid %d\n" % os.getpid())

json.dump({"hostvars": {}, "groups": {}}, sys.stdout)
