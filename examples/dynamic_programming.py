"""Dynamic programming: coin change, longest common subsequence, edit distance."""


def coin_change(coins, amount):
    """Fewest coins to make `amount`, or -1 if impossible."""
    best = [0] + [float("inf")] * amount
    for value in range(1, amount + 1):
        for coin in coins:
            if coin <= value:
                best[value] = min(best[value], best[value - coin] + 1)
    return best[amount] if best[amount] != float("inf") else -1


def longest_common_subsequence(a, b):
    grid = [[0] * (len(b) + 1) for _ in range(len(a) + 1)]
    for i in range(1, len(a) + 1):
        for j in range(1, len(b) + 1):
            if a[i - 1] == b[j - 1]:
                grid[i][j] = grid[i - 1][j - 1] + 1
            else:
                grid[i][j] = max(grid[i - 1][j], grid[i][j - 1])
    return grid[len(a)][len(b)]


def edit_distance(a, b):
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        curr = [i]
        for j, cb in enumerate(b, 1):
            cost = 0 if ca == cb else 1
            curr.append(min(prev[j] + 1, curr[j - 1] + 1, prev[j - 1] + cost))
        prev = curr
    return prev[-1]


print("coin change (5 for [1,2,5]):", coin_change([1, 2, 5], 5))
print("coin change (11 for [1,2,5]):", coin_change([1, 2, 5], 11))
print("coin change (3 for [2]):", coin_change([2], 3))

print("LCS('AGGTAB', 'GXTXAYB'):", longest_common_subsequence("AGGTAB", "GXTXAYB"))
print("edit distance('kitten', 'sitting'):", edit_distance("kitten", "sitting"))
print("edit distance('flaw', 'lawn'):", edit_distance("flaw", "lawn"))
