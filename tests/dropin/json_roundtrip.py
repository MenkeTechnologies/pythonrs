import json

data = {"name": "widget", "qty": 3, "tags": ["a", "b"], "meta": {"ok": True, "n": None}}
s = json.dumps(data, sort_keys=True)
print("dumps:", s)

back = json.loads(s)
print("qty  :", back["qty"] + 1)
print("tags :", ",".join(back["tags"]))
print("big  :", json.loads('{"n": 123456789012345678901234567890}')["n"])
