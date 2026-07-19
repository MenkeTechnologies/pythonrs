# Load a JSON config, transform it, write it back — the classic glue script.
import json

cfg = {"version": 1, "services": [{"name": "web", "port": 80}, {"name": "db", "port": 5432}], "debug": False}
with open("config.json", "w") as f:
    json.dump(cfg, f, indent=2, sort_keys=True)

with open("config.json") as f:
    loaded = json.load(f)

loaded["version"] += 1
loaded["debug"] = True
ports = {s["name"]: s["port"] for s in loaded["services"]}
print("version:", loaded["version"])
print("ports  :", json.dumps(ports, sort_keys=True))
print("names  :", sorted(ports))
