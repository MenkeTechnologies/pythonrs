"""Data aggregation — the statistics module plus group-by summaries.

The bread-and-butter of agent-written analysis scripts: load records, group them,
and compute per-group summaries.
"""

import statistics
from collections import defaultdict

samples = [4, 8, 15, 16, 23, 42]
print("mean:", statistics.mean(samples))
print("median:", statistics.median(samples))
print("mode:", statistics.mode([1, 2, 2, 3, 3, 3]))
print("pstdev:", round(statistics.pstdev(samples), 4))
print("variance:", round(statistics.variance(samples), 4))
print("quantiles:", statistics.quantiles(samples, n=4))

records = [
    ("eng", "alice", 95),
    ("eng", "bob", 82),
    ("sales", "carol", 71),
    ("eng", "dave", 88),
    ("sales", "erin", 90),
]

# Group scores by department, then summarize each group.
by_dept = defaultdict(list)
for dept, _name, score in records:
    by_dept[dept].append(score)

print("--- per-department summary ---")
for dept in sorted(by_dept):
    scores = by_dept[dept]
    print(
        f"{dept:6} n={len(scores)} "
        f"mean={statistics.mean(scores):.1f} "
        f"min={min(scores)} max={max(scores)}"
    )

# Top performer overall.
top = max(records, key=lambda r: r[2])
print("top:", top[1], "with", top[2])

# A running total and a normalized distribution.
total = sum(s for *_, s in records)
print("total:", total)
dist = {name: round(score / total, 3) for _d, name, score in records}
print("share:", dict(sorted(dist.items())))
