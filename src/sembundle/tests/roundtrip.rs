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
use sembundle::reader::read_entries;
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

#[test]
fn verify_checksums_detects_missing_file() {
    // A file listed in manifest.checksums but absent from the archive must be
    // rejected — otherwise a truncated/stripped bundle installs cleanly and only
    // fails later at query time.
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input");
    make_input(&input);
    let output = tmp.path().join("out.sembundle");
    pack(pack_opts(input, output.clone())).unwrap();

    let bytes = fs::read(&output).unwrap();
    let mut manifest = read_manifest(&bytes).unwrap();

    // Record a checksum for a file that was never packed into the archive.
    manifest
        .checksums
        .insert("graph/does-not-exist.bin".to_string(), "0".repeat(64));

    let err = verify_checksums(&bytes, &manifest).unwrap_err();
    assert!(
        matches!(err, sembundle::reader::ReadError::MissingFile(ref f) if f == "graph/does-not-exist.bin"),
        "expected MissingFile, got: {err}"
    );
}

// ── determinism (the reproducibility fix) ────────────────────────────────────

/// Extract the checksum-map keys in the order they physically appear in the
/// pretty-printed `manifest.json` text — i.e. the on-disk / signed order —
/// *without* deserializing (which would re-sort through the `BTreeMap` and hide
/// the very ordering we want to assert about).
///
/// `checksums` is the last field in the manifest and its values are plain hex
/// strings, so the first `}` after `"checksums": {` closes the object and no
/// nested braces appear inside it.
fn checksum_keys_in_document_order(manifest_json: &str) -> Vec<String> {
    let open = manifest_json
        .find("\"checksums\"")
        .expect("manifest has a checksums block");
    let brace = open + manifest_json[open..].find('{').expect("checksums opens");
    let close = brace + manifest_json[brace..].find('}').expect("checksums closes");
    manifest_json[brace + 1..close]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix('"')?;
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .collect()
}

#[test]
fn manifest_checksum_order_is_deterministic() {
    // The reproducibility fix stores `checksums` in a BTreeMap so the *serialized*
    // manifest — the bytes that get signed — always lists checksum keys in sorted
    // order. Under the previous HashMap the on-disk key order was randomized per
    // process, so a signed manifest wasn't reproducible.
    //
    // We assert on the RAW manifest bytes from the archive: reading back through
    // `read_manifest` deserializes into a BTreeMap and would re-sort, masking a
    // regression to an unordered map. (We deliberately do NOT compare full bundle
    // bytes across packs: `metadata.json` embeds a live `created_at`, so bundles
    // are not byte-reproducible across wall-clock — only the key *ordering* is
    // what BTreeMap makes deterministic.)
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input");
    make_input(&input);

    let raw_keys = |out: &Path| -> Vec<String> {
        let entries = read_entries(&fs::read(out).unwrap()).unwrap();
        let (_, manifest_bytes) = entries
            .iter()
            .find(|(k, _)| k == "manifest.json")
            .expect("manifest.json present in archive");
        checksum_keys_in_document_order(std::str::from_utf8(manifest_bytes).unwrap())
    };

    let out_a = tmp.path().join("a.sembundle");
    let out_b = tmp.path().join("b.sembundle");
    pack(pack_opts(input.clone(), out_a.clone())).unwrap();
    pack(pack_opts(input, out_b.clone())).unwrap();

    let keys_a = raw_keys(&out_a);
    let keys_b = raw_keys(&out_b);

    // Several files, so an unordered map would almost never land sorted by chance.
    assert!(
        keys_a.len() >= 3,
        "expected several checksummed files, got {keys_a:?}"
    );

    // 1. On-disk keys are in sorted order (the BTreeMap invariant on the signed bytes).
    let mut sorted = keys_a.clone();
    sorted.sort();
    assert_eq!(
        keys_a, sorted,
        "on-disk checksum keys must be serialized in sorted order"
    );

    // 2. Two packs of identical input produce the identical on-disk key ordering
    //    (compares keys, not timestamp-bearing values — so this is not flaky).
    assert_eq!(
        keys_a, keys_b,
        "identical inputs must produce identical on-disk checksum key ordering"
    );
}
