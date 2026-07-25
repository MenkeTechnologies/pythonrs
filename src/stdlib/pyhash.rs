//! `_md5` / `_sha1` / `_sha2` / `_sha3` / `_blake2` — the hash accelerators.
//!
//! `hashlib.py` is a dispatcher: it imports one of these per algorithm family and
//! caches the constructor. Without them every constructor raises and the module
//! imports but cannot hash anything.
//!
//! The algorithms themselves come from the RustCrypto crates rather than being
//! rewritten here. A hash function is defined by its test vectors, and a
//! hand-rolled MD5 that is subtly wrong is worse than no MD5 — these are audited
//! implementations of the same FIPS/RFC specifications CPython links OpenSSL for.
//!
//! A hash object is incremental: `update()` feeds more data, `digest()`/
//! `hexdigest()` read the result WITHOUT finalizing (CPython's objects can be
//! read repeatedly and updated afterwards), and `copy()` forks the state. The
//! simplest representation that gets all three right is to keep the fed bytes and
//! hash on demand — hashing is fast, and the alternative (storing a live engine
//! per algorithm) buys nothing for the sizes this is used on.

use crate::host::{self, PyHost, PyObj};
use fusevm::Value;

/// Which algorithm a hash object computes. The name is the one `hashlib` knows
/// it by and the one the object reports as `.name`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Algo {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha3_224,
    Sha3_256,
    Sha3_384,
    Sha3_512,
    Shake128,
    Shake256,
    Blake2b,
    Blake2s,
}

impl Algo {
    pub fn from_name(n: &str) -> Option<Algo> {
        Some(match n {
            "md5" | "MD5" => Algo::Md5,
            "sha1" | "SHA1" => Algo::Sha1,
            "sha224" | "SHA224" => Algo::Sha224,
            "sha256" | "SHA256" => Algo::Sha256,
            "sha384" | "SHA384" => Algo::Sha384,
            "sha512" | "SHA512" => Algo::Sha512,
            "sha3_224" => Algo::Sha3_224,
            "sha3_256" => Algo::Sha3_256,
            "sha3_384" => Algo::Sha3_384,
            "sha3_512" => Algo::Sha3_512,
            "shake_128" => Algo::Shake128,
            "shake_256" => Algo::Shake256,
            "blake2b" => Algo::Blake2b,
            "blake2s" => Algo::Blake2s,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Algo::Md5 => "md5",
            Algo::Sha1 => "sha1",
            Algo::Sha224 => "sha224",
            Algo::Sha256 => "sha256",
            Algo::Sha384 => "sha384",
            Algo::Sha512 => "sha512",
            Algo::Sha3_224 => "sha3_224",
            Algo::Sha3_256 => "sha3_256",
            Algo::Sha3_384 => "sha3_384",
            Algo::Sha3_512 => "sha3_512",
            Algo::Shake128 => "shake_128",
            Algo::Shake256 => "shake_256",
            Algo::Blake2b => "blake2b",
            Algo::Blake2s => "blake2s",
        }
    }

    /// `digest_size` — the natural output length. A SHAKE has none (the caller
    /// chooses at read time), which CPython reports as 0.
    pub fn digest_size(self) -> usize {
        match self {
            Algo::Md5 => 16,
            Algo::Sha1 => 20,
            Algo::Sha224 | Algo::Sha3_224 => 28,
            Algo::Sha256 | Algo::Sha3_256 => 32,
            Algo::Sha384 | Algo::Sha3_384 => 48,
            Algo::Sha512 | Algo::Sha3_512 => 64,
            Algo::Blake2b => 64,
            Algo::Blake2s => 32,
            Algo::Shake128 | Algo::Shake256 => 0,
        }
    }

    /// `block_size` — the compression-function input width, which HMAC needs.
    pub fn block_size(self) -> usize {
        match self {
            Algo::Md5 | Algo::Sha1 | Algo::Sha224 | Algo::Sha256 => 64,
            Algo::Sha384 | Algo::Sha512 => 128,
            Algo::Sha3_224 => 144,
            Algo::Sha3_256 => 136,
            Algo::Sha3_384 => 104,
            Algo::Sha3_512 => 72,
            Algo::Shake128 => 168,
            Algo::Shake256 => 136,
            Algo::Blake2b => 128,
            Algo::Blake2s => 64,
        }
    }

    /// Whether the output length is chosen by the reader (a SHAKE).
    fn is_xof(self) -> bool {
        matches!(self, Algo::Shake128 | Algo::Shake256)
    }
}

/// Hash `data`, producing `out_len` bytes (only meaningful for a SHAKE or a
/// length-parameterized BLAKE2).
pub fn digest_bytes(algo: Algo, data: &[u8], out_len: usize) -> Vec<u8> {
    use digest::{Digest, ExtendableOutput, Update, XofReader};
    match algo {
        Algo::Md5 => md5::Md5::digest(data).to_vec(),
        Algo::Sha1 => sha1::Sha1::digest(data).to_vec(),
        Algo::Sha224 => sha2::Sha224::digest(data).to_vec(),
        Algo::Sha256 => sha2::Sha256::digest(data).to_vec(),
        Algo::Sha384 => sha2::Sha384::digest(data).to_vec(),
        Algo::Sha512 => sha2::Sha512::digest(data).to_vec(),
        Algo::Sha3_224 => sha3::Sha3_224::digest(data).to_vec(),
        Algo::Sha3_256 => sha3::Sha3_256::digest(data).to_vec(),
        Algo::Sha3_384 => sha3::Sha3_384::digest(data).to_vec(),
        Algo::Sha3_512 => sha3::Sha3_512::digest(data).to_vec(),
        Algo::Shake128 => {
            let mut h = sha3::Shake128::default();
            h.update(data);
            let mut out = vec![0u8; out_len];
            h.finalize_xof().read(&mut out);
            out
        }
        Algo::Shake256 => {
            let mut h = sha3::Shake256::default();
            h.update(data);
            let mut out = vec![0u8; out_len];
            h.finalize_xof().read(&mut out);
            out
        }
        // BLAKE2's digest length is a construction parameter, not a truncation,
        // so a shortened digest must be produced by a differently-parameterized
        // hash — not by slicing the full one.
        Algo::Blake2b => {
            use blake2::digest::{Update as _, VariableOutput};
            let mut h = blake2::Blake2bVar::new(out_len.clamp(1, 64)).expect("valid blake2b length");
            h.update(data);
            let mut out = vec![0u8; out_len.clamp(1, 64)];
            h.finalize_variable(&mut out).expect("blake2b finalize");
            out
        }
        Algo::Blake2s => {
            use blake2::digest::{Update as _, VariableOutput};
            let mut h = blake2::Blake2sVar::new(out_len.clamp(1, 32)).expect("valid blake2s length");
            h.update(data);
            let mut out = vec![0u8; out_len.clamp(1, 32)];
            h.finalize_variable(&mut out).expect("blake2s finalize");
            out
        }
    }
}

/// Bytes out of a `bytes`/`bytearray`/`memoryview` argument.
fn as_bytes(h: &PyHost, v: &Value) -> Option<Vec<u8>> {
    match h.get(v) {
        Some(PyObj::Bytes(b)) | Some(PyObj::Bytearray(b)) => Some(b.clone()),
        _ => None,
    }
}

/// Construct a hash object: `<algo>([data], *, digest_size=…, usedforsecurity=…)`.
pub fn construct(
    h: &mut PyHost,
    algo: Algo,
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value, String> {
    let mut data = Vec::new();
    if let Some(v) = args.first() {
        if !matches!(v, Value::Undef) {
            data = as_bytes(h, v).ok_or_else(|| {
                host::type_error("object supporting the buffer API required")
            })?;
        }
    }
    // BLAKE2 takes its output length at construction; everything else uses its
    // natural size.
    let out_len = kwargs
        .iter()
        .find(|(k, _)| k == "digest_size")
        .and_then(|(_, v)| h.as_int(v))
        .map(|n| n as usize)
        .unwrap_or_else(|| algo.digest_size());
    Ok(h.alloc(PyObj::Hasher {
        algo,
        data,
        out_len,
    }))
}

/// Methods on a hash object.
pub fn method(
    h: &mut PyHost,
    recv: &Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let (algo, data, out_len) = match h.get(recv) {
        Some(PyObj::Hasher {
            algo,
            data,
            out_len,
        }) => (*algo, data.clone(), *out_len),
        _ => return None,
    };
    Some(match name {
        "update" => {
            let Some(v) = args.first() else {
                return Some(Err(host::type_error("update() missing required argument")));
            };
            match as_bytes(h, v) {
                Some(more) => {
                    if let Some(PyObj::Hasher { data, .. }) = h.get_mut(recv) {
                        data.extend_from_slice(&more);
                    }
                    Ok(Value::Undef)
                }
                None => Err(host::type_error(
                    "object supporting the buffer API required",
                )),
            }
        }
        "digest" | "hexdigest" => {
            // A SHAKE takes its length HERE, from the caller.
            let n = if algo.is_xof() {
                match args.first().and_then(|v| h.as_int(v)) {
                    Some(n) if n >= 0 => n as usize,
                    _ => {
                        return Some(Err(host::type_error(
                            "digest() missing required argument: 'length'",
                        )))
                    }
                }
            } else {
                out_len
            };
            let out = digest_bytes(algo, &data, n);
            Ok(if name == "digest" {
                h.alloc(PyObj::Bytes(out))
            } else {
                let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
                h.new_str(hex)
            })
        }
        "copy" => Ok(h.alloc(PyObj::Hasher {
            algo,
            data,
            out_len,
        })),
        _ => return None,
    })
}

/// Attributes on a hash object.
pub fn attr(h: &mut PyHost, recv: &Value, name: &str) -> Option<Result<Value, String>> {
    let (algo, out_len) = match h.get(recv) {
        Some(PyObj::Hasher { algo, out_len, .. }) => (*algo, *out_len),
        _ => return None,
    };
    Some(match name {
        "name" => Ok(h.new_str(algo.name().to_string())),
        "digest_size" => Ok(Value::Int(if algo.is_xof() { 0 } else { out_len } as i64)),
        "block_size" => Ok(Value::Int(algo.block_size() as i64)),
        _ => return None,
    })
}

/// The namespace of one hash accelerator module. `hashlib` imports `_md5`,
/// `_sha1`, `_sha2`, `_sha3` or `_blake2` and pulls the named constructors out.
pub fn entries(h: &mut PyHost, module: &str) -> Option<Vec<(String, Value)>> {
    let names: &[&str] = match module {
        "_md5" => &["md5"],
        "_sha1" => &["sha1"],
        "_sha2" => &["sha224", "sha256", "sha384", "sha512"],
        "_sha3" => &[
            "sha3_224",
            "sha3_256",
            "sha3_384",
            "sha3_512",
            "shake_128",
            "shake_256",
        ],
        "_blake2" => &["blake2b", "blake2s"],
        _ => return None,
    };
    let mut out: Vec<(String, Value)> = names
        .iter()
        .map(|n| ((*n).to_string(), h.alloc(PyObj::Builtin(format!("_hash.{n}")))))
        .collect();
    if module == "_blake2" {
        // `hashlib` reads these off `_blake2` for its keyed/parameterized forms.
        for (k, v) in [
            ("BLAKE2B_MAX_DIGEST_SIZE", 64),
            ("BLAKE2B_MAX_KEY_SIZE", 64),
            ("BLAKE2B_SALT_SIZE", 16),
            ("BLAKE2B_PERSON_SIZE", 16),
            ("BLAKE2S_MAX_DIGEST_SIZE", 32),
            ("BLAKE2S_MAX_KEY_SIZE", 32),
            ("BLAKE2S_SALT_SIZE", 8),
            ("BLAKE2S_PERSON_SIZE", 8),
        ] {
            out.push((k.to_string(), Value::Int(v)));
        }
    }
    Some(out)
}
