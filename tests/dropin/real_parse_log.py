# Parse structured log lines with a regex, aggregate by level. open + re + dict.
import re
from collections import defaultdict

log = """2026-07-19 INFO started
2026-07-19 WARN slow query 320ms
2026-07-19 ERROR timeout
2026-07-19 INFO ok
2026-07-19 WARN retry 2"""
with open("app.log", "w") as f:
    f.write(log)

counts = defaultdict(int)
pat = re.compile(r"^\S+ (\w+) (.+)$")
with open("app.log") as f:
    for line in f:
        m = pat.match(line.strip())
        if m:
            counts[m.group(1)] += 1

for level in sorted(counts):
    print(f"{level:5s} {counts[level]}")
