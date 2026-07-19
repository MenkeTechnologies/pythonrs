import sys

# The runner passes a fixed argv: prog.py alpha beta 42
print("argc:", len(sys.argv) - 1)
print("args:", sys.argv[1:])
print("prog:", sys.argv[0].rsplit("/", 1)[-1])
