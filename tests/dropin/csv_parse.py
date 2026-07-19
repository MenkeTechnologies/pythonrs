import csv

with open("people.csv", "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["name", "age"])
    w.writerow(["alice", "30"])
    w.writerow(["bob", "25"])

with open("people.csv", newline="") as f:
    rows = list(csv.DictReader(f))

print("count:", len(rows))
print("avg  :", sum(int(r["age"]) for r in rows) / len(rows))
print("names:", [r["name"] for r in rows])
