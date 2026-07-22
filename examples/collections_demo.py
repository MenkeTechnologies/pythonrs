"""The collections module: Counter, defaultdict, deque, namedtuple, OrderedDict."""

from collections import (
    Counter,
    defaultdict,
    deque,
    namedtuple,
    OrderedDict,
)

# Counter arithmetic.
inventory = Counter(apples=3, bananas=2, cherries=5)
restock = Counter(apples=2, dates=4)
print("combined:", sorted((inventory + restock).items()))
print("most common:", inventory.most_common(2))

# defaultdict for grouping.
groups = defaultdict(list)
for word in "apple ant bear bird cat cow".split():
    groups[word[0]].append(word)
print("grouped:", dict(sorted(groups.items())))

# deque as a bounded ring buffer / queue.
recent = deque(maxlen=3)
for i in range(6):
    recent.append(i)
print("last 3:", list(recent))

queue = deque([1, 2, 3])
queue.appendleft(0)
queue.rotate(1)
print("rotated:", list(queue), "maxlen:", queue.maxlen)

# namedtuple as a lightweight record.
Point = namedtuple("Point", ["x", "y"])
p = Point(3, 4)
print("point:", p, "| distance:", round((p.x**2 + p.y**2) ** 0.5, 2))
print("as dict:", p._asdict())
print("replaced:", p._replace(x=10))

# OrderedDict move-to-end.
od = OrderedDict([("a", 1), ("b", 2), ("c", 3)])
od.move_to_end("a")
print("reordered:", list(od))
