from datetime import date, timedelta

d = date(2026, 7, 19)
print("iso  :", d.isoformat())
print("wday :", d.weekday())
print("plus :", (d + timedelta(days=45)).isoformat())
print("diff :", (date(2026, 12, 25) - d).days)
