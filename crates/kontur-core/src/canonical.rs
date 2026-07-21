use crate::ids::Hash;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Deterministic CBOR encoding. Structs encode their fields in declaration
/// order, so identical values always produce identical bytes — the basis of a
/// reproducible audit hash. Never feed a `HashMap`/`HashSet` through this.
pub fn canonical_bytes<T: Serialize>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf).expect("serialization is infallible for our types");
    buf
}

/// SHA-256 of an arbitrary byte slice.
pub fn sha256(bytes: &[u8]) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    Hash(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize)]
    struct Sample {
        a: u32,
        b: String,
    }

    #[test]
    fn canonical_bytes_are_stable_across_calls() {
        let s = Sample {
            a: 7,
            b: "hi".into(),
        };
        assert_eq!(canonical_bytes(&s), canonical_bytes(&s));
    }

    #[test]
    fn different_values_differ() {
        let s1 = Sample {
            a: 7,
            b: "hi".into(),
        };
        let s2 = Sample {
            a: 8,
            b: "hi".into(),
        };
        assert_ne!(canonical_bytes(&s1), canonical_bytes(&s2));
    }

    #[test]
    fn sha256_is_deterministic_and_sensitive() {
        assert_eq!(sha256(b"abc"), sha256(b"abc"));
        assert_ne!(sha256(b"abc"), sha256(b"abd"));
    }
}
