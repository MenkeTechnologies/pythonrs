import glob

for name in ["a.py", "b.py", "c.txt", "d.py"]:
    open(name, "w").close()

print("py   :", sorted(glob.glob("*.py")))
print("all  :", sorted(glob.glob("*")))
