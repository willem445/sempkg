/// sembundle library interface.
///
/// Exposes the build pipeline and core types so that tools like `sempkg` can
/// call the pack/build logic in-process without shelling out to the sembundle binary.
pub mod build;
pub mod checksum;
pub mod error;
pub mod manifest;
pub mod pack;
pub mod validate;

// Re-export the most commonly needed items at crate root for convenience.
pub use build::{build, BuildOptions};
pub use error::PackError;
pub use pack::{pack, PackOptions};
pub use validate::validate_name;
