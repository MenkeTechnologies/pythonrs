import re

text = "user=alice id=42 user=bob id=7 user=carol id=13"
print("nums   :", re.findall(r"\d+", text))
print("users  :", re.findall(r"user=(\w+)", text))
print("sub    :", re.sub(r"id=\d+", "id=X", text))
m = re.search(r"user=(?P<who>\w+) id=(?P<n>\d+)", text)
print("groups :", m.group("who"), m.group("n"))
print("split  :", re.split(r"\s+", "a  b   c"))
