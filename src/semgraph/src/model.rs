//! Full schema-v4 record types for the write path (issue #78, Phase 2a).
//!
//! The reader ([`crate::GraphNode`]) is a *read projection* carrying only the
//! columns the query surfaces consume. The writer, by contrast, has to
//! round-trip the whole schema, so this module defines records that mirror the
//! `nodes` / `edges` / `files` tables column-for-column. Keeping them separate
//! from [`crate::GraphNode`] is deliberate: it keeps the reader's public API
//! (and its tests) untouched while the writer owns the fuller shape.
//!
//! ## Node id format
//!
//! Node ids are `"<kind>:<hash>"`, matching CodeGraph's observed format. For
//! `file` nodes the id is the literal `"file:<file_path>"` (CodeGraph does the
//! same — the path is already unique). For every other node the hash is the
//! first 16 bytes of `SHA-256(qualified_name \0 file_path)`, rendered as 32 hex
//! characters. Nothing in sempkg depends on the *content* of the hash — only on
//! id equality within a single database — so any stable function suffices (see
//! issue #78, "Node-id format drift"). Including `file_path` in the hash keeps
//! same-named symbols in different files distinct; the `kind` prefix keeps a
//! class and a same-named function apart.

use sha2::{Digest, Sha256};

/// A source language the parse layer understands (tier-1 + tier-2 rollout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    /// TypeScript (`.ts`, `.mts`, `.cts`) — parsed with the `typescript` grammar.
    TypeScript,
    /// TSX (`.tsx`) — parsed with the `tsx` grammar dialect.
    Tsx,
    /// JavaScript / JSX (`.js`, `.jsx`, `.mjs`, `.cjs`) — parsed with the `tsx`
    /// grammar, which is a superset that accepts plain JS and JSX.
    JavaScript,
    /// C (`.c`, `.h`) — tier-2.
    C,
    /// C++ (`.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`) — tier-2.
    Cpp,
    /// Go (`.go`) — tier-2.
    Go,
    /// Java (`.java`) — tier-2.
    Java,
}

impl Language {
    /// Infer the language from a file extension, or `None` if unsupported.
    pub fn from_extension(ext: &str) -> Option<Language> {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Language::Rust),
            "py" | "pyi" => Some(Language::Python),
            "ts" | "mts" | "cts" => Some(Language::TypeScript),
            "tsx" => Some(Language::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
            "c" | "h" => Some(Language::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Language::Cpp),
            "go" => Some(Language::Go),
            "java" => Some(Language::Java),
            _ => None,
        }
    }

    /// Infer the language from a path's extension.
    pub fn from_path(path: &std::path::Path) -> Option<Language> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension)
    }

    /// The string stored in `nodes.language` / `files.language`. Matches
    /// CodeGraph: `.ts`/`.tsx` → `"typescript"`, `.js`/`.jsx` → `"javascript"`.
    pub fn db_name(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::TypeScript | Language::Tsx => "typescript",
            Language::JavaScript => "javascript",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Go => "go",
            Language::Java => "java",
        }
    }
}

/// A node destined for the `nodes` table — every schema-v4 column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub language: String,
    pub start_line: u32,
    pub end_line: u32,
    pub start_column: u32,
    pub end_column: u32,
    pub docstring: Option<String>,
    pub signature: Option<String>,
    pub visibility: Option<String>,
    pub is_exported: bool,
    pub is_async: bool,
    pub is_static: bool,
    pub is_abstract: bool,
    /// JSON array string (e.g. `["@staticmethod"]`) or `None`.
    pub decorators: Option<String>,
    /// JSON array string (e.g. `["T","U"]`) or `None`.
    pub type_parameters: Option<String>,
    /// Epoch milliseconds when this node was written.
    pub updated_at: i64,
}

impl NodeRecord {
    /// Construct a node, deriving its id from `kind` + `qualified_name` +
    /// `file_path`. All optional/flag fields start empty; callers set what the
    /// language provides.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: impl Into<String>,
        name: impl Into<String>,
        qualified_name: impl Into<String>,
        file_path: impl Into<String>,
        language: impl Into<String>,
        start_line: u32,
        end_line: u32,
        start_column: u32,
        end_column: u32,
        updated_at: i64,
    ) -> NodeRecord {
        let kind = kind.into();
        let qualified_name = qualified_name.into();
        let file_path = file_path.into();
        let id = node_id(&kind, &qualified_name, &file_path);
        NodeRecord {
            id,
            kind,
            name: name.into(),
            qualified_name,
            file_path,
            language: language.into(),
            start_line,
            end_line,
            start_column,
            end_column,
            docstring: None,
            signature: None,
            visibility: None,
            is_exported: false,
            is_async: false,
            is_static: false,
            is_abstract: false,
            decorators: None,
            type_parameters: None,
            updated_at,
        }
    }
}

/// An edge destined for the `edges` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRecord {
    pub source: String,
    pub target: String,
    pub kind: String,
    /// JSON object string, or `None`.
    pub metadata: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    pub provenance: Option<String>,
}

impl EdgeRecord {
    /// A structural `contains` edge from `source` to `target` (no line/col).
    pub fn contains(source: impl Into<String>, target: impl Into<String>) -> EdgeRecord {
        EdgeRecord {
            source: source.into(),
            target: target.into(),
            kind: "contains".to_string(),
            metadata: None,
            line: None,
            col: None,
            provenance: None,
        }
    }
}

/// A row destined for the `files` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub path: String,
    /// SHA-256 hex of the file's bytes — the Phase 2b incremental-sync anchor
    /// (see [`content_hash`]).
    pub content_hash: String,
    pub language: String,
    pub size: u64,
    pub modified_at: i64,
    pub indexed_at: i64,
    pub node_count: u64,
    /// JSON array string of parse errors, or `None`.
    pub errors: Option<String>,
}

/// Compute a node id `"<kind>:<hash>"`.
///
/// `file` nodes use the literal path (`"file:<file_path>"`); all others hash
/// `qualified_name \0 file_path` with SHA-256 and take the first 16 bytes.
pub fn node_id(kind: &str, qualified_name: &str, file_path: &str) -> String {
    if kind == "file" {
        return format!("file:{file_path}");
    }
    let mut hasher = Sha256::new();
    hasher.update(qualified_name.as_bytes());
    hasher.update([0u8]);
    hasher.update(file_path.as_bytes());
    let digest = hasher.finalize();
    format!("{kind}:{}", hex::encode(&digest[..16]))
}

/// SHA-256 hex digest of `bytes`.
///
/// This is the `files.content_hash` value and the anchor for Phase 2b
/// incremental sync: a file whose bytes are unchanged produces the same hash,
/// so re-indexing can skip it. SHA-256 (64 hex chars) matches the width
/// CodeGraph records, so the column stays format-compatible.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn file_node_id_is_literal_path() {
        assert_eq!(
            node_id("file", "python/main.py", "python/main.py"),
            "file:python/main.py"
        );
    }

    #[test]
    fn non_file_node_id_is_kind_prefixed_32_hex() {
        let id = node_id("function", "circle_area", "python/shapes.py");
        let (kind, hash) = id.split_once(':').unwrap();
        assert_eq!(kind, "function");
        assert_eq!(hash.len(), 32);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn node_id_distinguishes_by_file_and_kind() {
        // Same qualified name, different files → different ids.
        assert_ne!(
            node_id("function", "hypot", "rust/geometry.rs"),
            node_id("function", "hypot", "typescript/geometry.ts"),
        );
        // Same qualified name + file, different kind → different ids.
        assert_ne!(
            node_id("class", "Point", "geometry.ts"),
            node_id("method", "Point", "geometry.ts"),
        );
        // Deterministic.
        assert_eq!(
            node_id("function", "hypot", "rust/geometry.rs"),
            node_id("function", "hypot", "rust/geometry.rs"),
        );
    }

    #[test]
    fn content_hash_is_stable_sha256_hex() {
        let h = content_hash(b"hello world");
        assert_eq!(h.len(), 64);
        assert_eq!(
            h,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn language_from_path_covers_tier1() {
        assert_eq!(Language::from_path(Path::new("a.rs")), Some(Language::Rust));
        assert_eq!(
            Language::from_path(Path::new("a.py")),
            Some(Language::Python)
        );
        assert_eq!(
            Language::from_path(Path::new("a.ts")),
            Some(Language::TypeScript)
        );
        assert_eq!(Language::from_path(Path::new("a.tsx")), Some(Language::Tsx));
        assert_eq!(
            Language::from_path(Path::new("a.jsx")),
            Some(Language::JavaScript)
        );
        assert_eq!(Language::from_path(Path::new("a.txt")), None);
        assert_eq!(Language::from_path(Path::new("Makefile")), None);
    }

    #[test]
    fn db_name_maps_dialects() {
        assert_eq!(Language::TypeScript.db_name(), "typescript");
        assert_eq!(Language::Tsx.db_name(), "typescript");
        assert_eq!(Language::JavaScript.db_name(), "javascript");
        assert_eq!(Language::Rust.db_name(), "rust");
    }
}
