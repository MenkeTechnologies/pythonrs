import argparse

# Parse a fixed list so output is deterministic regardless of the runner's argv.
p = argparse.ArgumentParser(prog="tool")
p.add_argument("--count", type=int, default=1)
p.add_argument("--name", default="x")
p.add_argument("--verbose", action="store_true")
p.add_argument("items", nargs="*")

ns = p.parse_args(["--count", "3", "--name", "job", "--verbose", "a", "b"])
print("count  :", ns.count)
print("name   :", ns.name)
print("verbose:", ns.verbose)
print("items  :", ns.items)
