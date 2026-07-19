import os

p = "/home/user/project/report.md"
print("basename:", os.path.basename(p))
print("dirname :", os.path.dirname(p))
print("split   :", os.path.split(p))
print("splitext:", os.path.splitext(p))
print("join    :", os.path.join("a", "b", "c.txt"))
