/// sembundle library interface.
///
/// Exposes the full `.sembundle` build/read/sign pipeline and core types so that
/// tools like `sempkg` can call the pack/build/verify logic in-process — and, by
/// depending on this one crate, share the archive-layout and signing conventions
/// instead of re-implementing (and drifting from) them.
pub mod build;
pub mod checksum;
pub mod consts;
pub mod error;
pub mod keygen;
pub mod manifest;
pub mod pack;
pub mod publish;
pub mod reader;
pub mod sign;
pub mod validate;
pub mod verify;

// Re-export the most commonly needed items at crate root for convenience.
pub use build::{build, BuildOptions};
pub use checksum::sha256_bytes;
pub use error::PackError;
pub use keygen::{keygen, KeygenOptions};
pub use manifest::Manifest;
pub use pack::{pack, PackOptions};
pub use reader::{read_manifest, verify_checksums, ReadError};
pub use sign::{sign, SignOptions};
pub use validate::validate_name;
pub use verify::{verify, verify_signature, VerifyOptions, VerifyingKey};
