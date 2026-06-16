use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use flate2::write::GzEncoder;
use flate2::Compression;
use walkdir::WalkDir;

use crate::checksum::sha256_bytes;
use crate::error::PackError;
use crate::manifest::{LanceMetadata, Manifest, Metadata};
use crate::validate::{validate_commit_hash, validate_input_dir, validate_lance_dir, validate_name};

/// Options for the `pack` operation.
pub struct PackOptions {
    /// Path to the CodeGraph output directory containing `graph/`, `embeddings/`, `config.json`.
    pub input_dir: PathBuf,
    /// Where to write the `.sembundle` file. Defaults to `./<name>-<version>.sembundle`.
    pub output_path: Option<PathBuf>,
    pub name: String,
    pub version: String,
    pub source_repo: String,
    /// Full 40-character lowercase Git SHA.
    pub commit_hash: String,
    pub tag: Option<String>,
    /// Primary language indexed (e.g. `"python"`, `"cpp"`).
    pub language: String,
    /// Repo-relative paths that were indexed. Defaults to `["."]` if empty.
    pub indexed_paths: Vec<String>,
    /// Version of CodeGraph used to produce the index.
    pub codegraph_version: String,
    /// Optional path to a pre-built LanceDB index directory.
    ///
    /// When supplied the directory must contain:
    /// `metadata.json` and at least one `*.lance/` table directory.
    /// Spec: sembundle-spec.md §9.
    pub lance_dir: Option<PathBuf>,
}

/// In-memory bundle entry: bundle-relative key + raw bytes.
struct Entry {
    /// Forward-slash path relative to the bundle root (e.g. `"graph/nodes.bin"`).
    key: String,
    content: Vec<u8>,
}

/// Pack a CodeGraph output directory into a `.sembundle` gzip tar archive.
///
/// Returns the path of the produced bundle file on success.
///
/// # Errors
/// Returns [`PackError`] for validation failures, I/O errors, or serialization errors.
pub fn pack(opts: PackOptions) -> Result<PathBuf, PackError> {
    // --- Validate inputs ---
    validate_name(&opts.name)?;
    validate_commit_hash(&opts.commit_hash)?;
    validate_input_dir(&opts.input_dir)?;
    if let Some(ref lance) = opts.lance_dir {
        validate_lance_dir(lance)?;
    }

    let created_at = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let prefix = format!("{}-{}", opts.name, opts.version);

    // --- Collect non-manifest entries ---
    let mut entries: Vec<Entry> = Vec::new();

    // config.json (verbatim copy from input dir)
    entries.push(Entry {
        key: "config.json".to_string(),
        content: std::fs::read(opts.input_dir.join("config.json"))?,
    });

    // graph/** and embeddings/** (sorted for determinism)
    collect_dir(&opts.input_dir.join("graph"), "graph", &mut entries)?;
    collect_dir(
        &opts.input_dir.join("embeddings"),
        "embeddings",
        &mut entries,
    )?;

    // metadata.json — generated from CLI args
    let indexed_paths = if opts.indexed_paths.is_empty() {
        vec![".".to_string()]
    } else {
        opts.indexed_paths.clone()
    };
    let metadata = Metadata {
        name: opts.name.clone(),
        version: opts.version.clone(),
        source_repo: opts.source_repo.clone(),
        commit_hash: opts.commit_hash.clone(),
        tag: opts.tag.clone(),
        language: opts.language.clone(),
        indexed_paths,
        created_at: created_at.clone(),
    };
    entries.push(Entry {
        key: "metadata.json".to_string(),
        content: serde_json::to_vec_pretty(&metadata)?,
    });

    // --- Optional Lance extension ---
    let mut extensions: Vec<String> = Vec::new();
    if let Some(ref lance_dir) = opts.lance_dir {
        collect_lance_entries(lance_dir, &created_at, &mut entries)?;
        extensions.push("lance".to_string());
    }

    // --- Compute checksums for all non-manifest files ---
    let mut checksums: HashMap<String, String> = HashMap::new();
    for e in &entries {
        checksums.insert(e.key.clone(), sha256_bytes(&e.content));
    }

    // --- Generate manifest.json (checksums are now final) ---
    let manifest = Manifest {
        spec_version: "1.2.0".to_string(),
        name: opts.name.clone(),
        version: opts.version.clone(),
        source_repo: opts.source_repo.clone(),
        commit_hash: opts.commit_hash.clone(),
        tag: opts.tag.clone(),
        created_at,
        codegraph_version: opts.codegraph_version.clone(),
        extensions,
        checksums,
    };
    // Insert manifest first so it appears first in the archive listing.
    entries.insert(
        0,
        Entry {
            key: "manifest.json".to_string(),
            content: serde_json::to_vec_pretty(&manifest)?,
        },
    );

    // --- Write archive ---
    let output_path = opts
        .output_path
        .unwrap_or_else(|| PathBuf::from(format!("{}.sembundle", prefix)));

    {
        let file = std::fs::File::create(&output_path)?;
        let gz = GzEncoder::new(file, Compression::best());
        let mut builder = tar::Builder::new(gz);

        for entry in &entries {
            let archive_path = format!("{}/{}", prefix, entry.key);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(entry.content.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0); // deterministic mtime
            header.set_cksum();
            builder.append_data(
                &mut header,
                &archive_path,
                entry.content.as_slice(),
            )?;
        }

        let gz = builder.into_inner()?;
        gz.finish()?;
    }

    Ok(output_path)
}

/// Recursively collect all files under `dir`, adding them to `entries` with
/// keys like `<dir_prefix>/relative/path`.
fn collect_dir(dir: &Path, dir_prefix: &str, entries: &mut Vec<Entry>) -> Result<(), PackError> {
    for result in WalkDir::new(dir).min_depth(1).sort_by_file_name() {
        let entry = result?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(dir).unwrap();
        // Build a forward-slash relative key regardless of host OS.
        let rel_key: String = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        entries.push(Entry {
            key: format!("{dir_prefix}/{rel_key}"),
            content: std::fs::read(entry.path())?,
        });
    }
    Ok(())
}

/// Collect all LanceDB index files into `lance/`-prefixed bundle entries.
///
/// - `lance/metadata.json`   — parsed, `created_at` stamped, re-serialised
/// - `lance/<table>.lance/**` — all Arrow/Lance data files copied verbatim
fn collect_lance_entries(
    lance_dir: &Path,
    created_at: &str,
    entries: &mut Vec<Entry>,
) -> Result<(), PackError> {
    // Stamp metadata.json with the bundle's created_at (spec §11.5)
    let meta_path = lance_dir.join("metadata.json");
    if meta_path.is_file() {
        let raw = std::fs::read(&meta_path)?;
        let mut meta: LanceMetadata = serde_json::from_slice(&raw)?;
        meta.created_at = created_at.to_string();
        entries.push(Entry {
            key: "lance/metadata.json".to_string(),
            content: serde_json::to_vec_pretty(&meta)?,
        });
    }

    // Copy everything else in the lance directory recursively (table files, FTS index, etc.)
    for result in WalkDir::new(lance_dir).min_depth(1).sort_by_file_name() {
        let entry = result?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(lance_dir).unwrap();
        let rel_key: String = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");

        // metadata.json already handled above
        if rel_key == "metadata.json" {
            continue;
        }

        entries.push(Entry {
            key: format!("lance/{rel_key}"),
            content: std::fs::read(entry.path())?,
        });
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::io::Read as _;
    use std::path::{Path, PathBuf};

    use flate2::read::GzDecoder;
    use tempfile::TempDir;

    use crate::checksum::sha256_bytes;
    use crate::error::PackError;
    use crate::manifest::Manifest;

    use super::{pack, PackOptions};

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Build a minimal but valid CodeGraph output directory.
    fn make_input(dir: &Path) {
        fs::create_dir_all(dir.join("graph")).unwrap();
        fs::create_dir_all(dir.join("embeddings")).unwrap();
        fs::write(dir.join("config.json"), b"{}").unwrap();
        fs::write(dir.join("graph").join("nodes.bin"), b"graph-data").unwrap();
        fs::write(dir.join("embeddings").join("vectors.bin"), b"emb-data").unwrap();
    }

    fn default_opts(input_dir: PathBuf, output: PathBuf) -> PackOptions {
        PackOptions {
            input_dir,
            output_path: Some(output),
            name: "my-sdk".to_string(),
            version: "1.0.0".to_string(),
            source_repo: "https://github.com/example/my-sdk".to_string(),
            commit_hash: "a".repeat(40),
            tag: Some("v1.0.0".to_string()),
            language: "rust".to_string(),
            indexed_paths: vec!["src".to_string()],
            codegraph_version: "0.3.1".to_string(),
            lance_dir: None,
        }
    }

    /// Build a minimal but valid LanceDB index directory.
    fn make_lance_dir(dir: &Path) {
        let table_dir = dir.join("docs.lance");
        fs::create_dir_all(&table_dir).unwrap();
        fs::write(table_dir.join("0.lance"), b"arrow-data").unwrap();
        let meta = serde_json::json!({
            "table_name": "docs",
            "document_count": 10,
            "chunk_count": 42,
            "indexed_paths": ["docs/**/*.md"],
            "fts_enabled": true,
            "created_at": "1970-01-01T00:00:00Z"
        });
        fs::write(dir.join("metadata.json"), serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
    }

    /// Extract all entries from a `.sembundle` file.
    /// Returns `(manifest_bytes, map_of_key → bytes)` where key excludes the
    /// top-level directory prefix.
    fn extract_bundle(path: &Path) -> (Vec<u8>, HashMap<String, Vec<u8>>) {
        let file = fs::File::open(path).unwrap();
        let gz = GzDecoder::new(file);
        let mut archive = tar::Archive::new(gz);

        let mut manifest_bytes: Option<Vec<u8>> = None;
        let mut files: HashMap<String, Vec<u8>> = HashMap::new();

        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let raw_path = entry.path().unwrap().to_string_lossy().to_string();
            // Strip the `<name>-<version>/` top-level prefix.
            let key = raw_path
                .splitn(2, '/')
                .nth(1)
                .unwrap_or(&raw_path)
                .to_string();
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).unwrap();
            if key == "manifest.json" {
                manifest_bytes = Some(buf);
            } else {
                files.insert(key, buf);
            }
        }

        (manifest_bytes.expect("manifest.json not found in archive"), files)
    }

    // ── Happy-path ───────────────────────────────────────────────────────────

    #[test]
    fn pack_succeeds_with_valid_input() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        make_input(&input);
        let output = tmp.path().join("out.sembundle");

        let result = pack(default_opts(input, output.clone()));
        assert!(result.is_ok(), "pack failed: {:?}", result.err());
        assert!(output.exists(), "output file not written");
    }

    #[test]
    fn default_output_path_derived_from_name_version() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        make_input(&input);

        // Change working directory is risky in tests; instead supply explicit output.
        let output = tmp.path().join("my-sdk-1.0.0.sembundle");
        let mut opts = default_opts(input, output.clone());
        opts.output_path = Some(output.clone());

        pack(opts).unwrap();
        assert!(output.exists());
    }

    // ── Missing directory errors ──────────────────────────────────────────────

    #[test]
    fn error_when_graph_dir_missing() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(input.join("embeddings")).unwrap();
        fs::write(input.join("config.json"), b"{}").unwrap();
        fs::write(input.join("embeddings").join("v.bin"), b"data").unwrap();

        let err = pack(default_opts(input, tmp.path().join("out"))).unwrap_err();
        assert!(
            matches!(err, PackError::MissingDirectory(ref d) if d == "graph"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_when_embeddings_dir_missing() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(input.join("graph")).unwrap();
        fs::write(input.join("config.json"), b"{}").unwrap();
        fs::write(input.join("graph").join("g.bin"), b"data").unwrap();

        let err = pack(default_opts(input, tmp.path().join("out"))).unwrap_err();
        assert!(
            matches!(err, PackError::MissingDirectory(ref d) if d == "embeddings"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_when_config_json_missing() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(input.join("graph")).unwrap();
        fs::create_dir_all(input.join("embeddings")).unwrap();
        fs::write(input.join("graph").join("g.bin"), b"data").unwrap();
        fs::write(input.join("embeddings").join("v.bin"), b"data").unwrap();

        let err = pack(default_opts(input, tmp.path().join("out"))).unwrap_err();
        assert!(
            matches!(err, PackError::MissingFile(ref f) if f == "config.json"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_when_graph_dir_empty() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(input.join("graph")).unwrap(); // empty
        fs::create_dir_all(input.join("embeddings")).unwrap();
        fs::write(input.join("config.json"), b"{}").unwrap();
        fs::write(input.join("embeddings").join("v.bin"), b"data").unwrap();

        let err = pack(default_opts(input, tmp.path().join("out"))).unwrap_err();
        assert!(
            matches!(err, PackError::EmptyDirectory(ref d) if d == "graph"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_when_embeddings_dir_empty() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(input.join("graph")).unwrap();
        fs::create_dir_all(input.join("embeddings")).unwrap(); // empty
        fs::write(input.join("config.json"), b"{}").unwrap();
        fs::write(input.join("graph").join("g.bin"), b"data").unwrap();

        let err = pack(default_opts(input, tmp.path().join("out"))).unwrap_err();
        assert!(
            matches!(err, PackError::EmptyDirectory(ref d) if d == "embeddings"),
            "unexpected error: {err}"
        );
    }

    // ── Field validation errors ───────────────────────────────────────────────

    #[test]
    fn error_on_invalid_name() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        make_input(&input);
        let mut opts = default_opts(input, tmp.path().join("out"));
        opts.name = "My SDK".to_string(); // uppercase + space

        let err = pack(opts).unwrap_err();
        assert!(matches!(err, PackError::InvalidField { .. }), "unexpected: {err}");
    }

    #[test]
    fn error_on_short_commit_hash() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        make_input(&input);
        let mut opts = default_opts(input, tmp.path().join("out"));
        opts.commit_hash = "deadbeef".to_string();

        let err = pack(opts).unwrap_err();
        assert!(matches!(err, PackError::InvalidField { .. }), "unexpected: {err}");
    }

    // ── Checksum correctness ──────────────────────────────────────────────────

    #[test]
    fn checksums_match_file_contents() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        make_input(&input);
        let output = tmp.path().join("out.sembundle");
        pack(default_opts(input, output.clone())).unwrap();

        let (manifest_bytes, files) = extract_bundle(&output);
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();

        for (key, expected) in &manifest.checksums {
            let content = files
                .get(key)
                .unwrap_or_else(|| panic!("file '{key}' in checksums but not in archive"));
            let actual = sha256_bytes(content);
            assert_eq!(&actual, expected, "checksum mismatch for '{key}'");
        }
    }

    #[test]
    fn manifest_not_in_its_own_checksums() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        make_input(&input);
        let output = tmp.path().join("out.sembundle");
        pack(default_opts(input, output.clone())).unwrap();

        let (manifest_bytes, _) = extract_bundle(&output);
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
        assert!(
            !manifest.checksums.contains_key("manifest.json"),
            "manifest.json must not appear in its own checksums"
        );
    }

    // ── Manifest field correctness ────────────────────────────────────────────

    #[test]
    fn manifest_fields_match_pack_options() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        make_input(&input);
        let output = tmp.path().join("out.sembundle");
        pack(default_opts(input, output.clone())).unwrap();

        let (manifest_bytes, _) = extract_bundle(&output);
        let m: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();

        assert_eq!(m.spec_version, "1.2.0");
        assert_eq!(m.name, "my-sdk");
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.source_repo, "https://github.com/example/my-sdk");
        assert_eq!(m.commit_hash, "a".repeat(40));
        assert_eq!(m.tag, Some("v1.0.0".to_string()));
        assert_eq!(m.codegraph_version, "0.3.1");
        assert!(!m.checksums.is_empty());
    }

    #[test]
    fn bundle_contains_all_required_files() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        make_input(&input);
        let output = tmp.path().join("out.sembundle");
        pack(default_opts(input, output.clone())).unwrap();

        let (_, files) = extract_bundle(&output);
        assert!(files.contains_key("metadata.json"), "metadata.json missing");
        assert!(files.contains_key("config.json"), "config.json missing");
        assert!(
            files.keys().any(|k| k.starts_with("graph/")),
            "no graph/ files"
        );
        assert!(
            files.keys().any(|k| k.starts_with("embeddings/")),
            "no embeddings/ files"
        );
    }

    // ── Lance extension ───────────────────────────────────────────────────────

    #[test]
    fn pack_with_lance_succeeds() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let lance = tmp.path().join("lance");
        make_input(&input);
        make_lance_dir(&lance);
        let output = tmp.path().join("out.sembundle");
        let mut opts = default_opts(input, output.clone());
        opts.lance_dir = Some(lance);
        assert!(pack(opts).is_ok());
        assert!(output.exists());
    }

    #[test]
    fn lance_files_present_in_archive() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let lance = tmp.path().join("lance");
        make_input(&input);
        make_lance_dir(&lance);
        let output = tmp.path().join("out.sembundle");
        let mut opts = default_opts(input, output.clone());
        opts.lance_dir = Some(lance);
        pack(opts).unwrap();

        let (_, files) = extract_bundle(&output);
        assert!(files.contains_key("lance/metadata.json"), "lance/metadata.json missing");
        assert!(
            files.keys().any(|k| k.starts_with("lance/docs.lance/")),
            "no lance/docs.lance/ files"
        );
    }

    #[test]
    fn lance_extension_declared_in_manifest() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let lance = tmp.path().join("lance");
        make_input(&input);
        make_lance_dir(&lance);
        let output = tmp.path().join("out.sembundle");
        let mut opts = default_opts(input, output.clone());
        opts.lance_dir = Some(lance);
        pack(opts).unwrap();

        let (manifest_bytes, _) = extract_bundle(&output);
        let m: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
        assert!(m.extensions.iter().any(|e| e == "lance"), "\"lance\" not in extensions");
    }

    #[test]
    fn no_lance_extension_without_lance_dir() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        make_input(&input);
        let output = tmp.path().join("out.sembundle");
        pack(default_opts(input, output.clone())).unwrap();

        let (manifest_bytes, _) = extract_bundle(&output);
        let m: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
        assert!(!m.extensions.iter().any(|e| e == "lance"), "\"lance\" should not be in extensions");
    }

    #[test]
    fn lance_metadata_created_at_stamped() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let lance = tmp.path().join("lance");
        make_input(&input);
        make_lance_dir(&lance);
        let output = tmp.path().join("out.sembundle");
        let mut opts = default_opts(input, output.clone());
        opts.lance_dir = Some(lance);
        pack(opts).unwrap();

        let (manifest_bytes, files) = extract_bundle(&output);
        let m: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
        let lance_meta: serde_json::Value =
            serde_json::from_slice(files.get("lance/metadata.json").unwrap()).unwrap();
        // created_at in lance/metadata.json must match manifest created_at (spec §11.5)
        assert_eq!(lance_meta["created_at"], m.created_at);
        assert_ne!(lance_meta["created_at"], "1970-01-01T00:00:00Z");
    }

    #[test]
    fn lance_checksums_include_all_lance_files() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let lance = tmp.path().join("lance");
        make_input(&input);
        make_lance_dir(&lance);
        let output = tmp.path().join("out.sembundle");
        let mut opts = default_opts(input, output.clone());
        opts.lance_dir = Some(lance);
        pack(opts).unwrap();

        let (manifest_bytes, files) = extract_bundle(&output);
        let m: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
        for key in files.keys().filter(|k| k.starts_with("lance/")) {
            assert!(
                m.checksums.contains_key(key),
                "lance file '{key}' missing from checksums"
            );
        }
    }
}
