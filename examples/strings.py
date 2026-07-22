"""String processing: methods, slicing, and the format mini-language."""

s = "The Quick Brown Fox"
print("upper/lower:", s.upper(), "|", s.lower())
print("swapcase:   ", s.swapcase())
print("title:      ", "hello world".title())
print("split/join: ", "-".join(s.split()))
print("replace:    ", s.replace("Quick", "Slow"))
print("reversed:   ", s[::-1])
print("every other:", s[::2])
print("centered:   ", "x".center(11, "*"))
print("startswith: ", s.startswith(("A", "The")))
print("counts:     ", s.lower().count("o"))
print("strip:      ", repr("  padded  ".strip()))
print("partition:  ", "key=value=extra".partition("="))

# f-string format specs.
pi = 3.14159265
print(f"fixed:   {pi:.2f}")
print(f"percent: {0.875:.1%}")
print(f"sci:     {123456.789:.2e}")
print(f"padded:  {42:>8} | {42:<8} | {42:^8}")
print(f"zero:    {7:04d}")
print(f"hex/oct/bin: {255:#x} {255:#o} {255:#b}")
print(f"thousands:   {1234567:,}")
print(f"debug: {pi=:.3f}")

# Palindrome check.
def is_palindrome(text):
    cleaned = [c.lower() for c in text if c.isalnum()]
    return cleaned == cleaned[::-1]


for phrase in ["racecar", "A man a plan a canal Panama", "hello"]:
    print(f"{phrase!r:35} -> {is_palindrome(phrase)}")
