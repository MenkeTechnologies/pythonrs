from collections import deque

q = deque([2, 3, 4])
q.appendleft(1)
q.append(5)
print("deque:", list(q))
print("pop  :", q.pop(), q.popleft())
print("rest :", list(q))
