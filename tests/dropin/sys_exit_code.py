import sys

print("before exit")
if len(sys.argv) > 1:
    sys.exit(0)
print("unreachable when args are present")
