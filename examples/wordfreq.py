"""Text analysis: word frequency, char counts, and a tiny histogram."""

from collections import Counter

text = """
the quick brown fox jumps over the lazy dog
the dog barks and the fox runs the quick fox wins
""".strip()

words = text.split()
freq = Counter(words)

print("total words:", len(words))
print("unique words:", len(freq))
print("top 5:", freq.most_common(5))

print("\nhistogram:")
for word, n in freq.most_common(5):
    print(f"  {word:>6} | {'#' * n} {n}")

# Character frequencies (letters only).
letters = Counter(c for c in text.lower() if c.isalpha())
print("\nmost common letters:", letters.most_common(3))

# Longest and shortest words.
by_length = sorted(set(words), key=lambda w: (len(w), w))
print("shortest:", by_length[0], "| longest:", by_length[-1])

# Average word length.
avg = sum(len(w) for w in words) / len(words)
print(f"average length: {avg:.2f}")
