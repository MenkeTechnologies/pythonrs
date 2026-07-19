from pathlib import Path

p = Path("src") / "pkg" / "mod.py"
print("str   :", str(p))
print("name  :", p.name)
print("stem  :", p.stem)
print("suffix:", p.suffix)
print("parent:", str(p.parent))
print("parts :", p.parts)

Path("hello.txt").write_text("hi there")
print("roundtrip:", Path("hello.txt").read_text())
