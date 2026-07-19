# Write a file, read it straight back — the single most common thing a script
# does. Relative path lands in the runner's isolated cwd.
with open("data.txt", "w") as f:
    f.write("line one\n")
    f.write("line two\n")

with open("data.txt") as f:
    text = f.read()

print(len(text), "chars")
print(text.rstrip().replace("\n", " | "))
