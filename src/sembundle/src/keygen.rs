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
    use ed25519_dalek::pkcs8::spki::EncodePublicKey;
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    // ed25519-dalek 3.0 moved to rand_core 0.10, and rand 0.10 replaced
    // `rand::rngs::OsRng` with `SysRng` — which only implements `TryCryptoRng`
    // (its error type isn't `Infallible`: the OS RNG can fail). `SigningKey::
    // generate` needs `CryptoRng`, so wrap it in `UnwrapErr`, which turns any
    // `TryCryptoRng` into an infallible `CryptoRng` (panicking on the error
    // path, same as the old `OsRng` effectively did).
    use ed25519_dalek::rand_core::UnwrapErr;
    use rand::rngs::SysRng;

    let mut csprng = UnwrapErr(SysRng);
    let signing_key = ed25519_dalek::SigningKey::generate(&mut csprng);

    std::fs::create_dir_all(&opts.output_dir)?;

    let private_path = opts.output_dir.join("private.pem");
    let public_path = opts.output_dir.join("public.pem");

    // `write_pkcs8_pem_file` still exists on `EncodePrivateKey`, but it's
    // gated behind pkcs8's own `std` feature — which ed25519-dalek's `pem`
    // feature no longer forwards (ed25519-dalek dropped its own `std`
    // feature in 3.0). Encode to a PEM string and write it ourselves instead,
    // matching how `verify.rs` already reads public keys manually.
    let private_pem = signing_key
        .to_pkcs8_pem(Default::default())
        .map_err(|e| KeygenError::Pkcs8(e.to_string()))?;
    std::fs::write(&private_path, private_pem.as_bytes())?;

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
