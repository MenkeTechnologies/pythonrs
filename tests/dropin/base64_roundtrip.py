import base64

raw = b"pythonrs drop-in"
enc = base64.b64encode(raw)
print("enc:", enc.decode())
print("dec:", base64.b64decode(enc).decode())
print("hex:", base64.b16encode(b"AB").decode())
