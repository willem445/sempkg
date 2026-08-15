# Backward-compatibility fixtures (ed25519-dalek 2.2.0 → 3.0 migration)

These fixtures were generated with the **pre-migration** `sembundle` binary
(`ed25519-dalek` 2.2.0, `pkcs8` 0.10.2) so they are provably old-format. They
pin two invariants that must survive the ed25519-dalek 3.0 migration
(see #102 / the `deps/ed25519-dalek-3` PR):

  (a) A PKCS#8 PEM key file written by the old code must still load under the
      new code.
  (b) A signature produced by the old code must still verify under the new
      code (and a signature the new code produces over the same bytes with
      the same key must be byte-identical, since Ed25519 signing is
      deterministic per RFC 8032).

## `self_generated_v2/`

Generated on this branch, before any migration code changed, by building the
workspace at the branch point (main @ bd5809f, `ed25519-dalek = "2"`) and
running the real CLI:

```
sembundle key-gen --output-dir tests/fixtures/self_generated_v2
sembundle sign --key tests/fixtures/self_generated_v2/private.pem \
    tests/fixtures/self_generated_v2/fixture.sembundle
```

- `private.pem` / `public.pem` — a throwaway Ed25519 keypair (not used for
  anything else; safe to keep committed).
- `fixture.sembundle` — arbitrary bytes standing in for a real bundle.
- `fixture.sembundle.sig` — the old code's hex-encoded signature over
  `fixture.sembundle`'s SHA-256 digest, using `private.pem`.

## `beta1_release/`

Downloaded verbatim from the real `v1.0.0-beta.1` GitHub release
(`gh release download v1.0.0-beta.1 --repo willem445/sempkg`), which shipped
under `ed25519-dalek` 2.2.0 — i.e. genuine production artifacts, not a test
double:

- `public.pem` — the published verifying key.
- `sempkg-1.0.0-beta.1.sembundle` — the published bundle.
- `sempkg-1.0.0-beta.1.sembundle.sig` — the published signature.

No private key is available for this keypair (as it shouldn't be), so this
set only proves the verify direction — but against a real signature real
users already rely on, not a synthetic one.
