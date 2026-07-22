"""JSON: serialize, deserialize, pretty-print, and round-trip a config."""

import json

config = {
    "name": "pythonrs",
    "version": [0, 1, 0],
    "features": {"jit": True, "cache": True, "ffi": True},
    "limits": {"recursion": 1000, "workers": None},
    "tags": ["fast", "compiled", "compatible"],
}

# Compact and sorted forms.
print(json.dumps(config, sort_keys=True))

# Pretty-printed.
print(json.dumps({"a": 1, "b": [2, 3]}, indent=2))

# Round-trip through a string.
encoded = json.dumps(config)
decoded = json.loads(encoded)
print("round-trip ok:", decoded == config)
print("nested access:", decoded["features"]["ffi"])

# Parse a document and summarize.
doc = '{"users": [{"id": 1, "name": "Alice"}, {"id": 2, "name": "Bob"}]}'
data = json.loads(doc)
print("user names:", [u["name"] for u in data["users"]])

# Escaping and unicode.
print(json.dumps({"quote": 'say "hi"', "tab": "a\tb"}))
print(json.dumps({"city": "café"}, ensure_ascii=False))

# Malformed input is a catchable error.
try:
    json.loads("{not valid}")
except json.JSONDecodeError as e:
    print("decode error caught:", type(e).__name__)
