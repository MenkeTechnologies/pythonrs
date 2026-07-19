with open("nums.txt", "w") as f:
    for i in range(1, 6):
        f.write(f"{i}\n")

total = 0
count = 0
with open("nums.txt") as f:
    for line in f:
        total += int(line.strip())
        count += 1

print("lines:", count, "sum:", total)
print("readlines:", [l.rstrip() for l in open("nums.txt")])
