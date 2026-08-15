#!/usr/bin/env python3
# A connector that exits 0 but prints something that is not JSON — the shape
# of a script that half-works. The adapter must answer with a parse error,
# not a panic and not an empty dataset.
print("this is not json {")
