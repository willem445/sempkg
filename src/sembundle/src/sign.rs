use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SignError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Key load error: {0}")]
    KeyLoad(String),
}

pub struct SignOptions {
    pub bundle_path: PathBuf,
    pub private_key_path: PathBuf,
    /// Output .sig path. Defaults to <bundle_path>.sig
    pub output: Option<PathBuf>,
}

/// Sign a .sembundle file. Writes a hex-encoded Ed25519 signature to the .sig file.
/// Returns the path of the written .sig file.
pub fn sign(opts: SignOptions) -> Result<PathBuf, SignError> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    use ed25519_dalek::Signer;
    use sha2::{Digest, Sha256};

    let bundle_bytes = std::fs::read(&opts.bundle_path)?;
    let bundle_sha256 = hex::encode(Sha256::digest(&bundle_bytes));

    let signing_key = ed25519_dalek::SigningKey::read_pkcs8_pem_file(&opts.private_key_path)
        .map_err(|e| SignError::KeyLoad(e.to_string()))?;

    let signature = signing_key.sign(bundle_sha256.as_bytes());
    let sig_hex = hex::encode(signature.to_bytes());

    let output_path = match opts.output {
        Some(p) => p,
        None => {
            let mut p = opts.bundle_path.as_os_str().to_owned();
            p.push(".sig");
            PathBuf::from(p)
        }
    };

    std::fs::write(&output_path, sig_hex.as_bytes())?;

    Ok(output_path)
}
