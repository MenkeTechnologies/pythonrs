"""itertools: combinatorics, infinite iterators, and grouping."""

import itertools as it

# Combinatorics.
print("permutations:", list(it.permutations([1, 2, 3], 2)))
print("combinations:", list(it.combinations("ABCD", 2)))
print("product:     ", list(it.product([0, 1], repeat=2)))

# Accumulate (running totals / products).
print("running sum: ", list(it.accumulate([1, 2, 3, 4, 5])))
print("running max: ", list(it.accumulate([3, 1, 4, 1, 5, 9, 2], max)))

# Chaining and flattening.
print("chained:     ", list(it.chain([1, 2], [3, 4], [5])))
print("from_iterable:", list(it.chain.from_iterable([[1, 2], [3], [4, 5]])))

# Slicing infinite iterators.
print("count slice: ", list(it.islice(it.count(10, 5), 5)))
print("cycle slice: ", list(it.islice(it.cycle("AB"), 5)))
print("repeat:      ", list(it.repeat("x", 3)))

# takewhile / dropwhile.
print("takewhile:   ", list(it.takewhile(lambda x: x < 5, range(10))))
print("dropwhile:   ", list(it.dropwhile(lambda x: x < 5, range(10))))

# Grouping consecutive runs.
runs = [(k, len(list(g))) for k, g in it.groupby("aaabbbccaa")]
print("run lengths: ", runs)

# Pairwise (sliding window of 2).
print("pairwise:    ", list(it.pairwise([1, 2, 3, 4])))

# starmap over argument tuples.
print("starmap pow: ", list(it.starmap(pow, [(2, 3), (3, 2), (10, 2)])))
