# A composite of what an agent actually writes: read a file, tokenize, count,
# and print the top entries sorted. Exercises open + re + Counter + sorting.
import re
from collections import Counter

text = """the quick brown fox the lazy dog
the fox jumps the dog sleeps the end"""
with open("corpus.txt", "w") as f:
    f.write(text)

with open("corpus.txt") as f:
    words = re.findall(r"[a-z]+", f.read().lower())

freq = Counter(words)
for word, n in sorted(freq.items(), key=lambda kv: (-kv[1], kv[0]))[:5]:
    print(f"{n:3d}  {word}")
print("unique:", len(freq), "total:", sum(freq.values()))
