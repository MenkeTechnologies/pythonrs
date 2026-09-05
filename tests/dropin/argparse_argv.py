import argparse

# Parses the REAL sys.argv. argparse_cli.py deliberately parses a fixed list so
# its output does not depend on the runner, which left argparse's own sys.argv
# read — the path every command-line program actually takes — uncovered: on the
# bridged build argparse reads the EMBEDDED interpreter's sys.argv, and that is a
# different list from the one pythonrs populates. The runner hands both
# interpreters the same fixed argv, so this stays deterministic.
p = argparse.ArgumentParser(prog="tool")
p.add_argument("--count", type=int, default=1)
p.add_argument("items", nargs="*")

ns = p.parse_args()
print("count:", ns.count)
print("items:", ns.items)
print("prog :", p.prog)
