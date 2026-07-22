"""Classic recursive algorithms: Hanoi, permutations, Ackermann, tree walk."""


def hanoi(n, source, target, spare, moves):
    if n == 1:
        moves.append((source, target))
        return
    hanoi(n - 1, source, spare, target, moves)
    moves.append((source, target))
    hanoi(n - 1, spare, target, source, moves)


def permutations(items):
    if len(items) <= 1:
        return [items]
    result = []
    for i, x in enumerate(items):
        rest = items[:i] + items[i + 1 :]
        for perm in permutations(rest):
            result.append([x] + perm)
    return result


def ackermann(m, n):
    if m == 0:
        return n + 1
    if n == 0:
        return ackermann(m - 1, 1)
    return ackermann(m - 1, ackermann(m, n - 1))


def tree_sum(node):
    """Sum a nested dict tree of the shape {'value': v, 'children': [...]}."""
    total = node["value"]
    for child in node.get("children", []):
        total += tree_sum(child)
    return total


moves = []
hanoi(3, "A", "C", "B", moves)
print("hanoi(3):", len(moves), "moves")
print("  ", moves)

print("permutations of [1,2,3]:")
for p in permutations([1, 2, 3]):
    print("  ", p)

print("ackermann(2, 3):", ackermann(2, 3))

tree = {
    "value": 1,
    "children": [
        {"value": 2, "children": [{"value": 4}, {"value": 5}]},
        {"value": 3, "children": [{"value": 6}]},
    ],
}
print("tree sum:", tree_sum(tree))
