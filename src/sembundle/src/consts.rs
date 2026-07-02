//! Canonical `.sembundle` layout constants.
//!
//! These are the "magic" file/directory names and spec-version strings that make
//! up the on-disk bundle contract. They live in one place so the writer (`pack`)
//! and the readers (`reader`, and `sempkg` via delegation) can never disagree on
//! them. Spec: sembundle-spec.md §4–§9.

/// The bundle manifest file, at the bundle root. Not itself checksummed.
pub const MANIFEST_FILE: &str = "manifest.json";

/// The source-metadata file, at the bundle root.
pub const METADATA_FILE: &str = "metadata.json";

/// CodeGraph config file copied verbatim into the bundle root.
pub const CONFIG_FILE: &str = "config.json";

/// CodeGraph graph output directory.
pub const GRAPH_DIR: &str = "graph";

/// CodeGraph embeddings output directory.
pub const EMBEDDINGS_DIR: &str = "embeddings";

/// LanceDB documentation-index extension directory.
pub const LANCE_DIR: &str = "lance";

/// LanceDB source-code-index extension directory.
pub const CODE_DIR: &str = "code";

/// `extensions` manifest value declaring a `lance/` directory is present.
pub const LANCE_EXT: &str = "lance";

/// `extensions` manifest value declaring a `code/` directory is present.
pub const CODE_EXT: &str = "code";

/// Spec version stamped when the bundle has no `code/` extension (may still have
/// `lance/`). Spec: sembundle-spec.md §4.
pub const SPEC_VERSION_LANCE: &str = "1.2.0";

/// Spec version stamped when the bundle includes the `code/` extension.
pub const SPEC_VERSION_CODE: &str = "1.3.0";
