use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KeygenError {
    #[error("I/O error writing key file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Key serialization error: {0}")]
    Pkcs8(String),
}

pub struct KeygenOptions {
    /// Directory to write private.pem and public.pem (default: current dir)
    pub output_dir: PathBuf,
}

/// Generate an Ed25519 keypair and write:
///   <output_dir>/private.pem  — PKCS8 PEM private key
///   <output_dir>/public.pem   — SubjectPublicKeyInfo PEM public key
///
/// Returns (private_pem_path, public_pem_path).
pub fn keygen(opts: KeygenOptions) -> Result<(PathBuf, PathBuf), KeygenError> {
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use ed25519_dalek::pkcs8::spki::EncodePublicKey;
    use rand::rngs::OsRng;

    let signing_key = ed25519_dalek::SigningKey::generate(&mut OsRng);

    std::fs::create_dir_all(&opts.output_dir)?;

    let private_path = opts.output_dir.join("private.pem");
    let public_path = opts.output_dir.join("public.pem");

    signing_key
        .write_pkcs8_pem_file(&private_path, Default::default())
        .map_err(|e| KeygenError::Pkcs8(e.to_string()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&private_path, std::fs::Permissions::from_mode(0o600))?;
    }

    let pem = signing_key
        .verifying_key()
        .to_public_key_pem(Default::default())
        .map_err(|e| KeygenError::Pkcs8(e.to_string()))?;
    std::fs::write(&public_path, pem.as_bytes())?;

    Ok((private_path, public_path))
}
