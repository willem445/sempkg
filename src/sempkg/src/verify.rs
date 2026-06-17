/// Ed25519 signature verification for sembundle packages.
///
/// The signature is over the hex-encoded SHA-256 digest of the bundle bytes,
/// matching the convention used by `sembundle publish`.
use std::path::Path;

use anyhow::{Context, Result};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::error::SempkgError;

/// Load an Ed25519 verifying key from a PEM file.
pub fn load_verifying_key(path: &Path) -> Result<VerifyingKey> {
    use ed25519_dalek::pkcs8::DecodePublicKey;

    let pem = std::fs::read_to_string(path)
        .with_context(|| format!("Cannot read public key file: {}", path.display()))?;

    VerifyingKey::from_public_key_pem(pem.trim()).map_err(|e| {
        SempkgError::SignatureVerificationFailed(format!("Failed to parse PEM key: {e}")).into()
    })
}

/// Verify that `sig_bytes` is a valid Ed25519 signature over the hex SHA-256
/// digest of `bundle_bytes`, using the provided verifying key.
pub fn verify_bundle_signature(
    bundle_bytes: &[u8],
    sig_bytes: &[u8],
    key: &VerifyingKey,
) -> Result<()> {
    let digest = hex::encode(Sha256::digest(bundle_bytes));
    let sig = Signature::from_slice(sig_bytes).map_err(|e| {
        SempkgError::SignatureVerificationFailed(format!("Invalid signature bytes: {e}"))
    })?;

    key.verify(digest.as_bytes(), &sig).map_err(|_| {
        SempkgError::SignatureVerificationFailed("Signature does not match bundle".to_string())
            .into()
    })
}
