text = "the quick brown fox the lazy dog the fox"
counts = {}
for word in text.split():
    counts[word] = counts.get(word, 0) + 1
for word, n in sorted(counts.items()):
    print(f"{word}: {n}")
print("unique words:", len(counts))
