"""argparse: the CLI-parsing pattern agents reach for constantly.

Args are parsed from fixed lists (not `sys.argv`) so the output is deterministic
and the whole flag surface — types, defaults, choices, flags, nargs, subcommands —
is exercised in one run.
"""

import argparse


def build_parser():
    p = argparse.ArgumentParser(prog="tool", description="a demo CLI")
    p.add_argument("input", help="input path")
    p.add_argument("-o", "--output", default="out.txt", help="output path")
    p.add_argument("-n", "--count", type=int, default=1, help="repetitions")
    p.add_argument("-v", "--verbose", action="store_true", help="chatty mode")
    p.add_argument(
        "--mode",
        choices=["fast", "safe", "auto"],
        default="auto",
        help="operating mode",
    )
    p.add_argument("--tag", action="append", default=[], help="repeatable tag")
    return p


parser = build_parser()

# A minimal invocation leans on every default.
a = parser.parse_args(["data.csv"])
print("defaults:", a.input, a.output, a.count, a.verbose, a.mode, a.tag)

# A fully specified one, with a repeated --tag.
b = parser.parse_args(
    ["data.csv", "-o", "res.txt", "-n", "3", "-v", "--mode", "fast", "--tag", "x", "--tag", "y"]
)
print("explicit:", b.input, b.output, b.count, b.verbose, b.mode, b.tag)

# Subcommands — the `git`-style dispatch pattern.
top = argparse.ArgumentParser(prog="vcs")
subs = top.add_subparsers(dest="cmd", required=True)

add = subs.add_parser("add")
add.add_argument("files", nargs="+")

commit = subs.add_parser("commit")
commit.add_argument("-m", "--message", required=True)

print("add:", vars(top.parse_args(["add", "a.py", "b.py"])))
print("commit:", vars(top.parse_args(["commit", "-m", "fix"])))

# A parse error is a catchable SystemExit (argparse exits 2 on bad input).
try:
    parser.parse_args(["data.csv", "--mode", "bogus"])
except SystemExit as e:
    print("rejected bad choice, exit code:", e.code)
