#!/usr/bin/env python3
"""Sample output transformer that takes too long — used to test the endpoint timeout."""
import time

time.sleep(10)
print("too late")
