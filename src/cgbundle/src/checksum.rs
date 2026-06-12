use sha2::{Digest, Sha256};

/// Compute the SHA-256 hex digest of a byte slice.
pub fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // echo -n "hello" | sha256sum
        assert_eq!(
            sha256_bytes(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn empty_input() {
        assert_eq!(
            sha256_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn distinct_inputs_produce_distinct_digests() {
        assert_ne!(sha256_bytes(b"foo"), sha256_bytes(b"bar"));
    }

    #[test]
    fn same_input_is_deterministic() {
        assert_eq!(sha256_bytes(b"codegraph"), sha256_bytes(b"codegraph"));
    }
}
