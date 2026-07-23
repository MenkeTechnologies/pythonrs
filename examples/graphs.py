"""Graph algorithms — BFS, DFS, topological sort, and Dijkstra (heapq)."""

import heapq
from collections import defaultdict, deque


def build_graph(edges):
    g = defaultdict(list)
    for a, b in edges:
        g[a].append(b)
    return g


def bfs(graph, start):
    """Breadth-first order from `start`."""
    seen, order, q = {start}, [], deque([start])
    while q:
        node = q.popleft()
        order.append(node)
        for nxt in graph[node]:
            if nxt not in seen:
                seen.add(nxt)
                q.append(nxt)
    return order


def dfs(graph, start):
    """Depth-first order (iterative, so recursion depth never matters)."""
    seen, order, stack = set(), [], [start]
    while stack:
        node = stack.pop()
        if node in seen:
            continue
        seen.add(node)
        order.append(node)
        # Reverse so neighbors are visited in listed order.
        stack.extend(reversed(graph[node]))
    return order


def topological_sort(nodes, edges):
    """Kahn's algorithm; raises on a cycle."""
    graph = build_graph(edges)
    indegree = dict.fromkeys(nodes, 0)
    for _, b in edges:
        indegree[b] += 1
    ready = deque(sorted(n for n in nodes if indegree[n] == 0))
    order = []
    while ready:
        node = ready.popleft()
        order.append(node)
        for nxt in graph[node]:
            indegree[nxt] -= 1
            if indegree[nxt] == 0:
                ready.append(nxt)
    if len(order) != len(nodes):
        raise ValueError("graph has a cycle")
    return order


def dijkstra(graph, start):
    """Shortest-path distances over a weighted graph {node: [(nbr, w), ...]}."""
    dist = {start: 0}
    pq = [(0, start)]
    while pq:
        d, node = heapq.heappop(pq)
        if d > dist.get(node, float("inf")):
            continue
        for nbr, w in graph[node]:
            nd = d + w
            if nd < dist.get(nbr, float("inf")):
                dist[nbr] = nd
                heapq.heappush(pq, (nd, nbr))
    return dist


edges = [("a", "b"), ("a", "c"), ("b", "d"), ("c", "d"), ("d", "e")]
g = build_graph(edges)
print("bfs:", bfs(g, "a"))
print("dfs:", dfs(g, "a"))

tasks = ["compile", "link", "test", "package", "deploy"]
deps = [
    ("compile", "link"),
    ("link", "test"),
    ("link", "package"),
    ("test", "deploy"),
    ("package", "deploy"),
]
print("topo:", topological_sort(tasks, deps))

weighted = {
    "s": [("a", 1), ("b", 4)],
    "a": [("b", 2), ("c", 5)],
    "b": [("c", 1)],
    "c": [],
}
print("dijkstra:", sorted(dijkstra(weighted, "s").items()))

try:
    topological_sort(["x", "y"], [("x", "y"), ("y", "x")])
except ValueError as e:
    print("cycle detected:", e)
