//! End-to-end round-trip tests for the shared `.sembundle` layer.
//!
//! These live at the crate boundary (integration tests, using only the public
//! library surface) so that any drift between the writer side (`pack`, `sign`)
//! and the reader side (`read_manifest`, `verify_checksums`, `verify`) is caught
//! by CI — the exact failure mode the shared layer exists to prevent.

use std::fs;
use std::path::{Path, PathBuf};

use sembundle::keygen::{keygen, KeygenOptions};
use sembundle::pack::{pack, PackOptions};
use sembundle::sign::{sign, SignOptions};
use sembundle::verify::{verify, VerifyOptions};
use sembundle::{read_manifest, verify_checksums};
use tempfile::TempDir;

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Build a minimal but valid CodeGraph output directory.
fn make_input(dir: &Path) {
    fs::create_dir_all(dir.join("graph")).unwrap();
    fs::create_dir_all(dir.join("embeddings")).unwrap();
    fs::write(dir.join("config.json"), b"{}").unwrap();
    fs::write(dir.join("graph").join("nodes.bin"), b"graph-data").unwrap();
    fs::write(dir.join("embeddings").join("vectors.bin"), b"emb-data").unwrap();
}

fn pack_opts(input_dir: PathBuf, output: PathBuf) -> PackOptions {
    PackOptions {
        input_dir,
        output_path: Some(output),
        name: "round-trip".to_string(),
        version: "1.0.0".to_string(),
        source_repo: "https://github.com/example/round-trip".to_string(),
        commit_hash: "a".repeat(40),
        tag: Some("v1.0.0".to_string()),
        language: "rust".to_string(),
        indexed_paths: vec!["src".to_string()],
        codegraph_version: "0.3.1".to_string(),
        lance_dir: None,
        code_dir: None,
    }
}

// ── keygen → sign → verify ────────────────────────────────────────────────────

#[test]
fn keygen_sign_verify_happy_path() {
    let tmp = TempDir::new().unwrap();
    let (private_pem, public_pem) = keygen(KeygenOptions {
        output_dir: tmp.path().join("keys"),
    })
    .unwrap();

    // Any bytes stand in for a bundle — sign/verify operate over the file bytes.
    let bundle = tmp.path().join("artifact.sembundle");
    fs::write(&bundle, b"pretend this is a real bundle").unwrap();

    let sig = sign(SignOptions {
        bundle_path: bundle.clone(),
        private_key_path: private_pem,
        output: None,
    })
    .unwrap();

    verify(VerifyOptions {
        bundle_path: bundle,
        sig_path: sig,
        public_key_path: public_pem,
    })
    .expect("freshly signed bundle should verify");
}

#[test]
fn verify_fails_when_bundle_is_tampered() {
    let tmp = TempDir::new().unwrap();
    let (private_pem, public_pem) = keygen(KeygenOptions {
        output_dir: tmp.path().join("keys"),
    })
    .unwrap();

    let bundle = tmp.path().join("artifact.sembundle");
    fs::write(&bundle, b"original contents").unwrap();
    let sig = sign(SignOptions {
        bundle_path: bundle.clone(),
        private_key_path: private_pem,
        output: None,
    })
    .unwrap();

    // Flip the bundle contents after signing.
    fs::write(&bundle, b"tampered contents").unwrap();

    let err = verify(VerifyOptions {
        bundle_path: bundle,
        sig_path: sig,
        public_key_path: public_pem,
    })
    .unwrap_err();
    assert!(
        matches!(err, sembundle::verify::VerifyError::VerificationFailed),
        "expected VerificationFailed, got: {err}"
    );
}

#[test]
fn verify_fails_with_wrong_key() {
    let tmp = TempDir::new().unwrap();
    let (signing_priv, _signing_pub) = keygen(KeygenOptions {
        output_dir: tmp.path().join("signer"),
    })
    .unwrap();
    let (_other_priv, other_pub) = keygen(KeygenOptions {
        output_dir: tmp.path().join("other"),
    })
    .unwrap();

    let bundle = tmp.path().join("artifact.sembundle");
    fs::write(&bundle, b"contents").unwrap();
    let sig = sign(SignOptions {
        bundle_path: bundle.clone(),
        private_key_path: signing_priv,
        output: None,
    })
    .unwrap();

    // Verifying against an unrelated public key must fail.
    let err = verify(VerifyOptions {
        bundle_path: bundle,
        sig_path: sig,
        public_key_path: other_pub,
    })
    .unwrap_err();
    assert!(matches!(
        err,
        sembundle::verify::VerifyError::VerificationFailed
    ));
}

// ── pack → read_manifest → verify_checksums ──────────────────────────────────

#[test]
fn pack_read_manifest_and_verify_checksums_round_trip() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input");
    make_input(&input);
    let output = tmp.path().join("out.sembundle");
    pack(pack_opts(input, output.clone())).unwrap();

    let bytes = fs::read(&output).unwrap();

    let manifest = read_manifest(&bytes).expect("manifest should read back");
    assert_eq!(manifest.name, "round-trip");
    assert_eq!(manifest.version, "1.0.0");
    assert!(!manifest.checksums.is_empty());

    verify_checksums(&bytes, &manifest).expect("checksums should validate");
}

#[test]
fn verify_checksums_detects_mismatch() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input");
    make_input(&input);
    let output = tmp.path().join("out.sembundle");
    pack(pack_opts(input, output.clone())).unwrap();

    let bytes = fs::read(&output).unwrap();
    let mut manifest = read_manifest(&bytes).unwrap();

    // Corrupt one recorded checksum; validation must reject the bundle.
    let key = manifest.checksums.keys().next().unwrap().clone();
    manifest.checksums.insert(key, "0".repeat(64));

    let err = verify_checksums(&bytes, &manifest).unwrap_err();
    assert!(
        matches!(err, sembundle::reader::ReadError::ChecksumMismatch { .. }),
        "expected ChecksumMismatch, got: {err}"
    );
}

// ── determinism (the reproducibility fix) ────────────────────────────────────

#[test]
fn manifest_checksum_order_is_deterministic() {
    // Two packs of the same input must produce byte-identical checksum maps in
    // the serialized manifest (BTreeMap ⇒ sorted, stable key order), which is
    // what makes the signed manifest reproducible.
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input");
    make_input(&input);

    let out_a = tmp.path().join("a.sembundle");
    let out_b = tmp.path().join("b.sembundle");
    pack(pack_opts(input.clone(), out_a.clone())).unwrap();
    pack(pack_opts(input, out_b.clone())).unwrap();

    let m_a = read_manifest(&fs::read(&out_a).unwrap()).unwrap();
    let m_b = read_manifest(&fs::read(&out_b).unwrap()).unwrap();

    // Serialized checksum maps are identical and sorted.
    let keys_a: Vec<&String> = m_a.checksums.keys().collect();
    let mut sorted = keys_a.clone();
    sorted.sort();
    assert_eq!(
        keys_a, sorted,
        "checksum keys must serialize in sorted order"
    );
    assert_eq!(
        serde_json::to_string(&m_a.checksums).unwrap(),
        serde_json::to_string(&m_b.checksums).unwrap(),
        "identical inputs must produce identical checksum serialization"
    );
}
