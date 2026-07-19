import hashlib

print("md5   :", hashlib.md5(b"hello world").hexdigest())
print("sha1  :", hashlib.sha1(b"hello world").hexdigest())
print("sha256:", hashlib.sha256(b"hello world").hexdigest())
