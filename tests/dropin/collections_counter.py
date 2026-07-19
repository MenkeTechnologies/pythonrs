from collections import Counter, defaultdict

words = "the cat sat on the mat the cat ran".split()
c = Counter(words)
print("most :", c.most_common(2))
print("the  :", c["the"], "nope:", c["nope"])

d = defaultdict(list)
for i, w in enumerate(words):
    d[len(w)].append(w)
print("bylen:", dict(sorted(d.items())))
