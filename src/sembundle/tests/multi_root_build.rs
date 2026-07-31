//! Issue #79 acceptance test at the `sembundle build` level: multiple
//! `-s`/`--source-dir` roots — including a same-basename pair — must all land in
//! ONE graph in the produced bundle, not silently overwrite each other.
//!
//! This drives the whole public `build::build` pipeline (native `semgraph`
//! indexing → pack) and then reads the packed artifact back, so it fails if the
//! multi-root fix regresses anywhere between indexing and packing.

use std::fs;
use std::path::Path;

use sembundle::build::{build, native_codegraph_version, BuildOptions};
use sembundle::read_manifest;
use sembundle::reader::read_entries;
use tempfile::TempDir;

/// Write a one-function Python module at `dir/mod.py`.
fn write_module(dir: &Path, func: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("mod.py"),
        format!("def {func}():\n    return \"{func}\"\n"),
    )
    .unwrap();
}

#[test]
fn build_with_two_same_basename_source_dirs_keeps_both_roots() {
    let work = TempDir::new().unwrap();
    // Same basename ("shared") under different parents — the #79 collision.
    let alpha = work.path().join("alpha").join("shared");
    let beta = work.path().join("beta").join("shared");
    write_module(&alpha, "alpha_only");
    write_module(&beta, "beta_only");

    let out = TempDir::new().unwrap();
    let bundle_path = out.path().join("multi-root-1.0.0.sembundle");

    let produced = build(BuildOptions {
        name: "multi-root".to_string(),
        version: "1.0.0".to_string(),
        source_repo: "https://github.com/example/multi-root".to_string(),
        commit_hash: "a".repeat(40),
        tag: None,
        language: "python".to_string(),
        codegraph_version: native_codegraph_version(),
        output_path: Some(bundle_path.clone()),
        source_dirs: vec![alpha, beta],
        docs_dirs: vec![],
        docs_glob: None,
        include_source: true,
        source_glob: None,
        exclude_dirs: vec![],
    })
    .expect("build should succeed with no CodeGraph installed");
    assert_eq!(produced, bundle_path);

    let bytes = fs::read(&bundle_path).unwrap();

    // Manifest records the native indexer version (task #3), not a CodeGraph one.
    let manifest = read_manifest(&bytes).unwrap();
    assert!(
        manifest.codegraph_version.starts_with("sempkg-native/"),
        "codegraph_version = {}",
        manifest.codegraph_version
    );

    // Pull the packed graph db out of the archive and open it natively.
    let entries = read_entries(&bytes).unwrap();
    let (_, db_bytes) = entries
        .iter()
        .find(|(name, _)| name.replace('\\', "/").ends_with("graph/codegraph.db"))
        .expect("bundle must contain graph/codegraph.db");

    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("codegraph.db");
    fs::write(&db_path, db_bytes).unwrap();

    let db = semgraph::GraphDb::open(&db_path).unwrap();
    let files = db.file_paths().unwrap();
    // Both roots survive, namespaced apart (pre-fix: only the last -s dir).
    assert!(
        files.iter().any(|f| f == "alpha/shared/mod.py"),
        "alpha root dropped from bundle: {files:?}"
    );
    assert!(
        files.iter().any(|f| f == "beta/shared/mod.py"),
        "beta root dropped from bundle: {files:?}"
    );
    assert!(
        !db.query("alpha_only", None, 10).unwrap().is_empty(),
        "alpha_only symbol missing"
    );
    assert!(
        !db.query("beta_only", None, 10).unwrap().is_empty(),
        "beta_only symbol missing"
    );
}
