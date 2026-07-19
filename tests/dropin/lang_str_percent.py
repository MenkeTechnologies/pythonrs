# printf-style %-formatting — still common in logging/format strings.
print("%.2f" % 3.14159)
print("%05d" % 42)
print("%s=%d" % ("count", 7))
print("%x %o %b" % (255, 8, 5) if False else "%x %o" % (255, 8))
print("%-8s|" % "hi")
