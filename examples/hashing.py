"""Hashing and encoding: hashlib digests, base64, and a checksum."""

import hashlib
import base64
import binascii

message = b"the quick brown fox"

print("md5:   ", hashlib.md5(message).hexdigest())
print("sha1:  ", hashlib.sha1(message).hexdigest())
print("sha256:", hashlib.sha256(message).hexdigest())

# Incremental hashing produces the same digest as a single call.
h = hashlib.sha256()
h.update(b"the quick ")
h.update(b"brown fox")
print("incremental matches:", h.hexdigest() == hashlib.sha256(message).hexdigest())

# base64 round-trip.
encoded = base64.b64encode(message)
print("base64:", encoded.decode())
print("decoded:", base64.b64decode(encoded).decode())
print("urlsafe:", base64.urlsafe_b64encode(b"\xfb\xff\xfe").decode())

# hex encoding.
print("hex:", binascii.hexlify(b"AB").decode())
print("unhex:", binascii.unhexlify(b"4849").decode())

# A tiny content-addressed store keyed by digest.
store = {}
for text in ["alpha", "beta", "alpha", "gamma"]:
    key = hashlib.sha256(text.encode()).hexdigest()[:8]
    store[key] = text
print("unique blobs:", len(store))
