"""Regular expressions: match, findall, groups, substitution."""

import re

# Find all numbers.
print(re.findall(r"\d+", "order 42 costs 1099 cents, ref #7"))

# Named groups.
m = re.match(r"(?P<year>\d{4})-(?P<month>\d{2})-(?P<day>\d{2})", "2024-03-15")
print("date parts:", m.group("year"), m.group("month"), m.group("day"))
print("groupdict:", m.groupdict())

# Validate with a full match.
emails = ["user@example.com", "bad@", "a.b@test.org", "nope"]
pattern = re.compile(r"^[\w.]+@[\w.]+\.\w+$")
print("valid emails:", [e for e in emails if pattern.match(e)])

# Substitution with a callback (title-case each word).
print(re.sub(r"\w+", lambda m: m.group().capitalize(), "hello world foo"))

# Swap word pairs with backreferences.
print(re.sub(r"(\w+)\s+(\w+)", r"\2 \1", "first second"))

# Split on multiple delimiters.
print(re.split(r"[,;\s]+", "a, b;c  d"))

# Count and replace, reporting how many.
result, count = re.subn(r"o", "0", "foo boo zoo")
print(f"replaced {count}: {result}")

# Extract key=value pairs.
config = "host=localhost port=8080 debug=true"
pairs = dict(re.findall(r"(\w+)=(\w+)", config))
print("parsed config:", pairs)
