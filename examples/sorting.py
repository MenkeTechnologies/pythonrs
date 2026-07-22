"""Sorting: the built-in sort with keys, plus a hand-written quicksort/mergesort."""


def quicksort(xs):
    if len(xs) <= 1:
        return xs
    pivot = xs[len(xs) // 2]
    less = [x for x in xs if x < pivot]
    equal = [x for x in xs if x == pivot]
    greater = [x for x in xs if x > pivot]
    return quicksort(less) + equal + quicksort(greater)


def mergesort(xs):
    if len(xs) <= 1:
        return xs
    mid = len(xs) // 2
    left, right = mergesort(xs[:mid]), mergesort(xs[mid:])
    out, i, j = [], 0, 0
    while i < len(left) and j < len(right):
        if left[i] <= right[j]:
            out.append(left[i])
            i += 1
        else:
            out.append(right[j])
            j += 1
    return out + left[i:] + right[j:]


data = [5, 2, 9, 1, 5, 6, 3, 8, 5, 0]
print("quicksort:", quicksort(data))
print("mergesort:", mergesort(data))
print("builtin:  ", sorted(data))
print("reverse:  ", sorted(data, reverse=True))

people = [("Alice", 30), ("Bob", 25), ("Carol", 30), ("Dan", 25)]
print("by age, then name:", sorted(people, key=lambda p: (p[1], p[0])))

words = ["banana", "Apple", "cherry", "date"]
print("case-insensitive:", sorted(words, key=str.lower))
print("by length:", sorted(words, key=len))
