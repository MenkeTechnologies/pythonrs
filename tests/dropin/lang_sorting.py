pairs = [("bob", 25), ("alice", 30), ("carol", 25)]
print(sorted(pairs))
print(sorted(pairs, key=lambda p: p[1]))
print(sorted(pairs, key=lambda p: (p[1], p[0])))
print(sorted(pairs, key=lambda p: p[1], reverse=True))
print(max(pairs, key=lambda p: p[1]))
print(min([3, 1, 4, 1, 5, 9, 2, 6]))
