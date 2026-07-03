use std::path::{Path, PathBuf};
use thiserror::Error;

// Re-exported so downstream crates (e.g. `sempkg`) can name the key type without
// taking their own `ed25519-dalek` dependency — which is exactly how a version
// split at the crate boundary would sneak back in.
pub use ed25519_dalek::VerifyingKey;

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid signature format: {0}")]
    InvalidFormat(String),
    #[error("Key load error: {0}")]
    KeyLoad(String),
    #[error(
        "Signature verification FAILED — bundle may be tampered or signed with a different key"
    )]
    VerificationFailed,
}

pub struct VerifyOptions {
    pub bundle_path: PathBuf,
    pub sig_path: PathBuf,
    pub public_key_path: PathBuf,
}

/// Load an Ed25519 verifying key from a PEM (SubjectPublicKeyInfo) file.
pub fn load_verifying_key(path: &Path) -> Result<VerifyingKey, VerifyError> {
    use ed25519_dalek::pkcs8::DecodePublicKey;

    let pem = std::fs::read_to_string(path)?;
    VerifyingKey::from_public_key_pem(pem.trim())
        .map_err(|e| VerifyError::KeyLoad(format!("Failed to parse PEM key: {e}")))
}

/// Verify a `.sig` file's contents against a bundle's bytes.
///
/// This is the single owner of the signing convention: an Ed25519 signature over
/// the hex-encoded SHA-256 digest of the bundle bytes, with the signature itself
/// stored **hex-encoded** in the `.sig` file. `sig_file_bytes` is the raw file
/// contents (hex text), matching what `sembundle sign` writes and what a registry
/// serves. Both the CLI and `sempkg` route through here so writer and reader
/// cannot drift.
pub fn verify_signature(
    bundle_bytes: &[u8],
    sig_file_bytes: &[u8],
    key: &VerifyingKey,
) -> Result<(), VerifyError> {
    use ed25519_dalek::Verifier;

    let bundle_sha256 = crate::checksum::sha256_bytes(bundle_bytes);

    let sig_hex = std::str::from_utf8(sig_file_bytes)
        .map_err(|e| VerifyError::InvalidFormat(format!("signature is not valid UTF-8: {e}")))?
        .trim();
    let sig_raw = hex::decode(sig_hex)
        .map_err(|e| VerifyError::InvalidFormat(format!("hex decode failed: {e}")))?;
    let sig_array: [u8; 64] = sig_raw
        .try_into()
        .map_err(|_| VerifyError::InvalidFormat("expected 64-byte signature".to_string()))?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_array);

    key.verify(bundle_sha256.as_bytes(), &sig)
        .map_err(|_| VerifyError::VerificationFailed)
}

/// Verify an Ed25519 `.sig` file against a `.sembundle`, reading all three inputs
/// from disk. Returns `Ok(())` if the signature is valid.
pub fn verify(opts: VerifyOptions) -> Result<(), VerifyError> {
    let bundle_bytes = std::fs::read(&opts.bundle_path)?;
    let sig_file_bytes = std::fs::read(&opts.sig_path)?;
    let key = load_verifying_key(&opts.public_key_path)?;
    verify_signature(&bundle_bytes, &sig_file_bytes, &key)
}
