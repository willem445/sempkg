/// Ed25519 signature verification for sembundle packages.
///
/// Thin delegation layer over [`sembundle::verify`], which owns the signing
/// convention: an Ed25519 signature over the hex-encoded SHA-256 digest of the
/// bundle bytes, with the signature stored **hex-encoded** in the `.sig` file.
/// Routing through the shared crate keeps writer (`sembundle sign`) and reader
/// (`sempkg`) from drifting — including the `.sig` hex-decode step — and avoids a
/// second `ed25519-dalek` dependency whose version could split at the boundary.
use std::path::Path;

use anyhow::{Context, Result};

pub use sembundle::verify::VerifyingKey;

/// Load an Ed25519 verifying key from a PEM file.
pub fn load_verifying_key(path: &Path) -> Result<VerifyingKey> {
    sembundle::verify::load_verifying_key(path)
        .with_context(|| format!("Cannot load public key: {}", path.display()))
}

/// Verify that `sig_bytes` — the raw contents of a `.sig` file (hex-encoded
/// signature) — is a valid signature over `bundle_bytes`, using `key`.
pub fn verify_bundle_signature(
    bundle_bytes: &[u8],
    sig_bytes: &[u8],
    key: &VerifyingKey,
) -> Result<()> {
    sembundle::verify::verify_signature(bundle_bytes, sig_bytes, key).map_err(anyhow::Error::from)
}
