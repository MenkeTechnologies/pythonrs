import os

for name in ["b.txt", "a.txt", "c.log"]:
    with open(name, "w") as f:
        f.write("x")

print("listing:", sorted(os.listdir(".")))
print("exists :", os.path.exists("a.txt"), os.path.exists("nope.txt"))
