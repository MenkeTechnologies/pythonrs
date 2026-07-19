import subprocess

r = subprocess.run(["printf", "hello\\nworld"], capture_output=True, text=True)
print("rc  :", r.returncode)
print("out :", repr(r.stdout))

lines = subprocess.check_output(["seq", "1", "3"], text=True)
print("seq :", lines.split())
