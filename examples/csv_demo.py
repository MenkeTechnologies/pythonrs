"""CSV in memory — write and read back records without touching the disk.

Uses io.StringIO so the example stays deterministic and self-contained, the way
an agent parses CSV payloads that arrive as strings.
"""

import csv
import io

rows = [
    {"name": "Alice", "dept": "eng", "score": 95},
    {"name": "Bob", "dept": "eng", "score": 82},
    {"name": "Carol", "dept": "sales", "score": 71},
]

# Write dict rows to a CSV string.
buf = io.StringIO()
writer = csv.DictWriter(buf, fieldnames=["name", "dept", "score"])
writer.writeheader()
writer.writerows(rows)
text = buf.getvalue()
print("--- serialized ---")
print(text, end="")

# Read it back and aggregate.
reader = csv.DictReader(io.StringIO(text))
parsed = list(reader)
print("--- parsed", len(parsed), "rows ---")
for r in parsed:
    print(f"{r['name']:6} {r['dept']:6} {r['score']}")

total = sum(int(r["score"]) for r in parsed)
print("total score:", total)

# Plain (non-dict) reader/writer with a custom delimiter.
buf2 = io.StringIO()
w = csv.writer(buf2, delimiter="|")
w.writerow(["a", "b", "c"])
w.writerow([1, 2, 3])
print("--- pipe-delimited ---")
print(buf2.getvalue(), end="")

# Quoting: fields with the delimiter or newlines round-trip intact.
buf3 = io.StringIO()
csv.writer(buf3).writerow(["plain", "has,comma", "has\nnewline"])
back = next(csv.reader(io.StringIO(buf3.getvalue())))
print("round-trip fields:", back)
