use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid signature format: {0}")]
    InvalidFormat(String),
    #[error("Key load error: {0}")]
    KeyLoad(String),
    #[error("Signature verification FAILED — bundle may be tampered or signed with a different key")]
    VerificationFailed,
}

pub struct VerifyOptions {
    pub bundle_path: PathBuf,
    pub sig_path: PathBuf,
    pub public_key_path: PathBuf,
}

/// Verify an Ed25519 .sig file against a .sembundle.
/// Returns Ok(()) if the signature is valid.
/// Returns Err(VerifyError::VerificationFailed) if the signature does not match.
pub fn verify(opts: VerifyOptions) -> Result<(), VerifyError> {
    use ed25519_dalek::pkcs8::DecodePublicKey;
    use ed25519_dalek::Verifier;
    use sha2::{Digest, Sha256};

    let bundle_bytes = std::fs::read(&opts.bundle_path)?;
    let bundle_sha256 = hex::encode(Sha256::digest(&bundle_bytes));

    let sig_content = std::fs::read_to_string(&opts.sig_path)?;
    let sig_hex = sig_content.trim();
    let sig_bytes = hex::decode(sig_hex)
        .map_err(|e| VerifyError::InvalidFormat(format!("hex decode failed: {e}")))?;

    let sig_bytes_array: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| VerifyError::InvalidFormat("expected 64-byte signature".to_string()))?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes_array);

    let verifying_key =
        ed25519_dalek::VerifyingKey::read_public_key_pem_file(&opts.public_key_path)
            .map_err(|e| VerifyError::KeyLoad(e.to_string()))?;

    verifying_key
        .verify(bundle_sha256.as_bytes(), &sig)
        .map_err(|_| VerifyError::VerificationFailed)?;

    Ok(())
}
