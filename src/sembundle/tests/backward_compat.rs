//! Backward-compatibility fixtures for the ed25519-dalek 2.2.0 → 3.0 migration
//! (#102 / deps/ed25519-dalek-3).
//!
//! Every fixture under `tests/fixtures/` was produced by the *old* code
//! (ed25519-dalek 2.2.0, pkcs8 0.10.2) — see `tests/fixtures/README.md` for
//! exact provenance. These tests pin two invariants across the migration:
//!
//!   (a) A PKCS#8 PEM private key written by the old code must still LOAD
//!       under the new code, and must reproduce the exact old signature
//!       bytes (Ed25519 signing is deterministic per RFC 8032 — same key,
//!       same message, same signature, regardless of library version).
//!   (b) A signature produced by the old code must still VERIFY under the
//!       new code.
//!
//! If either of these breaks, every `private.pem`/`public.pem` a user
//! already has on disk, and every already-published `.sembundle.sig`
//! (including the real `v1.0.0-beta.1` release), stops working.

use std::fs;
use std::path::PathBuf;

use sembundle::sign::{sign, SignOptions};
use sembundle::verify::{verify, VerifyOptions};
use tempfile::TempDir;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

// ── self-generated fixture (old key-gen + old sign) ─────────────────────────

/// (a) The old-format private key loads under the new code, and re-signing
/// the exact same bundle bytes with it reproduces the exact old signature —
/// byte for byte, not just "verifies".
#[test]
fn old_private_key_loads_and_reproduces_old_signature() {
    let fixtures = fixtures_dir().join("self_generated_v2");
    let tmp = TempDir::new().unwrap();
    let output = tmp.path().join("fixture.sembundle.sig");

    sign(SignOptions {
        bundle_path: fixtures.join("fixture.sembundle"),
        private_key_path: fixtures.join("private.pem"),
        output: Some(output.clone()),
    })
    .expect("old-format PKCS#8 PEM private key must still load and sign under the new code");

    let fresh_sig = fs::read_to_string(&output).unwrap();
    let old_sig = fs::read_to_string(fixtures.join("fixture.sembundle.sig")).unwrap();
    assert_eq!(
        fresh_sig.trim(),
        old_sig.trim(),
        "re-signing with the old key must reproduce the exact old signature bytes \
         (Ed25519 signing is deterministic) — a mismatch means the migration changed \
         either the key material derived from the PEM or the signing algorithm itself"
    );
}

/// (b) A signature produced by the old code verifies under the new code,
/// using the old-format public key.
#[test]
fn old_signature_verifies_under_new_code() {
    let fixtures = fixtures_dir().join("self_generated_v2");

    verify(VerifyOptions {
        bundle_path: fixtures.join("fixture.sembundle"),
        sig_path: fixtures.join("fixture.sembundle.sig"),
        public_key_path: fixtures.join("public.pem"),
    })
    .expect("a signature produced by the old code must verify under the new code");
}

// ── real production fixture (actual v1.0.0-beta.1 GitHub release assets) ───

/// The strongest possible evidence for (b): the actual bundle, signature and
/// public key published on the v1.0.0-beta.1 GitHub release — real bytes an
/// existing `sempkg` install already trusts — must still verify. If this
/// regresses, every existing user's install starts rejecting a bundle it
/// previously accepted.
#[test]
fn real_published_beta1_signature_verifies_under_new_code() {
    let fixtures = fixtures_dir().join("beta1_release");

    verify(VerifyOptions {
        bundle_path: fixtures.join("sempkg-1.0.0-beta.1.sembundle"),
        sig_path: fixtures.join("sempkg-1.0.0-beta.1.sembundle.sig"),
        public_key_path: fixtures.join("public.pem"),
    })
    .expect(
        "the real v1.0.0-beta.1 release signature must still verify under the new code — \
         a failure here means existing sempkg installs would start rejecting an \
         already-published, already-trusted bundle",
    );
}
